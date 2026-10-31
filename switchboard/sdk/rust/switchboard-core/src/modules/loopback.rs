//! 参考传输 `sb.core.loopback`（L-B）：进程内内存双端管道。
//!
//! 用途：T0 合成基准、SDK 自测、Studio 演示。真实部署两端在不同进程；
//! 单进程 harness 用 [`LoopbackPairTransport::take_remote`] 取对端。

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::caps::{ChannelCaps, DatagramCaps, StreamCaps};
use crate::channel::{Channel, ChannelError, RecvError, SendError, Stream, StreamKind};
use crate::stages::Transport;
use crate::telemetry::{EventBus, PathTag, Stage, StageStats};
use crate::types::{PathHandle, PathKind};

/// wire 消息（对端 pump 分发）。
enum Wire {
    Dgram(Bytes),
    Open { sid: u64, kind: StreamKind },
    Data { sid: u64, b: Bytes },
    Fin { sid: u64 },
    Close,
}

/// 统计寄存器（双端共享，遥测口径统一，规范 03）。
struct StatsReg {
    conn: String,
    start: std::time::Instant,
    dgrams_sent: AtomicU64,
    dgrams_recv: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_recv: AtomicU64,
    dropped_queue: AtomicU64,
    lost: AtomicU64,
}

impl StatsReg {
    fn new() -> Self {
        Self {
            conn: uuid_lite(),
            start: std::time::Instant::now(),
            dgrams_sent: AtomicU64::new(0),
            dgrams_recv: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_recv: AtomicU64::new(0),
            dropped_queue: AtomicU64::new(0),
            lost: AtomicU64::new(0),
        }
    }
}

/// 轻量 uuid（非密码学用途，避免引入依赖）。
fn uuid_lite() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "lb-{:x}-{:x}",
        t.as_millis() as u64,
        std::process::id() as u64
    )
}

fn next_sid() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

type StreamBufRx = mpsc::Receiver<Bytes>;

struct Inner {
    caps: ChannelCaps,
    wire_tx: mpsc::Sender<Wire>,
    /// 已打开待 accept 的流（对端 Open 到达时由 pump 登记）。
    open_rx: tokio::sync::Mutex<mpsc::Receiver<(u64, StreamKind, StreamBufRx)>>,
    /// 接收方向流缓冲：sid → 发送端（Data 路由用）。
    inbound: Mutex<HashMap<u64, mpsc::Sender<Bytes>>>,
    /// 待 accept 的数据报。
    dgram_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    stats: Arc<StatsReg>,
    closed: AtomicBool,
}

pub struct LoopbackChannel {
    inner: Arc<Inner>,
}

impl LoopbackChannel {
    fn make_caps(mtu: u16) -> ChannelCaps {
        ChannelCaps {
            datagram: Some(DatagramCaps {
                max_size: mtu.saturating_sub(16),
                lossy_ok: true,
            }),
            streams: Some(StreamCaps {
                reliable_ordered: true,
                bidi: true,
                max_concurrent: 64,
            }),
            congestion: vec![crate::caps::CongestionKind::Cubic],
            mtu,
            multipath: false,
        }
    }

    fn pair_with_capacity(mtu: u16, cap: usize) -> (Self, Self) {
        let caps = Self::make_caps(mtu);
        let stats = Arc::new(StatsReg::new());
        // 两条方向相反的 wire 通道：A: tx_a→(B 的 pump 读 rx_a)；B: tx_b→(A 的 pump 读 rx_b)
        let (tx_a, rx_a) = mpsc::channel::<Wire>(cap);
        let (tx_b, rx_b) = mpsc::channel::<Wire>(cap);
        let a = Self::build(Arc::clone(&stats), caps.clone(), tx_a, rx_b);
        let b = Self::build(stats, caps, tx_b, rx_a);
        (a, b)
    }

    fn build(
        stats: Arc<StatsReg>,
        caps: ChannelCaps,
        wire_tx: mpsc::Sender<Wire>,
        wire_rx: mpsc::Receiver<Wire>,
    ) -> Self {
        let (dgram_tx, dgram_rx) = mpsc::channel::<Bytes>(256);
        let (open_tx, open_rx) = mpsc::channel::<(u64, StreamKind, StreamBufRx)>(16);
        let inner = Arc::new(Inner {
            caps,
            wire_tx,
            open_rx: tokio::sync::Mutex::new(open_rx),
            inbound: Mutex::new(HashMap::new()),
            dgram_rx: tokio::sync::Mutex::new(dgram_rx),
            stats,
            closed: AtomicBool::new(false),
        });
        spawn_pump(Arc::clone(&inner), dgram_tx, open_tx, wire_rx);
        Self { inner }
    }
}

/// pump：从 wire 收消息分发（01 §6 MUST 4：datagram 与流共存）。
fn spawn_pump(
    inner: Arc<Inner>,
    dgram_tx: mpsc::Sender<Bytes>,
    open_tx: mpsc::Sender<(u64, StreamKind, StreamBufRx)>,
    mut wire_rx: mpsc::Receiver<Wire>,
) {
    tokio::spawn(async move {
        while let Some(msg) = wire_rx.recv().await {
            match msg {
                Wire::Dgram(b) => {
                    inner.stats.dgrams_recv.fetch_add(1, Ordering::Relaxed);
                    inner
                        .stats
                        .bytes_recv
                        .fetch_add(b.len() as u64, Ordering::Relaxed);
                    // 接收侧溢出丢弃（lossy_ok 语义），计入 lost
                    if dgram_tx.try_send(b).is_err() {
                        inner.stats.lost.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Wire::Open { sid, kind } => {
                    let (buf_tx, buf_rx) = mpsc::channel::<Bytes>(64);
                    inner.inbound.lock().unwrap().insert(sid, buf_tx);
                    if open_tx.try_send((sid, kind, buf_rx)).is_err() {
                        inner.inbound.lock().unwrap().remove(&sid);
                    }
                }
                Wire::Data { sid, b } => {
                    if let Some(tx) = inner.inbound.lock().unwrap().get(&sid) {
                        let _ = tx.try_send(b);
                    }
                }
                Wire::Fin { sid } => {
                    // 丢发送端 → 接收端 recv() 返回 None（FIN 语义）
                    inner.inbound.lock().unwrap().remove(&sid);
                }
                Wire::Close => break,
            }
        }
        inner.closed.store(true, Ordering::SeqCst);
    });
}

#[async_trait]
impl Channel for LoopbackChannel {
    fn caps(&self) -> &ChannelCaps {
        &self.inner.caps
    }

    fn conn_id(&self) -> &str {
        &self.inner.stats.conn
    }

    async fn send_dgram(&self, b: Bytes) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }
        let max = self
            .inner
            .caps
            .datagram
            .as_ref()
            .map(|d| d.max_size)
            .unwrap_or(0) as usize;
        if b.len() > max {
            // 01 §6 MUST 2：超限 = 丢新包，绝不阻塞
            self.inner
                .stats
                .dropped_queue
                .fetch_add(1, Ordering::Relaxed);
            return Err(SendError::WouldBlock);
        }
        match self.inner.wire_tx.try_send(Wire::Dgram(b)) {
            Ok(()) => {
                self.inner.stats.dgrams_sent.fetch_add(1, Ordering::Relaxed);
                self.inner.stats.bytes_sent.fetch_add(0, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.inner
                    .stats
                    .dropped_queue
                    .fetch_add(1, Ordering::Relaxed);
                Err(SendError::WouldBlock)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SendError::Closed),
        }
    }

    async fn recv_dgram(&self) -> Result<Bytes, RecvError> {
        let mut rx = self.inner.dgram_rx.lock().await;
        match rx.recv().await {
            Some(b) => Ok(b),
            None => Err(RecvError::Closed),
        }
    }

    async fn open_stream(&self, kind: StreamKind) -> Result<Box<dyn Stream>, ChannelError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(ChannelError::Closed);
        }
        let sid = next_sid();
        let (buf_tx, buf_rx) = mpsc::channel::<Bytes>(64);
        self.inner.inbound.lock().unwrap().insert(sid, buf_tx);
        self.inner
            .wire_tx
            .send(Wire::Open { sid, kind })
            .await
            .map_err(|_| ChannelError::Closed)?;
        Ok(Box::new(LoopbackStream {
            sid,
            out: self.inner.wire_tx.clone(),
            in_rx: tokio::sync::Mutex::new(buf_rx),
            stats: Arc::clone(&self.inner.stats),
        }))
    }

    async fn accept_stream(&self) -> Result<(StreamKind, Box<dyn Stream>), ChannelError> {
        let mut rx = self.inner.open_rx.lock().await;
        match rx.recv().await {
            Some((sid, kind, buf_rx)) => Ok((
                kind,
                Box::new(LoopbackStream {
                    sid,
                    out: self.inner.wire_tx.clone(),
                    in_rx: tokio::sync::Mutex::new(buf_rx),
                    stats: Arc::clone(&self.inner.stats),
                }),
            )),
            None => Err(ChannelError::Closed),
        }
    }

    fn stats(&self) -> StageStats {
        let s = &self.inner.stats;
        let secs = s.start.elapsed().as_secs_f32().max(1e-6);
        StageStats {
            v: crate::TELEMETRY_VERSION,
            ts_ms: s.start.elapsed().as_millis() as u64,
            stage: Stage::Transport.as_str().into(),
            module: "sb.core.loopback".into(),
            conn: s.conn.clone(),
            path: PathTag {
                kind: PathKind::Loopback,
                local: None,
                remote: None,
            },
            rtt_ewma_ms: 0.05,
            loss_ratio: s.lost.load(Ordering::Relaxed) as f32
                / (s.dgrams_sent.load(Ordering::Relaxed) + 1) as f32,
            send_mbps: s.bytes_sent.load(Ordering::Relaxed) as f32 * 8.0 / secs / 1e6,
            recv_mbps: s.bytes_recv.load(Ordering::Relaxed) as f32 * 8.0 / secs / 1e6,
            queue_depth_bytes: 0,
            jitter_ms: 0.0,
            extra: Default::default(),
        }
    }

    fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.inner.wire_tx.try_send(Wire::Close);
    }

    fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

struct LoopbackStream {
    sid: u64,
    out: mpsc::Sender<Wire>,
    in_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    stats: Arc<StatsReg>,
}

#[async_trait]
impl Stream for LoopbackStream {
    async fn send(&mut self, b: &[u8]) -> Result<(), ChannelError> {
        self.stats
            .bytes_sent
            .fetch_add(b.len() as u64, Ordering::Relaxed);
        self.out
            .send(Wire::Data {
                sid: self.sid,
                b: Bytes::copy_from_slice(b),
            })
            .await
            .map_err(|_| ChannelError::Closed)
    }

    async fn recv(&mut self) -> Option<Bytes> {
        self.in_rx.lock().await.recv().await
    }

    fn finish(&mut self) {
        let _ = self.out.try_send(Wire::Fin { sid: self.sid });
    }
}

/// 参考传输工厂：establish 产出 A 端，B 端由 [`Self::take_remote`] 取走（单进程 harness）。
pub struct LoopbackPairTransport {
    mtu: u16,
    capacity: usize,
    caps: ChannelCaps,
    pending_remote: tokio::sync::Mutex<Option<LoopbackChannel>>,
}

impl LoopbackPairTransport {
    pub fn new(mtu: u16) -> Self {
        Self::with_capacity(mtu, 256)
    }

    /// 自定义队列容量（大帧分片基准用）。
    pub fn with_capacity(mtu: u16, capacity: usize) -> Self {
        Self {
            mtu,
            capacity,
            caps: LoopbackChannel::make_caps(mtu),
            pending_remote: tokio::sync::Mutex::new(None),
        }
    }

    /// 取走最近一次 establish 的对端（None = 已取走/未建立）。
    pub async fn take_remote(&self) -> Option<LoopbackChannel> {
        self.pending_remote.lock().await.take()
    }
}

#[async_trait]
impl Transport for LoopbackPairTransport {
    fn offers(&self) -> &ChannelCaps {
        &self.caps
    }

    async fn establish(
        &self,
        _paths: Vec<PathHandle>,
        _bus: Arc<EventBus>,
    ) -> anyhow::Result<Box<dyn Channel>> {
        let (a, b) = LoopbackChannel::pair_with_capacity(self.mtu, self.capacity);
        *self.pending_remote.lock().await = Some(b);
        Ok(Box::new(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (Box<dyn Channel>, Box<dyn Channel>) {
        let t = LoopbackPairTransport::new(1200);
        let a: Box<dyn Channel> = t
            .establish(Vec::new(), Arc::new(EventBus::new()))
            .await
            .unwrap();
        let b: Box<dyn Channel> = Box::new(t.take_remote().await.unwrap());
        (a, b)
    }

    #[tokio::test]
    async fn 数据报往返_与_队列满丢新包() {
        let (a, b) = setup().await;
        a.send_dgram(Bytes::from_static(b"ping")).await.unwrap();
        let got = b.recv_dgram().await.unwrap();
        assert_eq!(&got[..], b"ping");

        // 灌满 256 深度队列后必须 WouldBlock（不阻塞）——01 §6 MUST 2
        let big = Bytes::from(vec![0u8; 100]);
        for _ in 0..4096 {
            let _ = a.send_dgram(big.clone()).await;
        }
        assert!(matches!(
            a.send_dgram(big).await,
            Err(SendError::WouldBlock)
        ));
        a.close();
        assert!(a.is_closed());
    }

    #[tokio::test]
    async fn 流双向与fin语义() {
        let (a, b) = setup().await;
        let mut s = a.open_stream(StreamKind::Terminal).await.unwrap();
        s.send(b"hello").await.unwrap();
        let (_kind, mut rs) = b.accept_stream().await.unwrap();
        let got = rs.recv().await.unwrap();
        assert_eq!(&got[..], b"hello");
        s.finish();
        assert!(rs.recv().await.is_none());
    }

    #[tokio::test]
    async fn 遥测含必填字段() {
        let (a, _b) = setup().await;
        a.send_dgram(Bytes::from_static(b"x")).await.unwrap();
        let s = a.stats();
        assert!(crate::telemetry::stats_complete(
            &serde_json::to_string(&s).unwrap()
        ));
        assert_eq!(s.path.kind, PathKind::Loopback);
    }
}
