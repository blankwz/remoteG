//! 参考媒体适配 `sb.moonlight.fec-rs`（L-B）：`fec-shards` profile 的 Moonlight 级实现。
//!
//! 与卡片 docs/cards/moonlight-gamestream.md 对齐的三个关键行为：
//! 1. **Reed-Solomon**（GF(256)，reed-solomon-erasure crate）——非 XOR；
//! 2. **动态 FEC 比例**：parity = ⌈data × ratio⌉，ratio 可配 [0, 2.0]（Moonlight
//!    主机逐帧下放 fecInfo 的等价物；本模块为会话级常量，逐帧自适应见 v0.2）；
//! 3. **单片小帧抬到 200%**：k==1 时 parity 取 `min_parity_single`（默认 2，
//!    Moonlight minRequiredFecPackets=2 同款，SdpGenerator.c:207-211）。
//!
//! wire 头（17B）：`frame_id:u32 | orig_len:u32 | shard_size:u16 | k:u16 | idx:u16 | m:u8 | pidx:u8 | type:u8`
//! type：0=数据片（idx=块内片序号），1=校验片（idx=块号，pidx=块内校验序号）。
//! 大帧自动切多块（每块 ≤ MAX_BLOCK_DATA 片，规避 GF(256) 的 k+m ≤ 255 限制）。

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use switchboard_core::channel::{Channel, SendError};
use switchboard_core::stages::{
    EncodedFrame, FrameIn, FrameSink, FrameSource, MediaAdapt, MediaError, QueueDepth,
};
use switchboard_core::telemetry::EventBus;

pub const SHARD_SIZE: usize = 1024;
/// GF(256)：k + m ≤ 255，留余量。
pub const MAX_BLOCK_DATA: usize = 128;
/// 乱序容忍窗（帧）。
pub const HEADER: usize = 17;
/// parity = ⌈k × ratio⌉，k==1 抬到 min_parity_single；上限 255-k。
fn parity_count(cfg: FecRsConfig, k: usize) -> usize {
    if k == 1 {
        return (cfg.min_parity_single as usize).min(254);
    }
    let r = (k as f32 * cfg.overhead_ratio).ceil() as usize;
    r.clamp(1, 255 - k.min(255))
}
const TYPE_DATA: u8 = 0;
const TYPE_PARITY: u8 = 1;

#[derive(Clone, Copy, Debug)]
pub struct FecRsConfig {
    pub shard_size: usize,
    /// FEC 比例：parity = ⌈k × ratio⌉（Moonlight 动态 fecInfo 的会话级等价）。
    pub overhead_ratio: f32,
    /// k==1 时的最小校验片数（Moonlight minRequiredFecPackets）。
    pub min_parity_single: u16,
    /// 乱序容忍窗（帧）：fid 差超过该值才判旧帧丢失（审查 #7：可配，默认 1）。
    pub reorder_window: u32,
    /// 同时在重组中的帧数上限（超限丢最旧，防内存膨胀）。
    pub max_pending: usize,
}

impl Default for FecRsConfig {
    fn default() -> Self {
        Self {
            shard_size: SHARD_SIZE,
            overhead_ratio: 0.2,
            min_parity_single: 2,
            reorder_window: 1,
            max_pending: 8,
        }
    }
}

struct Counters {
    frames_tx: Arc<AtomicU32>,
    frames_rx: Arc<AtomicU32>,
    frames_recovered: Arc<AtomicU32>,
    frames_lost: Arc<AtomicU32>,
    overhead_pct: Arc<AtomicU32>,
}

/// FEC 运行期实时统计快照（供方案实验室展示真实救回帧数与丢帧数）。
#[derive(Clone, Copy, Debug, Default)]
pub struct FecRsStats {
    pub frames_tx: u32,
    pub frames_rx: u32,
    pub frames_recovered: u32,
    pub frames_lost: u32,
    pub overhead_pct: u32,
}

/// Moonlight 级 RS FEC 适配器。
pub struct FecRs {
    cfg: FecRsConfig,
    counters: Arc<Counters>,
}

impl FecRs {
    pub fn new(cfg: FecRsConfig) -> Self {
        let pct = (cfg.overhead_ratio * 100.0).round() as u32;
        Self {
            cfg,
            counters: Arc::new(Counters {
                frames_tx: Arc::new(AtomicU32::new(0)),
                frames_rx: Arc::new(AtomicU32::new(0)),
                frames_recovered: Arc::new(AtomicU32::new(0)),
                frames_lost: Arc::new(AtomicU32::new(0)),
                overhead_pct: Arc::new(AtomicU32::new(pct.clamp(1, 200))),
            }),
        }
    }

    /// 动态调整冗余比例（5% ~ 100%，方案实验室在线热调）。
    pub fn set_overhead_pct(&self, pct: u32) {
        self.counters
            .overhead_pct
            .store(pct.clamp(1, 200), Ordering::Relaxed);
    }

    /// 读取实时 FEC 统计（累计发送/接收/RS重塑救回/彻底丢失帧数）。
    pub fn stats_snapshot(&self) -> FecRsStats {
        FecRsStats {
            frames_tx: self.counters.frames_tx.load(Ordering::Relaxed),
            frames_rx: self.counters.frames_rx.load(Ordering::Relaxed),
            frames_recovered: self.counters.frames_recovered.load(Ordering::Relaxed),
            frames_lost: self.counters.frames_lost.load(Ordering::Relaxed),
            overhead_pct: self.counters.overhead_pct.load(Ordering::Relaxed),
        }
    }
}

#[async_trait]
impl MediaAdapt for FecRs {
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
        let tx = FecRsTx {
            ch: tx_side,
            shard,
            cfg: self.cfg,
            bus: Arc::clone(&bus),
            frames: Arc::clone(&self.counters.frames_tx),
            lost: Arc::clone(&self.counters.frames_lost),
            overhead_pct: Arc::clone(&self.counters.overhead_pct),
        };
        let rx = FecRsRx {
            ch: rx_side,
            bus,
            counters: Arc::clone(&self.counters),
            pending: HashMap::new(),
            last_completed: None,
            outbox: VecDeque::new(),
            cfg: self.cfg,
        };
        Ok((Box::new(tx), Box::new(rx)))
    }
}

struct FecRsTx {
    ch: Arc<dyn Channel>,
    shard: usize,
    cfg: FecRsConfig,
    bus: Arc<EventBus>,
    frames: Arc<AtomicU32>,
    lost: Arc<AtomicU32>,
    overhead_pct: Arc<AtomicU32>,
}

impl FecRsTx {
    /// 发送一个分片；队满（WouldBlock）≠ 关闭：整帧放弃并置 loss 标记（01 §8）。
    /// 返回 false = 已放弃整帧，调用方立即返回。
    async fn send_or_abandon(&mut self, pkt: Bytes, frame_id: u32) -> Result<bool, MediaError> {
        match self.ch.send_dgram(pkt).await {
            Ok(()) => Ok(true),
            Err(SendError::WouldBlock) => {
                self.lost.fetch_add(1, Ordering::Relaxed);
                self.bus.emit_event(
                    "frame_drop",
                    serde_json::json!({
                        "frame_id": frame_id, "reason": "queue_full",
                        "stream": "video", "abandoned": true
                    }),
                );
                Ok(false)
            }
            Err(_) => Err(MediaError::Closed),
        }
    }
}

#[allow(clippy::too_many_arguments)] // 线格式头一次性写入，拆参反而更难核对
fn put_hdr(
    buf: &mut BytesMut,
    frame_id: u32,
    orig_len: u32,
    shard_size: u16,
    k: u16,
    idx: u16,
    m: u8,
    pidx: u8,
    ty: u8,
) {
    buf.put_u32(frame_id);
    buf.put_u32(orig_len);
    buf.put_u16(shard_size);
    buf.put_u16(k);
    buf.put_u16(idx);
    buf.put_u8(m);
    buf.put_u8(pidx);
    buf.put_u8(ty);
}

#[async_trait]
impl FrameSink for FecRsTx {
    async fn submit(&mut self, frame: EncodedFrame) -> Result<QueueDepth, MediaError> {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let s = self.shard;
        let total = frame.data.len().div_ceil(s).max(1);

        // 切块（每块 ≤ MAX_BLOCK_DATA 数据片），逐块 RS 编码
        let block_data = total.min(MAX_BLOCK_DATA);
        let mut active_cfg = self.cfg;
        active_cfg.overhead_ratio =
            (self.overhead_pct.load(Ordering::Relaxed) as f32 / 100.0).clamp(0.01, 2.0);
        let m = parity_count(active_cfg, block_data);
        let rs: ReedSolomon = ReedSolomon::new(block_data, m).map_err(|_| MediaError::Closed)?;

        let mut block = vec![vec![0u8; s]; block_data];
        for (bi, bstart) in (0..total).step_by(block_data).enumerate() {
            let bend = (bstart + block_data).min(total);
            let k_here = bend - bstart;
            let m_here = if k_here == 1 && bi == 0 && total == 1 {
                parity_count(active_cfg, 1)
            } else {
                m
            };
            // 装填数据片
            for (i, shard_buf) in block.iter_mut().enumerate().take(k_here) {
                let base = (bstart + i) * s;
                let src_end = ((bstart + i) * s + s).min(frame.data.len());
                if base < frame.data.len() {
                    let src = &frame.data[base..src_end];
                    shard_buf[..src.len()].copy_from_slice(src);
                }
            }
            // RS 编码：k_here 数据 + m_here 校验（非满块重开编码器）
            let rs_here = if k_here == block_data && m_here == m {
                &rs
            } else {
                &ReedSolomon::new(k_here, m_here).map_err(|_| MediaError::Closed)?
            };
            let mut shards: Vec<Vec<u8>> = block[..k_here].to_vec();
            shards.resize(k_here + m_here, vec![0u8; s]);
            rs_here
                .encode(&mut shards)
                .map_err(|_| MediaError::Closed)?;

            // 发送数据片（idx=全局片序号，pidx=块号）
            for (i, shard_buf) in shards.iter().enumerate().take(k_here) {
                let mut pkt = BytesMut::with_capacity(HEADER + s);
                put_hdr(
                    &mut pkt,
                    frame.frame_id,
                    frame.data.len() as u32,
                    s as u16,
                    k_here as u16,
                    (bstart + i) as u16,
                    m_here as u8,
                    bi as u8,
                    TYPE_DATA,
                );
                pkt.extend_from_slice(shard_buf);
                if !self.send_or_abandon(pkt.freeze(), frame.frame_id).await? {
                    return Ok(QueueDepth { queued_frames: 0 });
                }
            }
            // 发送校验片
            for (pi, shard_buf) in shards.iter().enumerate().skip(k_here) {
                let mut pkt = BytesMut::with_capacity(HEADER + s);
                put_hdr(
                    &mut pkt,
                    frame.frame_id,
                    frame.data.len() as u32,
                    s as u16,
                    k_here as u16,
                    bi as u16,
                    m_here as u8,
                    (pi - k_here) as u8,
                    TYPE_PARITY,
                );
                pkt.extend_from_slice(shard_buf);
                if !self.send_or_abandon(pkt.freeze(), frame.frame_id).await? {
                    return Ok(QueueDepth { queued_frames: 0 });
                }
            }
        }
        Ok(QueueDepth { queued_frames: 0 })
    }
}

/// 单块重组状态。
struct BlockReasm {
    k: u16,
    m: u8,
    /// k 数据片 + m 校验片
    shards: Vec<Option<Vec<u8>>>,
    present: usize,
}

struct Reasm {
    orig_len: u32,
    shard_size: usize,
    blocks: HashMap<u16, BlockReasm>,
    /// 期望数据片总数 = ceil(orig_len / shard_size)
    data_expected: usize,
    /// 满块的数据片数（块 0 必为满块；全局片序号 = 块号 × max_k + 块内序号）
    max_k: usize,
}

impl Reasm {
    fn complete(&self) -> bool {
        let have: usize = self
            .blocks
            .values()
            .map(|b| {
                b.shards[..b.k as usize]
                    .iter()
                    .filter(|s| s.is_some())
                    .count()
            })
            .sum();
        have >= self.data_expected
    }
}

struct FecRsRx {
    ch: Arc<dyn Channel>,
    bus: Arc<EventBus>,
    counters: Arc<Counters>,
    pending: HashMap<u32, Reasm>,
    last_completed: Option<u32>,
    outbox: VecDeque<FrameIn>,
    cfg: FecRsConfig,
}

impl FecRsRx {
    /// RS 重建：块内收到的片数 ≥ k 时可恢复全部数据片。
    fn try_reconstruct(r: &mut Reasm, _cfg_shard: usize, counters: &Arc<Counters>) -> usize {
        let mut recovered_blocks = 0;
        let bs: Vec<(u16, usize, usize)> = r
            .blocks
            .iter()
            .map(|(id, b)| (*id, b.k as usize, b.m as usize))
            .collect();
        for (id, k, m) in bs {
            let b = r.blocks.get_mut(&id).unwrap();
            if b.present >= k && b.present < k + m {
                // crate 6.0：ReconstructShard 实现于 Option<Vec<u8>>，None = 缺失片
                let rs: ReedSolomon = match ReedSolomon::new(k, m) {
                    Ok(rs) => rs,
                    Err(_) => continue,
                };
                if rs.reconstruct(&mut b.shards).is_ok() {
                    b.present = k + m;
                    recovered_blocks += 1;
                }
            }
        }
        if recovered_blocks > 0 {
            counters.frames_recovered.fetch_add(1, Ordering::Relaxed);
        }
        recovered_blocks
    }

    fn assemble(&mut self, fid: u32) -> Option<FrameIn> {
        let r = self.pending.remove(&fid)?;
        if !r.complete() {
            self.pending.insert(fid, r);
            return None;
        }
        let mut out = vec![0u8; r.orig_len as usize];
        for (block_id, b) in &r.blocks {
            for (i, shard) in b.shards.iter().take(b.k as usize).enumerate() {
                let global = *block_id as usize * r.max_k + i;
                let base = global * r.shard_size;
                let Some(sh) = shard.as_ref() else {
                    continue;
                };
                if base >= out.len() {
                    continue;
                }
                let n = (base + r.shard_size).min(out.len()) - base;
                out[base..base + n].copy_from_slice(&sh[..n]);
            }
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

#[async_trait]
impl FrameSource for FecRsRx {
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
            let k = u16::from_be_bytes(pkt[10..12].try_into().unwrap());
            let idx = u16::from_be_bytes(pkt[12..14].try_into().unwrap());
            let m = pkt[14];
            let pidx = pkt[15];
            let ty = pkt[16];
            let parity = ty == TYPE_PARITY;
            let payload = &pkt[HEADER..];

            if matches!(self.last_completed, Some(lc) if fid <= lc) {
                continue;
            }

            if !self.pending.contains_key(&fid) && self.pending.len() >= self.cfg.max_pending {
                if let Some(oldest) = self.pending.keys().copied().min() {
                    if let Some(_rr) = self.pending.remove(&oldest) {
                        self.counters.frames_lost.fetch_add(1, Ordering::Relaxed);
                        self.bus.emit_event(
                            "frame_lost",
                            serde_json::json!({ "frame_id": oldest, "reason": "pending_cap" }),
                        );
                        self.outbox.push_back(FrameIn::Lost { frame_id: oldest });
                    }
                }
            }
            let r = self.pending.entry(fid).or_insert(Reasm {
                orig_len,
                shard_size: ss,
                blocks: HashMap::new(),
                data_expected: (orig_len as usize).div_ceil(ss).max(1),
                // 审查 #3 修复：max_k 从包头字段确定性推导（tx 的 block_data =
                // min(total, MAX_BLOCK_DATA)），不再依赖到达顺序学习——彻底消除
                // 「块 1 先于块 0 到达 → 槽位错位 → RS 伪恢复」的静默腐蚀路径
                max_k: (orig_len as usize).div_ceil(ss).clamp(1, MAX_BLOCK_DATA),
            });
            // 块号：数据片取 pidx，校验片取 idx（均为显式字段，无推断）
            let block_id = if parity { idx } else { pidx as u16 };
            let b = r.blocks.entry(block_id).or_insert_with(|| BlockReasm {
                k,
                m,
                shards: (0..k as usize + m as usize).map(|_| None).collect(),
                present: 0,
            });
            let slot = if parity {
                k as usize + pidx as usize
            } else {
                // 数据片块内序号 = 全局序号 − 块号 × max_k（max_k 已由包头确定，与顺序无关）
                let local = (idx as usize).saturating_sub(block_id as usize * r.max_k);
                local.min(k as usize - 1)
            };
            if slot < b.shards.len() && b.shards[slot].is_none() {
                b.shards[slot] = Some(payload.to_vec());
                b.present += 1;
            }
            Self::try_reconstruct(r, 0, &self.counters);
            let assembled = self.assemble(fid);

            // 乱序窗外旧帧先报 Lost（不吞新帧；同包触发的 Complete 进 outbox）
            let stale_bound = fid.saturating_sub(self.cfg.reorder_window);
            let stale = self
                .pending
                .keys()
                .copied()
                .filter(|kk| *kk < stale_bound)
                .min();
            if let Some(stale) = stale {
                let rr = self.pending.remove(&stale).unwrap();
                self.counters.frames_lost.fetch_add(1, Ordering::Relaxed);
                self.bus.emit_event(
                    "frame_lost",
                    serde_json::json!({ "frame_id": stale, "had": rr.blocks.len(), "expected_data": rr.data_expected }),
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
    use switchboard_core::modules::loopback::LoopbackPairTransport;
    use switchboard_core::stages::Transport;

    async fn bind() -> (Box<dyn FrameSink>, Box<dyn FrameSource>, Arc<EventBus>) {
        let t = LoopbackPairTransport::new(1200);
        let a: Arc<dyn Channel> = Arc::from(
            t.establish(Vec::new(), Arc::new(EventBus::new()))
                .await
                .unwrap(),
        );
        let b: Arc<dyn Channel> = Arc::from(t.take_remote().await.unwrap());
        let fec = FecRs::new(FecRsConfig::default());
        let bus = Arc::new(EventBus::new());
        let (tx, rx) = fec.bind(a, b, bus.clone()).await.unwrap();
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
        fn caps(&self) -> &switchboard_core::caps::ChannelCaps {
            self.inner.caps()
        }
        fn conn_id(&self) -> &str {
            self.inner.conn_id()
        }
        async fn send_dgram(&self, b: Bytes) -> Result<(), switchboard_core::channel::SendError> {
            let kk = self.seen.fetch_add(1, Ordering::SeqCst);
            if kk == self.n {
                return Ok(()); // 静默丢弃
            }
            self.inner.send_dgram(b).await
        }
        async fn recv_dgram(&self) -> Result<Bytes, switchboard_core::channel::RecvError> {
            self.inner.recv_dgram().await
        }
        async fn open_stream(
            &self,
            _k: switchboard_core::channel::StreamKind,
        ) -> Result<
            Box<dyn switchboard_core::channel::Stream>,
            switchboard_core::channel::ChannelError,
        > {
            unimplemented!()
        }
        async fn accept_stream(
            &self,
        ) -> Result<
            (
                switchboard_core::channel::StreamKind,
                Box<dyn switchboard_core::channel::Stream>,
            ),
            switchboard_core::channel::ChannelError,
        > {
            unimplemented!()
        }
        fn stats(&self) -> switchboard_core::telemetry::StageStats {
            self.inner.stats()
        }
        fn close(&self) {
            self.inner.close();
        }
        fn is_closed(&self) -> bool {
            self.inner.is_closed()
        }
    }

    #[tokio::test]
    async fn 单片帧_默认200pct_丢2片仍可恢复() {
        // 100B 帧 → k=1, m=2（Moonlight 200% 规则）：丢任意 2 片中的 1 片数据片仍可恢复
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
        let fec = FecRs::new(FecRsConfig::default());
        let (mut tx, mut rx) = fec.bind(a, b, Arc::new(EventBus::new())).await.unwrap();
        tx.submit(EncodedFrame {
            frame_id: 9,
            data: Bytes::from(vec![3u8; 100]),
        })
        .await
        .unwrap();
        match rx.next().await.unwrap() {
            FrameIn::Complete { frame_id, data } => {
                assert_eq!(frame_id, 9);
                assert_eq!(
                    data,
                    Bytes::from(vec![3u8; 100]),
                    "k=1,m=2 丢 1 数据片应 RS 重建"
                );
            }
            other => panic!("期望 RS 恢复后 Complete，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn 突发丢2片_20pct比例可恢复() {
        // 5 片帧 k=5, m=1：丢 1 片可恢复（20% 比例的基线能力）
        let t = LoopbackPairTransport::new(1200);
        let a_raw: Arc<dyn Channel> = Arc::from(
            t.establish(Vec::new(), Arc::new(EventBus::new()))
                .await
                .unwrap(),
        );
        let b: Arc<dyn Channel> = Arc::from(t.take_remote().await.unwrap());
        let a: Arc<dyn Channel> = Arc::new(DropNth {
            inner: a_raw,
            n: 2,
            seen: Default::default(),
        });
        let fec = FecRs::new(FecRsConfig {
            overhead_ratio: 0.2,
            ..Default::default()
        });
        let (mut tx, mut rx) = fec.bind(a, b, Arc::new(EventBus::new())).await.unwrap();
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
                assert_eq!(&got[..], &data[..], "RS 恢复后内容必须一致");
            }
            other => panic!("期望 Complete，得到 {other:?}"),
        }
    }
}
