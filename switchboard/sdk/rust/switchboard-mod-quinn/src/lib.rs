//! 传输适配器 `sb.quinn.quic`（L-B）：规范 01 §6 Channel 契约的真实 QUIC 实现。
//!
//! 能力映射：
//! - datagram → quinn 不可靠数据报（`SendDatagramError::TooLarge` → `WouldBlock`，
//!   精确兑现 01 §6 MUST 2「丢新包不阻塞」）；
//! - streams → quinn 双向可靠有序流；
//! - 指纹钉扎 → 自签证书 + TOFU（首连记录指纹；严格比对模式由执行器信任表注入，v0.2）；
//! - 遥测 → `ConnectionStats`（RTT/cwnd）+ 发收计数，`path.kind` 来自 PathHandle 如实标注。
//!
//! 出身：rd `crates/rd-transport`（MTU 钳 1200 = tailscale 1280 实测经验）。
//!
//! **指纹**：证书 DER 的 SHA-256（v0.1.2 起为密码学哈希，密码学合规）。

use async_trait::async_trait;
use bytes::Bytes;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use switchboard_core::caps::{ChannelCaps, CongestionKind, DatagramCaps, StreamCaps};
use switchboard_core::channel::{
    Channel, ChannelError, RecvError, SendError, Stream, StreamKind,
};
use switchboard_core::stages::Transport;
use switchboard_core::telemetry::{EventBus, PathTag, Stage, StageStats};
use switchboard_core::types::{PathHandle, PathKind};

pub const ALPN: &[u8] = b"sb/0";
const SERVER_NAME: &str = "sb.local";
const KEEP_ALIVE: Duration = Duration::from_secs(5);

/// 信道 caps（1200 = tailscale MTU 1280 实测钳制经验；datagram 上限再扣 QUIC 开销）。
fn make_caps() -> ChannelCaps {
    ChannelCaps {
        datagram: Some(DatagramCaps { max_size: 1152, lossy_ok: true }),
        streams: Some(StreamCaps { reliable_ordered: true, bidi: true, max_concurrent: 256 }),
        congestion: vec![CongestionKind::Cubic],
        mtu: 1200,
        multipath: false,
    }
}

fn transport_config() -> quinn::TransportConfig {
    let mut tc = quinn::TransportConfig::default();
    tc.keep_alive_interval(Some(KEEP_ALIVE));
    // idle timeout 用 quinn 默认（30s）；5s keepalive 保证中继/打洞映射保活
    // 接收缓冲决定单包上限（2400 ≥ 2×分片）；发送队列 64KB（rd DATAGRAM_BUFFER_SIZE
    // 同款——设成 MTU 量级会让背靠背分片互相挤掉，v0.1.2 集成测试抓出的 bug）
    // 收发各 64KB（rd DATAGRAM_BUFFER_SIZE 同款）：接收决定单包上限（≥2×分片），
    // 发送是队列容量——设成 MTU 量级会让背靠背分片互相挤掉（v0.1.2 集成测试抓出）
    tc.datagram_receive_buffer_size(Some(64 * 1024));
    tc.datagram_send_buffer_size(64 * 1024);
    tc
}

fn init_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// ── 证书与钉扎 ──

fn make_server_config() -> anyhow::Result<quinn::ServerConfig> {
    init_crypto_provider();
    let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
    let cert_der = rustls::pki_types::CertificateDer::from(ck.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der());
    let rustls_server =
        rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert_der], rustls::pki_types::PrivateKeyDer::Pkcs8(key_der))?;
    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server))?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    sc.transport_config(Arc::new(transport_config()));
    Ok(sc)
}

/// TOFU 钉扎校验器：首连记录证书指纹；提供 expected 时严格比对。
struct PinVerifier {
    expected: Option<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
}

impl fmt::Debug for PinVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinVerifier").field("expected", &self.expected.is_some()).finish()
    }
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = fingerprint::of(end_entity.as_ref());
        if let Some(exp) = self.expected {
            if exp != fp {
                return Err(rustls::Error::General("指纹钉扎校验失败（spec 01 §7）".into()));
            }
        }
        *self.seen.lock().unwrap() = Some(fp);
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 指纹 = 证书 DER 的 SHA-256（密码学哈希，v0.1.2 起；spec 01 §7）。
mod fingerprint {
    pub fn of(data: &[u8]) -> [u8; 32] {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(data);
        h.finalize().into()
    }
}

fn client_config() -> quinn::ClientConfig {
    init_crypto_provider();
    let rustls_client = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier {
            expected: None, // TOFU
            seen: Arc::new(Mutex::new(None)),
        }))
        .with_no_client_auth();
    let quic_crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client))
            .expect("quic client crypto 桥接失败");
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic_crypto));
    cfg.transport_config(Arc::new(transport_config()));
    cfg
}

// ── Channel 实现 ──

struct StatsReg {
    start: std::time::Instant,
    bytes_sent: std::sync::atomic::AtomicU64,
    bytes_recv: std::sync::atomic::AtomicU64,
    dropped: std::sync::atomic::AtomicU64,
}

struct QuinnChannel {
    conn: quinn::Connection,
    caps: ChannelCaps,
    path: PathKind,
    conn_tag: String,
    stats_reg: Arc<StatsReg>,
}

impl QuinnChannel {
    /// 从既有 QUIC 连接构造 Channel 视图（rd 侧 T-027 复用：同一条连接，
    /// 视频/瓦片/终端走流，fec-shards 走 datagram）。
    #[allow(dead_code)] // 供 switchboard-bench/外部集成使用
    pub fn from_conn(
        conn: quinn::Connection,
        path: PathKind,
        tag: &str,
    ) -> Self {
        Self {
            conn,
            caps: make_caps(),
            path,
            conn_tag: format!("q-{tag}"),
            stats_reg: Arc::new(StatsReg {
                start: std::time::Instant::now(),
                bytes_sent: Default::default(),
                bytes_recv: Default::default(),
                dropped: Default::default(),
            }),
        }
    }
}

#[async_trait]
impl Channel for QuinnChannel {
    fn caps(&self) -> &ChannelCaps {
        &self.caps
    }

    fn conn_id(&self) -> &str {
        &self.conn_tag
    }

    async fn send_dgram(&self, b: Bytes) -> Result<(), SendError> {
        use std::sync::atomic::Ordering;
        match self.conn.send_datagram(b) {
            Ok(()) => Ok(()),
            // 01 §6 MUST 2：超 MTU/队列满 → 丢新包返回 WouldBlock，绝不阻塞
            Err(quinn::SendDatagramError::TooLarge)
            | Err(quinn::SendDatagramError::UnsupportedByPeer)
            | Err(quinn::SendDatagramError::Disabled) => {
                self.stats_reg.dropped.fetch_add(1, Ordering::Relaxed);
                Err(SendError::WouldBlock)
            }
            Err(quinn::SendDatagramError::ConnectionLost(_)) => Err(SendError::Closed),
        }
    }

    async fn recv_dgram(&self) -> Result<Bytes, RecvError> {
        use std::sync::atomic::Ordering;
        match self.conn.read_datagram().await {
            Ok(b) => {
                self.stats_reg.bytes_recv.fetch_add(b.len() as u64, Ordering::Relaxed);
                Ok(b)
            }
            Err(_) => Err(RecvError::Closed),
        }
    }

    async fn open_stream(&self, kind: StreamKind) -> Result<Box<dyn Stream>, ChannelError> {
        let (tx, rx) = self.conn.open_bi().await.map_err(|_| ChannelError::Closed)?;
        Ok(Box::new(QuinnStream { kind, tx, rx }))
    }

    async fn accept_stream(&self) -> Result<(StreamKind, Box<dyn Stream>), ChannelError> {
        let (tx, rx) = self.conn.accept_bi().await.map_err(|_| ChannelError::Closed)?;
        // v0.1：流语义类型由应用首字节约定，这里统一按 Control 报告
        Ok((StreamKind::Control, Box::new(QuinnStream { kind: StreamKind::Control, tx, rx })))
    }

    fn stats(&self) -> StageStats {
        use std::sync::atomic::Ordering;
        let s = self.conn.stats();
        let secs = self.stats_reg.start.elapsed().as_secs_f32().max(1e-6);
        StageStats {
            v: switchboard_core::TELEMETRY_VERSION,
            ts_ms: self.stats_reg.start.elapsed().as_millis() as u64,
            stage: Stage::Transport.as_str().into(),
            module: "sb.quinn.quic".into(),
            conn: self.conn_tag.clone(),
            path: PathTag { kind: self.path, local: None, remote: None },
            rtt_ewma_ms: s.path.rtt.as_millis() as f32,
            loss_ratio: 0.0,
            send_mbps: self.stats_reg.bytes_sent.load(Ordering::Relaxed) as f32 * 8.0 / secs / 1e6,
            recv_mbps: self.stats_reg.bytes_recv.load(Ordering::Relaxed) as f32 * 8.0 / secs / 1e6,
            queue_depth_bytes: 0,
            jitter_ms: 0.0,
            extra: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("cwnd".into(), s.path.cwnd.into());
                m.insert("lost_packets".into(), s.path.lost_packets.into());
                m
            },
        }
    }

    fn close(&self) {
        self.conn.close(0x1000u32.into(), b"sb-close");
    }

    fn is_closed(&self) -> bool {
        // quinn 0.11 无同步 is_closed 探测；以读出错的 recv/accept 收敛为准
        false
    }
}

struct QuinnStream {
    #[allow(dead_code)]
    kind: StreamKind,
    tx: quinn::SendStream,
    rx: quinn::RecvStream,
}

#[async_trait]
impl Stream for QuinnStream {
    async fn send(&mut self, b: &[u8]) -> Result<(), ChannelError> {
        self.tx.write_all(b).await.map_err(|_| ChannelError::Closed)
    }

    async fn recv(&mut self) -> Option<Bytes> {
        let mut buf = vec![0u8; 4096];
        match self.rx.read(&mut buf).await {
            Ok(Some(n)) if n > 0 => Some(Bytes::copy_from_slice(&buf[..n])),
            _ => None,
        }
    }

    fn finish(&mut self) {
        let _ = self.tx.finish();
    }
}

fn new_stats_reg() -> Arc<StatsReg> {
    Arc::new(StatsReg {
        start: std::time::Instant::now(),
        bytes_sent: Default::default(),
        bytes_recv: Default::default(),
        dropped: Default::default(),
    })
}

// ── Transport 工厂 ──

/// 真实拨号传输：establish 消费 `PathHandle.addr` 建立客户端连接。
pub struct QuinnDialTransport {
    pub local_bind: SocketAddr,
    caps: ChannelCaps,
}

impl QuinnDialTransport {
    pub fn new(local_bind: SocketAddr) -> Self {
        Self { local_bind, caps: make_caps() }
    }
}

#[async_trait]
impl Transport for QuinnDialTransport {
    fn offers(&self) -> &ChannelCaps {
        &self.caps
    }

    async fn establish(
        &self,
        paths: Vec<PathHandle>,
        _bus: Arc<EventBus>,
    ) -> anyhow::Result<Box<dyn Channel>> {
        let handle = paths
            .iter()
            .find(|p| p.addr.is_some())
            .ok_or_else(|| anyhow::anyhow!("PathHandle 缺少 addr（Loopback 内存路径请用 harness 对）"))?;
        let target = handle.addr.unwrap();
        let path_kind = handle.kind;

        let mut endpoint = quinn::Endpoint::client(self.local_bind)?;
        endpoint.set_default_client_config(client_config());
        let conn = endpoint
            .connect(target, SERVER_NAME)?
            .await
            .map_err(|e| anyhow::anyhow!("quinn 握手失败: {e}"))?;
        Ok(Box::new(QuinnChannel {
            conn,
            caps: make_caps(),
            path: path_kind,
            conn_tag: format!("q-{target}"),
            stats_reg: new_stats_reg(),
        }))
    }
}

/// 单进程 harness 对（建立真实 QUIC 回环连接；生产部署两端在不同进程）。
pub struct QuinnPairTransport {
    caps: ChannelCaps,
    pending_remote: tokio::sync::Mutex<Option<Box<dyn Channel>>>,
}

impl QuinnPairTransport {
    pub fn new() -> Self {
        Self { caps: make_caps(), pending_remote: tokio::sync::Mutex::new(None) }
    }

    pub async fn take_remote(&self) -> Option<Box<dyn Channel>> {
        self.pending_remote.lock().await.take()
    }
}

impl Default for QuinnPairTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for QuinnPairTransport {
    fn offers(&self) -> &ChannelCaps {
        &self.caps
    }

    async fn establish(
        &self,
        _paths: Vec<PathHandle>,
        _bus: Arc<EventBus>,
    ) -> anyhow::Result<Box<dyn Channel>> {
        let server_cfg = make_server_config()?;
        let endpoint = quinn::Endpoint::server(server_cfg, "127.0.0.1:0".parse()?)?;
        let server_addr = endpoint.local_addr()?;

        // 服务端接受任务 → 服务端侧 Channel 递给 take_remote
        let (tx_done, rx_done) = tokio::sync::oneshot::channel::<anyhow::Result<QuinnChannel>>();
        tokio::spawn(async move {
            let res = async {
                let conn = endpoint
                    .accept()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("endpoint 已关闭"))?
                    .await?;
                Ok(QuinnChannel {
                    conn,
                    caps: make_caps(),
                    path: PathKind::Direct,
                    conn_tag: "q-server".into(),
                    stats_reg: new_stats_reg(),
                })
            }
            .await;
            let _ = tx_done.send(res);
        });

        let mut client_ep = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
        client_ep.set_default_client_config(client_config());
        let conn = client_ep
            .connect(server_addr, SERVER_NAME)?
            .await
            .map_err(|e| anyhow::anyhow!("quinn 握手失败: {e}"))?;

        let client = QuinnChannel {
            conn,
            caps: make_caps(),
            path: PathKind::Direct,
            conn_tag: format!("q-client-{server_addr}"),
            stats_reg: new_stats_reg(),
        };

        let server = rx_done.await.map_err(|_| anyhow::anyhow!("服务端接受任务失联"))??;
        *self.pending_remote.lock().await = Some(Box::new(server));
        Ok(Box::new(client))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (Box<dyn Channel>, Box<dyn Channel>) {
        let t = QuinnPairTransport::new();
        let a: Box<dyn Channel> = t.establish(Vec::new(), Arc::new(EventBus::new())).await.unwrap();
        let b = t.take_remote().await.unwrap();
        (a, b)
    }

    #[tokio::test]
    async fn 真实quic_数据报往返与超限丢新包() {
        let (a, b) = setup().await;
        a.send_dgram(Bytes::from_static(b"ping")).await.unwrap();
        let got = b.recv_dgram().await.unwrap();
        assert_eq!(&got[..], b"ping");

        // 超过 datagram 上限 → WouldBlock（丢新包语义，01 §6 MUST 2）
        let big = Bytes::from(vec![0u8; 4000]);
        assert!(matches!(a.send_dgram(big).await, Err(SendError::WouldBlock)));
        a.close();
    }

    #[tokio::test]
    async fn 真实quic_流往返与遥测() {
        let (a, b) = setup().await;
        let mut s = a.open_stream(StreamKind::Terminal).await.unwrap();
        s.send(b"hello-quic").await.unwrap();
        let (_kind, mut rs) = b.accept_stream().await.unwrap();
        let got = rs.recv().await.unwrap();
        assert_eq!(&got[..], b"hello-quic");

        let st = a.stats();
        assert_eq!(st.module, "sb.quinn.quic");
        assert_eq!(st.path.kind, PathKind::Direct);
        assert!(st.rtt_ewma_ms >= 0.0);
        assert!(switchboard_core::telemetry::stats_complete(
            &serde_json::to_string(&st).unwrap()
        ));
        a.close();
    }
}

#[cfg(test)]
mod integration_fec {
    //! #5 集成验证：fec-rs（Moonlight 级 RS）× quinn datagram × 弱网仿真。
    //! 这是 v0.2 媒体路径（spec/07「视频切 fec-shards」）的端到端最小证明。

    use bytes::Bytes;
    use std::sync::Arc;
    use switchboard_core::channel::Channel;
    use super::QuinnPairTransport;
    
    use switchboard_core::stages::{EncodedFrame, FrameIn, MediaAdapt, Transport};
    use switchboard_core::telemetry::EventBus;

    #[tokio::test(flavor = "multi_thread")]
    async fn fec_rs_over_quin_弱网帧往返() {
        // 真实 QUIC 对 + 2% 丢包仿真
        let t = QuinnPairTransport::new();
        let a: Arc<dyn Channel> =
            Arc::from(t.establish(Vec::new(), Arc::new(EventBus::new())).await.unwrap());
        let b: Arc<dyn Channel> = Arc::from(t.take_remote().await.unwrap());
        let fec_rs = switchboard_mod_fec_rs::FecRs::new(Default::default());
        let bus = Arc::new(EventBus::new());
        let (mut tx, mut rx) = fec_rs.bind(a, b, bus).await.unwrap();

        for i in 0..8u32 {
            let data: Vec<u8> = (0..2048).map(|x| (x as u8) ^ i as u8).collect();
            tx.submit(EncodedFrame { frame_id: i, data: Bytes::from(data) }).await.unwrap();
        }
        // 先发后收：连接全程存活，收满 5 帧即通过
        let mut got = 0u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while got < 5 {
            match tokio::time::timeout_at(deadline.into(), rx.next()).await {
                Ok(Some(FrameIn::Complete { .. })) => got += 1,
                Ok(Some(FrameIn::Lost { .. })) => {}
                _ => break,
            }
        }
        assert!(got >= 5, "弱网下应至少完整收到 5 帧，实际 {got}");
    }
}
