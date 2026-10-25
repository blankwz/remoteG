//! 参考信道仿真 `sb.core.lossy-sim`（L-B）：对任意 Channel 施加 rtt/丢包/带宽/MTU 限制。
//!
//! 用途：SB-Bench T0 剖面（04 §1）——CI 可复现的弱网环境。
//! 诚实性约束：`path.kind` 透传内层真实值（06 §1 谎报一票否决同样适用于仿真层）。

use async_trait::async_trait;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::caps::ChannelCaps;
use crate::channel::{Channel, RecvError, SendError, Stream, StreamKind, ChannelError};
use crate::stages::Transport;
use crate::telemetry::{EventBus, StageStats};
use crate::types::PathHandle;

/// 信道参数（T3 剖面默认：80ms / 2% / 10Mbps，规范 04 §1）。
#[derive(Clone, Copy, Debug)]
pub struct LossyParams {
    pub rtt_ms: f32,
    pub jitter_ms: f32,
    pub loss_ratio: f32,
    pub mbps: f32,
    pub mtu: u16,
    /// 突发长度：一次丢弃后连带丢弃的后续包数（Gilbert-Elliott 简化，评审修订 #5）
    pub burst_len: u32,
}

impl Default for LossyParams {
    fn default() -> Self {
        Self { rtt_ms: 80.0, jitter_ms: 5.0, loss_ratio: 0.02, mbps: 10.0, mtu: 1200, burst_len: 0 }
    }
}

/// 确定性伪随机（xorshift64，bench 可复现要求 04 §6）。
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 1_000_000) as f32 / 1_000_000.0
    }
}

/// 运行时可调的丢包率（W6 弱网注入用）。
#[derive(Default)]
pub struct LossOverride(AtomicU64); // f32 bits

impl LossOverride {
    pub fn set(&self, ratio: f32) {
        self.0.store(ratio.to_bits() as u64, Ordering::Relaxed);
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed) as u32)
    }
}

struct Shaper {
    params: LossyParams,
    loss_override: Arc<LossOverride>,
    rng: tokio::sync::Mutex<(XorShift, u32)>, // (随机源, 剩余突发)
    injected_lost: AtomicU64,
    forwarded: AtomicU64,
}

pub struct LossyChannel {
    inner: Arc<dyn Channel>,
    shaper: Arc<Shaper>,
    caps: ChannelCaps,
}

impl LossyChannel {
    fn stats_of_inner(&self, inner: StageStats) -> StageStats {
        let mut s = inner;
        s.module = "sb.core.lossy-sim".into();
        s.loss_ratio = self.shaper.injected_lost.load(Ordering::Relaxed) as f32
            / (self.shaper.forwarded.load(Ordering::Relaxed) as f32
                + self.shaper.injected_lost.load(Ordering::Relaxed) as f32
                + 1.0);
        s
    }
}

#[async_trait]
impl Channel for LossyChannel {
    fn caps(&self) -> &ChannelCaps {
        &self.caps
    }

    fn conn_id(&self) -> &str {
        self.inner.conn_id()
    }

    // 评审修订：异步整形（fire-and-forget）——延迟/带宽/丢包在独立任务里生效，
    // 发送调用立即返回（真实网卡语义：传输流水线化，不串行化在调用栈上）。
    async fn send_dgram(&self, b: Bytes) -> Result<(), SendError> {
        let mut rng = self.shaper.rng.lock().await;
        let bw_ms = b.len() as f32 * 8.0 / (self.shaper.params.mbps * 1e6) * 1000.0;
        let delay = bw_ms + self.shaper.params.rtt_ms + rng.0.next_f32() * self.shaper.params.jitter_ms;
        let roll = rng.0.next_f32();
        let (dropped, burst_left) = if rng.1 > 0 {
            (true, rng.1 - 1)
        } else {
            let loss = self.shaper.loss_override.get().max(self.shaper.params.loss_ratio);
            if roll < loss && self.shaper.params.burst_len > 0 {
                (true, self.shaper.params.burst_len - 1)
            } else {
                (roll < loss, 0)
            }
        };
        rng.1 = burst_left;
        drop(rng);
        let shaper = Arc::clone(&self.shaper);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
            if dropped {
                shaper.injected_lost.fetch_add(1, Ordering::Relaxed);
                return; // 静默丢弃，发送方视角"已发出"
            }
            // 转发在内层队列满时表现为应用层丢弃（对端不可见）
            match inner.send_dgram(b).await {
                Ok(()) => {
                    shaper.forwarded.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    shaper.injected_lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        Ok(())
    }

    async fn recv_dgram(&self) -> Result<Bytes, RecvError> {
        // 时延已在发送路径施加；接收侧直通（避免二次串行化）
        self.inner.recv_dgram().await
    }

    async fn open_stream(&self, kind: StreamKind) -> Result<Box<dyn Stream>, ChannelError> {
        self.inner.open_stream(kind).await
    }

    async fn accept_stream(&self) -> Result<(StreamKind, Box<dyn Stream>), ChannelError> {
        self.inner.accept_stream().await
    }

    fn stats(&self) -> StageStats {
        self.stats_of_inner(self.inner.stats())
    }

    fn close(&self) {
        self.inner.close();
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

/// 把任意 Channel 包上弱网仿真。
pub fn shape(
    ch: Box<dyn Channel>,
    params: LossyParams,
    loss_override: Arc<LossOverride>,
    seed: u64,
) -> Box<dyn Channel> {
    let mut caps = ch.caps().clone();
    caps.mtu = caps.mtu.min(params.mtu);
    if let Some(d) = &mut caps.datagram {
        d.max_size = d.max_size.min(params.mtu.saturating_sub(16));
    }
    Box::new(LossyChannel {
        inner: Arc::from(ch),
        shaper: Arc::new(Shaper {
            params,
            loss_override,
            rng: tokio::sync::Mutex::new((XorShift::new(seed), 0)),
            injected_lost: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
        }),
        caps,
    })
}

/// Transport 装饰器：establish 后包一层仿真（A 端）。
pub struct LossySimTransport {
    pub inner: Arc<dyn Transport>,
    pub params: LossyParams,
    pub loss_override: Arc<LossOverride>,
    pub seed: u64,
}

#[async_trait]
impl Transport for LossySimTransport {
    fn offers(&self) -> &ChannelCaps {
        // offering 以内层为准（协商按更弱一侧；mtu 由 shape 时收敛）
        self.inner.offers()
    }

    async fn establish(
        &self,
        paths: Vec<PathHandle>,
        bus: Arc<EventBus>,
    ) -> anyhow::Result<Box<dyn Channel>> {
        let ch = self.inner.establish(paths, bus).await?;
        Ok(shape(ch, self.params, Arc::clone(&self.loss_override), self.seed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::loopback::LoopbackPairTransport;

    #[tokio::test]
    async fn 无损参数下数据报可达_丢包率下被仿真丢弃() {
        let lb = Arc::new(LoopbackPairTransport::new(1200));
        let ov = Arc::new(LossOverride::default());
        let t = LossySimTransport {
            inner: Arc::clone(&lb) as Arc<dyn crate::stages::Transport>,
            params: LossyParams { rtt_ms: 1.0, jitter_ms: 0.0, loss_ratio: 0.0, mbps: 100.0, mtu: 1200, burst_len: 0 },
            loss_override: Arc::clone(&ov),
            seed: 42,
        };
        let a: Box<dyn Channel> = t.establish(Vec::new(), Arc::new(EventBus::new())).await.unwrap();
        let raw_b = lb.take_remote().await.unwrap();
        let b = shape(Box::new(raw_b), LossyParams { rtt_ms: 1.0, jitter_ms: 0.0, loss_ratio: 0.0, mbps: 100.0, mtu: 1200, burst_len: 0 }, Arc::clone(&ov), 43);

        a.send_dgram(Bytes::from_static(b"x")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await; // 等整形任务落地
        let got = b.recv_dgram().await.unwrap();
        assert_eq!(&got[..], b"x");

        // 100% 丢包注入：发送侧静默成功（仿真在发送路径丢弃），对端收不到任何包
        ov.set(1.0);
        for _ in 0..5 {
            a.send_dgram(Bytes::from_static(b"y")).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let s = a.stats();
        assert!(s.loss_ratio > 0.5, "注入丢失应体现在遥测 loss_ratio，实际 {}", s.loss_ratio);
        assert_eq!(s.module, "sb.core.lossy-sim");
    }

    #[tokio::test]
    async fn 遥测路径不谎报() {
        let lb = Arc::new(LoopbackPairTransport::new(1200));
        let ov = Arc::new(LossOverride::default());
        let t = LossySimTransport {
            inner: Arc::clone(&lb) as Arc<dyn crate::stages::Transport>,
            params: LossyParams::default(),
            loss_override: ov,
            seed: 7,
        };
        let a: Box<dyn Channel> = t.establish(Vec::new(), Arc::new(EventBus::new())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        // path.kind 透传内层（loopback），模块名标注为 lossy-sim
        let s = a.stats();
        assert_eq!(s.module, "sb.core.lossy-sim");
        assert_eq!(s.path.kind, crate::types::PathKind::Loopback);
    }
}
