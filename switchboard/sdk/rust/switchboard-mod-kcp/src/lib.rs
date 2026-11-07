//! 传输适配器 `sb.kcp.kcp`（L-B）：KCP 可靠 UDP 直连（RustDesk 直连路径的同源协议栈）。
//!
//! 出身：RustDesk `kcp_stream.rs`（kcp-sys new_turbo：nodelay=true, interval=10,
//! resend=2, nc=true）+ `bytes_codec.rs` 变长分帧。本模块用开源 `kcp` crate 0.6
//! 复刻该参数档（turbo），单 UDP socket 单会话，独立线程驱动状态机。
//!
//! caps 诚实性：KCP 是**可靠有序**传输——`datagram.lossy_ok = false`（fec-shards
//! 协商将正确拒绝它），`reliable-idr` profile 可用。`send_dgram` 在此模块 = 可靠
//! 消息（与 RustDesk 在 KCP 上套可靠分帧的用法一致）。
//!
//! 帧格式（复用管道内分帧）：`type:u8 | ...`：
//! - `0x01` 数据报：`len:u32 + data`
//! - `0x02` 流打开：`kind:u8 + sid:u32`
//! - `0x03` 流数据：`sid:u32 + len:u32 + data`
//! - `0x04` 流 FIN：`sid:u32`

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use switchboard_core::caps::{ChannelCaps, CongestionKind, DatagramCaps, StreamCaps};
use switchboard_core::channel::{
    Channel, ChannelError, RecvError, SendError, Stream, StreamKind,
};
use switchboard_core::stages::Transport;
use switchboard_core::telemetry::{EventBus, PathTag, Stage, StageStats};
use switchboard_core::types::{PathHandle, PathKind};

const T_DGRAM: u8 = 0x01;
const T_OPEN: u8 = 0x02;
const T_SDATA: u8 = 0x03;
const T_FIN: u8 = 0x04;
/// KCP 头开销（kcp crate KCP_OVERHEAD）。
const KCP_OVERHEAD: usize = 24;

struct StatsReg {
    conn: String,
    start: Instant,
    bytes_sent: AtomicU64,
    bytes_recv: AtomicU64,
    dropped: AtomicU64,
}

/// 每端的共享接收面（驱动线程 → Channel）。
struct SideRx {
    dgram_tx: mpsc::Sender<Bytes>,
    open_tx: mpsc::Sender<(u64, StreamKind)>,
    inbound_streams: Arc<Mutex<HashMap<u64, mpsc::Sender<Bytes>>>>,
    pending_open: Arc<Mutex<HashMap<u64, mpsc::Receiver<Bytes>>>>,
    stats: Arc<StatsReg>,
}

/// KCP Channel：可靠消息（datagram API）+ 单流。
pub struct KcpChannel {
    app_tx: mpsc::UnboundedSender<Bytes>,
    rx: Arc<SideRx>,
    dgram_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    open_rx: tokio::sync::Mutex<mpsc::Receiver<(u64, StreamKind)>>,
    stats: Arc<StatsReg>,
    caps: ChannelCaps,
    path: PathKind,
    closed: Arc<AtomicBool>,
}

impl KcpChannel {
    fn write_frame(&self, mut frame: BytesMut) -> Result<(), SendError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }
        self.app_tx.send(frame.split().freeze()).map_err(|_| SendError::Closed)
    }
}

#[async_trait]
impl Channel for KcpChannel {
    fn caps(&self) -> &ChannelCaps {
        &self.caps
    }

    fn conn_id(&self) -> &str {
        &self.stats.conn
    }

    async fn send_dgram(&self, b: Bytes) -> Result<(), SendError> {
        // KCP 是可靠传输：datagram API = 可靠消息（lossy_ok=false，caps 已如实）
        self.stats.bytes_sent.fetch_add(b.len() as u64, Ordering::Relaxed);
        let mut f = BytesMut::with_capacity(5 + b.len());
        f.put_u8(T_DGRAM);
        f.put_u32(b.len() as u32);
        f.extend_from_slice(&b);
        self.write_frame(f)
    }

    async fn recv_dgram(&self) -> Result<Bytes, RecvError> {
        let mut rx = self.dgram_rx.lock().await;
        match rx.recv().await {
            Some(b) => Ok(b),
            None => Err(RecvError::Closed),
        }
    }

    async fn open_stream(&self, kind: StreamKind) -> Result<Box<dyn Stream>, ChannelError> {
        static SID: AtomicU64 = AtomicU64::new(1);
        let sid = SID.fetch_add(1, Ordering::Relaxed);
        let (buf_tx, buf_rx) = mpsc::channel::<Bytes>(64);
        self.rx.inbound_streams.lock().unwrap().insert(sid, buf_tx);
        let mut f = BytesMut::with_capacity(6);
        f.put_u8(T_OPEN);
        f.put_u8(kind.as_u8());
        f.put_u32(sid as u32);
        self.write_frame(f).map_err(|_| ChannelError::Closed)?;
        Ok(Box::new(KcpStream {
            sid,
            app_tx: self.app_tx.clone(),
            in_rx: tokio::sync::Mutex::new(buf_rx),
        }))
    }

    async fn accept_stream(&self) -> Result<(StreamKind, Box<dyn Stream>), ChannelError> {
        let mut rx = self.open_rx.lock().await;
        match rx.recv().await {
            Some((sid, kind)) => {
                let buf_rx = self
                    .rx
                    .pending_open
                    .lock()
                    .unwrap()
                    .remove(&sid)
                    .ok_or(ChannelError::Closed)?;
                Ok((
                    kind,
                    Box::new(KcpStream {
                        sid,
                        app_tx: self.app_tx.clone(),
                        in_rx: tokio::sync::Mutex::new(buf_rx),
                    }),
                ))
            }
            None => Err(ChannelError::Closed),
        }
    }

    fn stats(&self) -> StageStats {
        let secs = self.stats.start.elapsed().as_secs_f32().max(1e-6);
        StageStats {
            v: switchboard_core::TELEMETRY_VERSION,
            ts_ms: self.stats.start.elapsed().as_millis() as u64,
            stage: Stage::Transport.as_str().into(),
            module: "sb.kcp.kcp".into(),
            conn: self.stats.conn.clone(),
            path: PathTag { kind: self.path, local: None, remote: None },
            rtt_ewma_ms: 0.0, // KCP 无内建 RTT 外露；v0.2 用控制帧测
            loss_ratio: 0.0,
            send_mbps: self.stats.bytes_sent.load(Ordering::Relaxed) as f32 * 8.0 / secs / 1e6,
            recv_mbps: self.stats.bytes_recv.load(Ordering::Relaxed) as f32 * 8.0 / secs / 1e6,
            queue_depth_bytes: 0,
            jitter_ms: 0.0,
            extra: Default::default(),
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

struct KcpStream {
    sid: u64,
    app_tx: mpsc::UnboundedSender<Bytes>,
    in_rx: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
}

#[async_trait]
impl Stream for KcpStream {
    async fn send(&mut self, b: &[u8]) -> Result<(), ChannelError> {
        let mut f = BytesMut::with_capacity(9 + b.len());
        f.put_u8(T_SDATA);
        f.put_u32(self.sid as u32);
        f.put_u32(b.len() as u32);
        f.extend_from_slice(b);
        self.app_tx.send(f.split().freeze()).map_err(|_| ChannelError::Closed)
    }

    async fn recv(&mut self) -> Option<Bytes> {
        self.in_rx.lock().await.recv().await
    }

    fn finish(&mut self) {
        let mut f = BytesMut::with_capacity(5);
        f.put_u8(T_FIN);
        f.put_u32(self.sid as u32);
        let _ = self.app_tx.send(f.split().freeze());
    }
}

// ── 驱动线程（同步 kcp 状态机） ──

/// kcp crate 的 Output 要求实现 std::io::Write（udp 已 connect → write 即发送）。
/// 用 std socket：驱动线程无 Tokio 上下文，AsyncFd 系列不可用。
struct UdpOut(Arc<std::net::UdpSocket>);

impl std::io::Write for UdpOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.send(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0), // 丢弃本次，kcp 重传兜底
            Err(e) => Err(e),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_driver(
    sock: Arc<std::net::UdpSocket>,
    conv: u32,
    mut app_rx: mpsc::UnboundedReceiver<Bytes>,
    side: Arc<SideRx>,
    closed: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut kcp = kcp::Kcp::new(conv, UdpOut(Arc::clone(&sock)));
        // turbo 参数（RustDesk new_turbo 同款）
        kcp.set_nodelay(true, 10, 2, true);
        let _ = kcp.set_mtu(1200 - 28 + KCP_OVERHEAD);

        let mut buf = vec![0u8; 4096];
        let mut base_ms: u32 = 0;
        let mut last = Instant::now();
        loop {
            if closed.load(Ordering::Relaxed) {
                return;
            }
            // 1) UDP 入包 → kcp.input
            while let Ok((n, _from)) = sock.recv_from(&mut buf) { // nonblocking → 立即返回
                let _ = 0;
                let _ = kcp.input(&buf[..n]);
            }
            // 2) 应用分帧 → kcp.send
            while let Ok(frame) = app_rx.try_recv() {
                let _ = kcp.send(&frame);
                side.stats.bytes_sent.fetch_add(frame.len() as u64, Ordering::Relaxed);
            }
            // 3) kcp 收 → 分帧分发
            while let Ok(size) = kcp.peeksize() {
                if size > buf.len() {
                    buf.resize(size, 0);
                }
                let n = match kcp.recv(&mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                side.stats.bytes_recv.fetch_add(n as u64, Ordering::Relaxed);
                demux(&buf[..n], &side);
            }
            // 4) 10ms turbo 节拍 update + flush
            if last.elapsed() >= Duration::from_millis(10) {
                base_ms = base_ms.wrapping_add(10);
                let _ = kcp.update(base_ms);
                last = Instant::now();
            }
            let _ = kcp.flush(); // 发送缓冲尽力推；失败由下次 tick 重试
            std::thread::sleep(Duration::from_millis(1));
        }
    });
}

/// 分帧分发（帧格式见模块文档）。
fn demux(frame: &[u8], side: &Arc<SideRx>) {
    if frame.is_empty() {
        return;
    }
    let ty = frame[0];
    match ty {
        T_DGRAM if frame.len() >= 5 => {
            let len = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize;
            if frame.len() >= 5 + len {
                let _ = side.dgram_tx.try_send(Bytes::copy_from_slice(&frame[5..5 + len]));
            }
        }
        T_OPEN if frame.len() >= 6 => {
            let kind = StreamKind::from_u8(frame[1]).unwrap_or(StreamKind::Generic);
            let sid = u32::from_be_bytes(frame[2..6].try_into().unwrap()) as u64;
            let (buf_tx, buf_rx) = mpsc::channel::<Bytes>(64);
            side.inbound_streams.lock().unwrap().insert(sid, buf_tx);
            side.pending_open.lock().unwrap().insert(sid, buf_rx);
            let _ = side.open_tx.try_send((sid, kind));
        }
        T_SDATA if frame.len() >= 9 => {
            let sid = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as u64;
            let len = u32::from_be_bytes(frame[5..9].try_into().unwrap()) as usize;
            if frame.len() >= 9 + len {
                if let Some(tx) = side.inbound_streams.lock().unwrap().get(&sid) {
                    if tx.try_send(Bytes::copy_from_slice(&frame[9..9 + len])).is_err() {
                        side.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        T_FIN if frame.len() >= 5 => {
            let sid = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as u64;
            side.inbound_streams.lock().unwrap().remove(&sid); // 丢发送端 → recv None
        }
        _ => {}
    }
}

// ── Transport 工厂 ──

fn make_caps() -> ChannelCaps {
    ChannelCaps {
        datagram: Some(DatagramCaps { max_size: 1152, lossy_ok: false }), // 可靠：不许丢
        streams: Some(StreamCaps { reliable_ordered: true, bidi: true, max_concurrent: 1 }),
        congestion: vec![CongestionKind::Cubic], // 语义标注：kcp 内建窗口
        mtu: 1200,
        multipath: false,
    }
}

fn make_side(sock: Arc<std::net::UdpSocket>, conv: u32, name: String) -> anyhow::Result<KcpChannel> {
    let stats = Arc::new(StatsReg {
        conn: name,
        start: Instant::now(),
        bytes_sent: AtomicU64::new(0),
        bytes_recv: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });
    let (dgram_tx, dgram_rx) = mpsc::channel(256);
    let (open_tx, open_rx) = mpsc::channel(16);
    let side = Arc::new(SideRx {
        dgram_tx,
        open_tx,
        inbound_streams: Arc::new(Mutex::new(HashMap::new())),
        pending_open: Arc::new(Mutex::new(HashMap::new())),
        stats: Arc::clone(&stats),
    });
    let (app_tx, app_rx) = mpsc::unbounded_channel();
    let closed = Arc::new(AtomicBool::new(false));
    let chan = KcpChannel {
        app_tx: app_tx.clone(),
        rx: Arc::clone(&side),
        dgram_rx: tokio::sync::Mutex::new(dgram_rx),
        open_rx: tokio::sync::Mutex::new(open_rx),
        stats,
        caps: make_caps(),
        path: PathKind::Direct,
        closed: Arc::clone(&closed),
    };
    spawn_driver(sock, conv, app_rx, side, closed);
    Ok(chan)
}

/// KCP harness 对：两个真实 UDP socket 在 127.0.0.1 互通。
pub struct KcpPairTransport {
    caps: ChannelCaps,
    pending_remote: tokio::sync::Mutex<Option<Box<dyn Channel>>>,
}

impl KcpPairTransport {
    pub fn new() -> Self {
        Self { caps: make_caps(), pending_remote: tokio::sync::Mutex::new(None) }
    }

    pub async fn take_remote(&self) -> Option<Box<dyn Channel>> {
        self.pending_remote.lock().await.take()
    }
}

impl Default for KcpPairTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for KcpPairTransport {
    fn offers(&self) -> &ChannelCaps {
        &self.caps
    }

    async fn establish(
        &self,
        _paths: Vec<PathHandle>,
        _bus: Arc<EventBus>,
    ) -> anyhow::Result<Box<dyn Channel>> {
        // conv 两端必须一致（KCP 会话标识）；socket 互连顺序：B 绑定 → A 绑定 →
        // 互连（真实部署这些地址来自打洞阶段）。
        let conv = 0x5B00_0001u32;
        let a_std = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let addr_a = a_std.local_addr()?;
        let b_std = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let addr_b = b_std.local_addr()?;
        a_std.connect(addr_b)?;
        b_std.connect(addr_a)?;
        for s in [&a_std, &b_std] {
            s.set_nonblocking(true)?;
        }
        let a_sock = Arc::new(a_std);
        let b_sock = Arc::new(b_std);

        let chan_a = make_side(a_sock, conv, format!("kcp-{}", addr_a.port()))?;
        let chan_b = make_side(b_sock, conv, format!("kcp-{}", addr_b.port()))?;
        *self.pending_remote.lock().await = Some(Box::new(chan_b));
        Ok(Box::new(chan_a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (Box<dyn Channel>, Box<dyn Channel>) {
        let t = KcpPairTransport::new();
        let a: Box<dyn Channel> = t.establish(Vec::new(), Arc::new(EventBus::new())).await.unwrap();
        let b = t.take_remote().await.unwrap();
        (a, b)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kcp_数据报往返() {
        let (a, b) = setup().await;
        a.send_dgram(Bytes::from_static(b"ping")).await.unwrap();
        for _ in 0..100 {
            if let Ok(Ok(got)) = tokio::time::timeout(Duration::from_millis(100), b.recv_dgram()).await {
                assert_eq!(&got[..], b"ping");
                a.close();
                return;
            }
        }
        panic!("KCP 数据报 10s 内未到达");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kcp_流往返() {
        let (a, b) = setup().await;
        let mut s = a.open_stream(StreamKind::Terminal).await.unwrap();
        s.send(b"hello-kcp").await.unwrap();
        for _ in 0..100 {
            if let Ok(Ok((_k, mut rs))) =
                tokio::time::timeout(Duration::from_millis(100), b.accept_stream()).await
            {
                let got = rs.recv().await.unwrap();
                assert_eq!(&got[..], b"hello-kcp");
                return;
            }
        }
        panic!("KCP 流 10s 内未建立");
    }
}
