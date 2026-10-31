//! 参考媒体适配 `sb.core.fec-xor`（L-B）：`fec-shards` profile 的可运行最小实现。
//!
//! 原理出处：Moonlight GameStream 的「帧→分片→FEC→丢帧」模型（原理卡片
//! docs/cards/moonlight-gamestream.md）。**简化声明**：Moonlight 原版是
//! Reed-Solomon + 主机逐帧动态 FEC 比例（≤200%）；本模块用固定 XOR 单校验
//! （组内 8 数据片 + 1 校验片，overhead 12.5%）做规范验证，RS 见 v0.2 路线。
//!
//! wire 头（15B）：`frame_id:u32 | orig_len:u32 | shard_size:u16 | total:u16 | idx:u16 | type:u8`
//! type：0=数据片（idx=片序号），1=校验片（idx=组号）。
//!
//! 评审修订（2026-10-24，流媒体大师 round）：
//! - idx/total 升 u16（u8 时 1024B 分片下帧上限 255KB，4K IDR 必溢出）；
//! - 丢帧判定保留当前帧首片（先入 pending 再报 Lost，不再静默丢弃新帧）；
//! - WouldBlock（队满）≠ 关闭：整帧放弃并置 loss 标记（01 §8），不误报 Closed；
//! - 1 帧乱序容忍窗（parity 晚到不再立即判死）；
//! - shard_size 与 Channel MTU 联动钳制（01 §4.1 第 6 条 MUST）。

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::channel::{Channel, SendError};
use crate::stages::{
    EncodedFrame, FrameIn, FrameSink, FrameSource, MediaAdapt, MediaError, QueueDepth,
};
use crate::telemetry::EventBus;

pub const SHARD_SIZE: usize = 1024;
pub const GROUP_SIZE: usize = 8;
/// 乱序容忍窗（帧）：fid 差超过该值才宣布旧帧丢失。
const REORDER_WINDOW: u32 = 1;
pub const HEADER: usize = 15;
const TYPE_DATA: u8 = 0;
const TYPE_PARITY: u8 = 1;

#[derive(Clone, Copy, Debug)]
pub struct FecConfig {
    /// 期望分片大小；bind 时按 Channel datagram 上限钳制（含 28B IP/UDP 开销预留）。
    pub shard_size: usize,
    pub group_size: usize,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self {
            shard_size: SHARD_SIZE,
            group_size: GROUP_SIZE,
        }
    }
}

struct Counters {
    frames_tx: Arc<AtomicU32>,
    frames_rx: Arc<AtomicU32>,
    frames_recovered: Arc<AtomicU32>,
    frames_lost: Arc<AtomicU32>,
}

/// Moonlight 型分片 FEC 适配器。
pub struct FecShards {
    cfg: FecConfig,
    counters: Arc<Counters>,
}

impl FecShards {
    pub fn new(cfg: FecConfig) -> Self {
        Self {
            cfg,
            counters: Arc::new(Counters {
                frames_tx: Arc::new(AtomicU32::new(0)),
                frames_rx: Arc::new(AtomicU32::new(0)),
                frames_recovered: Arc::new(AtomicU32::new(0)),
                frames_lost: Arc::new(AtomicU32::new(0)),
            }),
        }
    }
}

#[async_trait]
impl MediaAdapt for FecShards {
    fn profile(&self) -> &'static str {
        "fec-shards"
    }

    async fn bind(
        &self,
        tx_side: Arc<dyn Channel>,
        rx_side: Arc<dyn Channel>,
        bus: Arc<EventBus>,
    ) -> anyhow::Result<(Box<dyn FrameSink>, Box<dyn FrameSource>)> {
        // 01 §4.1-6：分片头 + payload + 28(IP/UDP) 必须 ≤ 信道 datagram 上限
        let overhead = HEADER + 28;
        let max_payload = tx_side
            .caps()
            .datagram
            .as_ref()
            .map(|d| d.max_size as usize)
            .unwrap_or(SHARD_SIZE);
        let shard = self
            .cfg
            .shard_size
            .min(max_payload.saturating_sub(overhead))
            .max(256);
        if shard != self.cfg.shard_size {
            bus.emit_event(
                "shard_size_clamped",
                serde_json::json!({ "requested": self.cfg.shard_size, "effective": shard }),
            );
        }
        let tx = FecTx {
            ch: tx_side,
            shard,
            group_size: self.cfg.group_size,
            bus: Arc::clone(&bus),
            frames: Arc::clone(&self.counters.frames_tx),
            lost: Arc::clone(&self.counters.frames_lost),
        };
        let rx = FecRx {
            ch: rx_side,
            group_size: self.cfg.group_size,
            bus,
            counters: Arc::clone(&self.counters),
            pending: HashMap::new(),
            last_completed: None,
            outbox: VecDeque::new(),
        };
        Ok((Box::new(tx), Box::new(rx)))
    }
}

struct FecTx {
    ch: Arc<dyn Channel>,
    shard: usize,
    group_size: usize,
    bus: Arc<EventBus>,
    frames: Arc<AtomicU32>,
    lost: Arc<AtomicU32>,
}

fn put_hdr(
    buf: &mut BytesMut,
    frame_id: u32,
    orig_len: u32,
    shard_size: u16,
    total: u16,
    idx: u16,
    ty: u8,
) {
    buf.put_u32(frame_id);
    buf.put_u32(orig_len);
    buf.put_u16(shard_size);
    buf.put_u16(total);
    buf.put_u16(idx);
    buf.put_u8(ty);
}

#[async_trait]
impl FrameSink for FecTx {
    async fn submit(&mut self, frame: EncodedFrame) -> Result<QueueDepth, MediaError> {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let s = self.shard;
        let total = (frame.data.len().div_ceil(s).max(1)) as u16;
        // 发送一个分片；队满（WouldBlock）≠ 关闭：按规范放弃整帧并置 loss 标记
        macro_rules! send {
            ($pkt:expr) => {
                match self.ch.send_dgram($pkt).await {
                    Ok(()) => {}
                    Err(SendError::WouldBlock) => {
                        self.lost.fetch_add(1, Ordering::Relaxed);
                        self.bus.emit_event(
                            "frame_drop",
                            serde_json::json!({
                                "frame_id": frame.frame_id, "reason": "queue_full",
                                "stream": "video", "abandoned": true
                            }),
                        );
                        return Ok(QueueDepth { queued_frames: 0 });
                    }
                    Err(_) => return Err(MediaError::Closed),
                }
            };
        }
        for g in 0..(total as usize).div_ceil(self.group_size) {
            let g = g as u16;
            let start = g as usize * self.group_size;
            let end = ((g as usize + 1) * self.group_size).min(total as usize);
            let mut parity = vec![0u8; s];
            for idx in start..end {
                let base = idx * s;
                let mut shard_buf = vec![0u8; s];
                let src =
                    &frame.data[base.min(frame.data.len())..((base + s).min(frame.data.len()))];
                shard_buf[..src.len()].copy_from_slice(src);
                for (p, v) in parity.iter_mut().zip(shard_buf.iter()) {
                    *p ^= v;
                }
                let mut pkt = BytesMut::with_capacity(HEADER + s);
                put_hdr(
                    &mut pkt,
                    frame.frame_id,
                    frame.data.len() as u32,
                    s as u16,
                    total,
                    idx as u16,
                    TYPE_DATA,
                );
                pkt.extend_from_slice(&shard_buf);
                send!(pkt.freeze());
            }
            let mut pkt = BytesMut::with_capacity(HEADER + s);
            put_hdr(
                &mut pkt,
                frame.frame_id,
                frame.data.len() as u32,
                s as u16,
                total,
                g,
                TYPE_PARITY,
            );
            pkt.extend_from_slice(&parity);
            send!(pkt.freeze());
        }
        Ok(QueueDepth { queued_frames: 0 })
    }
}

struct Reasm {
    orig_len: u32,
    total_data: u16,
    shard_size: usize,
    data: HashMap<u16, Vec<u8>>,
    /// 组号 → parity 片
    parity: HashMap<u16, Vec<u8>>,
}

struct FecRx {
    ch: Arc<dyn Channel>,
    group_size: usize,
    bus: Arc<EventBus>,
    counters: Arc<Counters>,
    pending: HashMap<u32, Reasm>,
    /// 已完成的最大帧号（None=尚未完成任何帧）：滞后包直接丢弃，避免幽灵重组态
    last_completed: Option<u32>,
    /// 待交付产出（一个包可能同时产生 Lost + Complete，必须按序都交付）
    outbox: VecDeque<FrameIn>,
}

impl FecRx {
    /// 完整则重组并返回帧。
    fn assemble(&mut self, fid: u32) -> Option<FrameIn> {
        let r = self.pending.remove(&fid)?;
        if (r.data.len() as u16) < r.total_data {
            self.pending.insert(fid, r);
            return None;
        }
        let mut out = vec![0u8; r.orig_len as usize];
        for (i, sh) in &r.data {
            let base = *i as usize * r.shard_size;
            if base >= out.len() {
                continue;
            }
            let n = (base + r.shard_size).min(out.len()) - base;
            out[base..base + n].copy_from_slice(&sh[..n]);
        }
        self.counters.frames_rx.fetch_add(1, Ordering::Relaxed);
        if self.last_completed.is_none_or(|lc| fid > lc) {
            self.last_completed = Some(fid);
        }
        Some(FrameIn::Complete {
            frame_id: fid,
            data: Bytes::from(out),
        })
    }
}

/// 尝试组内恢复：缺恰好 1 个数据片且有 parity → XOR 重建（自由函数，避免 self 双重借用）。
fn try_recover(
    group_size: usize,
    counters: &Arc<Counters>,
    bus: &Arc<EventBus>,
    fid: u32,
    r: &mut Reasm,
) {
    for g in 0..r.total_data.div_ceil(group_size as u16) {
        let start = g as usize * group_size;
        let end = ((g as usize + 1) * group_size).min(r.total_data as usize);
        let missing: Vec<u16> = (start..end)
            .map(|i| i as u16)
            .filter(|i| !r.data.contains_key(i))
            .collect();
        if missing.len() == 1 && r.parity.contains_key(&g) {
            let mut rec = r.parity[&g].clone();
            for i in start..end {
                let i = i as u16;
                if Some(&i) != missing.first() {
                    if let Some(sh) = r.data.get(&i) {
                        for (p, v) in rec.iter_mut().zip(sh.iter()) {
                            *p ^= v;
                        }
                    }
                }
            }
            r.data.insert(missing[0], rec);
            counters.frames_recovered.fetch_add(1, Ordering::Relaxed);
            bus.emit_event(
                "fec_recovered",
                serde_json::json!({ "frame_id": fid, "shard": missing[0] }),
            );
        }
    }
}

#[async_trait]
impl FrameSource for FecRx {
    async fn next(&mut self) -> Option<FrameIn> {
        if let Some(f) = self.outbox.pop_front() {
            return Some(f);
        }
        loop {
            let pkt = self.ch.recv_dgram().await.ok()?;
            if pkt.len() < HEADER {
                continue;
            }
            let fid = u32::from_be_bytes(pkt[0..4].try_into().unwrap());
            let orig_len = u32::from_be_bytes(pkt[4..8].try_into().unwrap());
            let ss = u16::from_be_bytes(pkt[8..10].try_into().unwrap()) as usize;
            let total = u16::from_be_bytes(pkt[10..12].try_into().unwrap());
            let idx = u16::from_be_bytes(pkt[12..14].try_into().unwrap());
            let ty = pkt[14];
            let parity = ty == TYPE_PARITY;
            let payload = &pkt[HEADER..];

            // 滞后包跳过（帧尾 parity 常在帧完成后才被读到）
            if matches!(self.last_completed, Some(lc) if fid <= lc) {
                continue;
            }

            // 当前帧先入 pending（评审修订：报旧帧丢失不得静默丢弃新帧首片）
            let r = self.pending.entry(fid).or_insert(Reasm {
                orig_len,
                total_data: total,
                shard_size: ss,
                data: HashMap::new(),
                parity: HashMap::new(),
            });
            if parity {
                r.parity.insert(idx, payload.to_vec());
            } else {
                r.data.insert(idx, payload.to_vec());
            }
            try_recover(self.group_size, &self.counters, &self.bus, fid, r);
            let assembled = self.assemble(fid);

            // 乱序窗外的旧帧先报 Lost（评审修订：不得因 assemble 提前返回而漏报）；
            // 同包触发的 Complete 进 outbox，保证两个产出都不丢（评审修订后修复）
            let stale_bound = fid.saturating_sub(REORDER_WINDOW);
            let stale = self
                .pending
                .keys()
                .copied()
                .filter(|k| *k < stale_bound)
                .min();
            if let Some(stale) = stale {
                let r = self.pending.remove(&stale).unwrap();
                self.counters.frames_lost.fetch_add(1, Ordering::Relaxed);
                self.bus.emit_event(
                    "frame_lost",
                    serde_json::json!({ "frame_id": stale, "had": r.data.len(), "of": r.total_data }),
                );
                self.outbox.push_back(FrameIn::Lost { frame_id: stale });
            }
            if let Some(frame) = assembled {
                self.outbox.push_back(frame);
            }
            if let Some(f) = self.outbox.pop_front() {
                return Some(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelError, Stream, StreamKind};
    use crate::modules::loopback::LoopbackPairTransport;
    use crate::stages::Transport;
    use crate::telemetry::StageStats;
    use std::time::Instant;

    async fn bind() -> (Box<dyn FrameSink>, Box<dyn FrameSource>, Arc<EventBus>) {
        let t = LoopbackPairTransport::new(1200);
        let a: Arc<dyn Channel> = Arc::from(
            t.establish(Vec::new(), Arc::new(EventBus::new()))
                .await
                .unwrap(),
        );
        let b: Arc<dyn Channel> = Arc::from(t.take_remote().await.unwrap());
        let fec = FecShards::new(FecConfig::default());
        let bus = Arc::new(EventBus::new());
        let (tx, rx) = fec.bind(Arc::clone(&a), b, Arc::clone(&bus)).await.unwrap();
        (tx, rx, bus)
    }

    #[tokio::test]
    async fn 无损信道_多帧完整往返() {
        let (mut tx, mut rx, _bus) = bind().await;
        for i in 0..5u32 {
            let data: Vec<u8> = (0..(1024 * 3 + 123)).map(|x| (x as u8) ^ i as u8).collect();
            tx.submit(EncodedFrame {
                frame_id: i,
                data: Bytes::from(data.clone()),
            })
            .await
            .unwrap();
            match rx.next().await.unwrap() {
                FrameIn::Complete {
                    frame_id,
                    data: got,
                } => {
                    assert_eq!(frame_id, i);
                    assert_eq!(&got[..], &data[..]);
                }
                other => panic!("期望 Complete，得到 {other:?}"),
            }
        }
    }

    /// 丢第 n 个出站数据报的包装器。
    struct DropNth {
        inner: Arc<dyn Channel>,
        n: usize,
        seen: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Channel for DropNth {
        fn caps(&self) -> &crate::caps::ChannelCaps {
            self.inner.caps()
        }
        fn conn_id(&self) -> &str {
            self.inner.conn_id()
        }
        async fn send_dgram(&self, b: Bytes) -> Result<(), crate::channel::SendError> {
            let k = self.seen.fetch_add(1, Ordering::SeqCst);
            if k == self.n {
                return Ok(()); // 静默丢弃
            }
            self.inner.send_dgram(b).await
        }
        async fn recv_dgram(&self) -> Result<Bytes, crate::channel::RecvError> {
            self.inner.recv_dgram().await
        }
        async fn open_stream(&self, k: StreamKind) -> Result<Box<dyn Stream>, ChannelError> {
            self.inner.open_stream(k).await
        }
        async fn accept_stream(&self) -> Result<(StreamKind, Box<dyn Stream>), ChannelError> {
            self.inner.accept_stream().await
        }
        fn stats(&self) -> StageStats {
            self.inner.stats()
        }
        fn close(&self) {
            self.inner.close();
        }
        fn is_closed(&self) -> bool {
            self.inner.is_closed()
        }
    }

    async fn bind_with_tx_drop(n: usize) -> (Box<dyn FrameSink>, Box<dyn FrameSource>) {
        let t = LoopbackPairTransport::new(1200);
        let a_raw: Arc<dyn Channel> = Arc::from(
            t.establish(Vec::new(), Arc::new(EventBus::new()))
                .await
                .unwrap(),
        );
        let b: Arc<dyn Channel> = Arc::from(t.take_remote().await.unwrap());
        let a: Arc<dyn Channel> = Arc::new(DropNth {
            inner: a_raw,
            n,
            seen: Default::default(),
        });
        let fec = FecShards::new(FecConfig::default());
        fec.bind(a, b, Arc::new(EventBus::new())).await.unwrap()
    }

    #[tokio::test]
    async fn 组内丢1片_xor恢复() {
        // 帧 5 片 + 1 parity：丢 idx=2（第 3 个数据报）→ parity 重建
        let (mut tx, mut rx) = bind_with_tx_drop(2).await;
        let data: Vec<u8> = (0..(1024 * 5)).map(|x| x as u8).collect();
        tx.submit(EncodedFrame {
            frame_id: 7,
            data: Bytes::from(data.clone()),
        })
        .await
        .unwrap();
        match rx.next().await.unwrap() {
            FrameIn::Complete {
                frame_id,
                data: got,
            } => {
                assert_eq!(frame_id, 7);
                assert_eq!(&got[..], &data[..], "XOR 恢复后内容必须一致");
            }
            other => panic!("期望恢复后 Complete，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn 组内丢2片_乱序窗后报丢失且不吞新帧() {
        // 帧1 丢 2 片不可恢复；乱序窗=1：帧2 先完成（Complete），帧3 首片到达时
        // 才宣布 Lost(1)；随后帧3 完成——验证"报丢失不吞新帧"
        let t = LoopbackPairTransport::new(1200);
        let a_raw: Arc<dyn Channel> = Arc::from(
            t.establish(Vec::new(), Arc::new(EventBus::new()))
                .await
                .unwrap(),
        );
        let b: Arc<dyn Channel> = Arc::from(t.take_remote().await.unwrap());
        let a: Arc<dyn Channel> = Arc::new(DropNth {
            inner: a_raw,
            n: 0,
            seen: Default::default(),
        });
        let a2: Arc<dyn Channel> = Arc::new(DropNth {
            inner: a,
            n: 0,
            seen: Default::default(),
        });
        let fec = FecShards::new(FecConfig::default());
        let (mut tx, mut rx) = fec.bind(a2, b, Arc::new(EventBus::new())).await.unwrap();

        tx.submit(EncodedFrame {
            frame_id: 1,
            data: Bytes::from(vec![7u8; 1024 * 3]),
        })
        .await
        .unwrap();
        tx.submit(EncodedFrame {
            frame_id: 2,
            data: Bytes::from(vec![9u8; 100]),
        })
        .await
        .unwrap();
        tx.submit(EncodedFrame {
            frame_id: 3,
            data: Bytes::from(vec![5u8; 200]),
        })
        .await
        .unwrap();
        match rx.next().await.unwrap() {
            FrameIn::Complete { frame_id, .. } => assert_eq!(frame_id, 2, "帧2 应先完成"),
            other => panic!("期望 Complete(2)，得到 {other:?}"),
        }
        match rx.next().await.unwrap() {
            FrameIn::Lost { frame_id } => assert_eq!(frame_id, 1, "乱序窗外才宣布丢失"),
            other => panic!("期望 Lost(1)，得到 {other:?}"),
        }
        match rx.next().await.unwrap() {
            FrameIn::Complete { frame_id, .. } => assert_eq!(frame_id, 3),
            other => panic!("期望 Complete(3)，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn 大帧_u16分片不溢出() {
        // 300KB 帧 → 293 片（u8 会溢出，验证 u16 正确）；
        // 大容量队列 + 接收侧并发排空，避免触发队满弃帧
        let t = LoopbackPairTransport::with_capacity(1200, 1024);
        let a: Arc<dyn Channel> = Arc::from(
            t.establish(Vec::new(), Arc::new(EventBus::new()))
                .await
                .unwrap(),
        );
        let b: Arc<dyn Channel> = Arc::from(t.take_remote().await.unwrap());
        let fec = FecShards::new(FecConfig::default());
        let bus = Arc::new(EventBus::new());
        let (mut tx, rx) = fec.bind(Arc::clone(&a), b, Arc::clone(&bus)).await.unwrap();

        let reader = tokio::spawn(async move {
            let mut rx = rx;
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while let Ok(Some(fin)) = tokio::time::timeout_at(deadline.into(), rx.next()).await {
                if let FrameIn::Complete { frame_id, data } = fin {
                    if frame_id == 100 {
                        return Some((frame_id, data));
                    }
                }
            }
            None
        });
        let data: Vec<u8> = (0..(1024 * 293)).map(|x| (x % 251) as u8).collect();
        tx.submit(EncodedFrame {
            frame_id: 100,
            data: Bytes::from(data.clone()),
        })
        .await
        .unwrap();
        let got = reader.await.unwrap().expect("应收到完整大帧");
        assert_eq!(got.0, 100);
        assert_eq!(&got.1[..], &data[..]);
    }
}
