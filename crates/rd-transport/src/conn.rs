//! 连接管理：Agent 监听 / 控制端拨号（quinn QUIC + rustls 双向自签认证，ALPN `rd/0`）。
//!
//! 拥塞控制用 quinn 默认（Cubic）；暴露 [`Conn::rtt`] / [`Conn::stats`] 供码率自适应。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use prost::Message;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{EndpointConfig, IdleTimeout, ReadExactError, TransportConfig, VarInt};
use quinn_proto::congestion::{Bbr, BbrConfig, Controller, ControllerFactory};
use rustls::pki_types::CertificateDer;

use crate::framing::{
    self, DataFrame, StreamKind, TileHeader, VideoHeader, LEN_PREFIX_LEN, MAX_CONTROL_FRAME_LEN,
    MAX_DATA_FRAME_LEN,
};
use crate::identity::{Fingerprint, Identity};
use crate::verify::PeerVerifier;

pub const ALPN: &[u8] = b"rd/0";
/// 证书钉扎体系下服务器名不参与校验（自定义验证器忽略 SNI），仅占位。
pub(crate) const SERVER_NAME: &str = "rd.local";
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// datagram 收发缓冲（槽位字节数；单包上限由 QUIC 首包 MTU 决定，约 1200B）。
const DATAGRAM_BUFFER_SIZE: usize = 64 * 1024;

/// Agent 端监听器。
#[derive(Debug)]
pub struct Server {
    endpoint: quinn::Endpoint,
}

impl Server {
    /// 在 `addr` 上开始监听（UDP/QUIC），要求双向证书认证。
    pub fn bind(addr: SocketAddr, id: &Identity) -> anyhow::Result<Self> {
        let endpoint = quinn::Endpoint::server(server_config(id)?, addr)?;
        Ok(Self { endpoint })
    }

    /// 经自定义 UDP socket（中继虚拟链路，T-032 见 relay.rs）建 QUIC 服务端。
    pub fn bind_over(socket: impl quinn::AsyncUdpSocket, id: &Identity) -> anyhow::Result<Self> {
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server_config(id)?),
            std::sync::Arc::new(socket),
            std::sync::Arc::new(quinn::TokioRuntime),
        )?;
        Ok(Self { endpoint })
    }

    /// 接受一个完成握手（双向认证）的连接。
    pub async fn accept(&self) -> anyhow::Result<Conn> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| anyhow::anyhow!("endpoint closed"))?;
        let conn = incoming.await?;
        Conn::new(conn)
    }

    /// 实际监听地址（bind 时端口为 0 则由系统分配）。
    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint
            .local_addr()
            .expect("endpoint is bound to a socket")
    }
}

/// 控制端拨号：连接 `addr`，`expected` 为 Some 时对端指纹必须匹配，否则握手失败。
pub async fn connect(
    addr: SocketAddr,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(client_config(id, expected)?);
    let conn = Conn::new(endpoint.connect(addr, SERVER_NAME)?.await?)?;
    verify_fingerprint(&conn, expected)?;
    Ok(conn)
}

/// 经自定义 UDP socket（中继虚拟链路）向 `remote`（虚拟地址即可）发起 QUIC。
pub(crate) async fn connect_over(
    socket: impl quinn::AsyncUdpSocket,
    remote: SocketAddr,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        std::sync::Arc::new(socket),
        std::sync::Arc::new(quinn::TokioRuntime),
    )?;
    endpoint.set_default_client_config(client_config(id, expected)?);
    let conn = Conn::new(endpoint.connect(remote, SERVER_NAME)?.await?)?;
    verify_fingerprint(&conn, expected)?;
    Ok(conn)
}

/// 测试专用：暴露传输配置（BBR + MTU 钳制，minimal_abstract_quinn 实验）。
#[doc(hidden)]
pub fn transport_config_for_test() -> quinn::TransportConfig {
    transport_config()
}

/// 测试专用：暴露服务端配置工厂（minimal_abstract_quinn 实验）。
#[doc(hidden)]
pub fn server_config_for_test(id: &Identity) -> anyhow::Result<quinn::ServerConfig> {
    server_config(id)
}

/// 测试专用：暴露客户端配置工厂。
#[doc(hidden)]
pub fn client_config_for_test(
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<quinn::ClientConfig> {
    client_config(id, expected)
}

/// 双向证书认证的服务端 TLS+QUIC 配置。
fn server_config(id: &Identity) -> anyhow::Result<quinn::ServerConfig> {
    let (certs, key) = id.rustls_identity()?;
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PeerVerifier::new(provider.signature_verification_algorithms, None);
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(std::sync::Arc::new(verifier))
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("invalid server identity: {e}"))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic_server = QuicServerConfig::try_from(std::sync::Arc::new(tls))
        .map_err(|e| anyhow::anyhow!("quic server config: {e}"))?;
    let mut config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(quic_server));
    config.transport_config(std::sync::Arc::new(transport_config()));
    Ok(config)
}

/// 指纹钉扎的客户端 TLS+QUIC 配置。
pub(crate) fn client_config(
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<quinn::ClientConfig> {
    let (certs, key) = id.rustls_identity()?;
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PeerVerifier::new(provider.signature_verification_algorithms, expected);
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
        .with_client_auth_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("invalid client identity: {e}"))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic_client = QuicClientConfig::try_from(std::sync::Arc::new(tls))
        .map_err(|e| anyhow::anyhow!("quic client config: {e}"))?;
    let mut config = quinn::ClientConfig::new(std::sync::Arc::new(quic_client));
    config.transport_config(std::sync::Arc::new(transport_config()));
    Ok(config)
}

/// 握手后二次确认（验证器已在 TLS 层拦截，这里是纵深防御）。
fn verify_fingerprint(conn: &Conn, expected: Option<Fingerprint>) -> anyhow::Result<()> {
    if let Some(expected) = expected {
        let actual = conn.peer_fingerprint();
        if actual != expected {
            conn.close(1, "fingerprint mismatch");
            anyhow::bail!("peer fingerprint mismatch: got {}", actual.hex());
        }
    }
    Ok(())
}

fn transport_config() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
    tc.max_idle_timeout(Some(
        IdleTimeout::try_from(MAX_IDLE_TIMEOUT).expect("20s fits VarInt"),
    ));
    tc.datagram_receive_buffer_size(Some(DATAGRAM_BUFFER_SIZE));
    tc.datagram_send_buffer_size(DATAGRAM_BUFFER_SIZE);
    // MTU 钳制（D-013 包5，2026-10-23 B 机日志实测）：Tailscale 接口 MTU
    // 1280，quinn 的 MTU 发现曾在 LAN 探测升 MTU，切 tailnet 路径后
    // sendmsg 报 WSAEMSGSIZE(10040) 重传降速。统一钳到 QUIC 保证值 1200
    // （IPv6 最小 MTU 1280 也安全）并关闭发现——跨路径确定性优先。
    tc.initial_mtu(1200);
    tc.min_mtu(1200);
    tc.mtu_discovery_config(None);
    // BBR（D-013 包5）：跨 DERP 中继/高丢包路径下 Cubic 对丢包过敏、吞吐
    // 崩塌（跨网实测 fps=1/RTT 1.2s）；BBR 以带宽估测驱动、丢包鲁棒。
    // A/B 回退：RD_BBR=0 临时切回 Cubic（05_api §6.4 候选项的对比开关）。
    if std::env::var("RD_BBR").as_deref() != Ok("0") {
        tc.congestion_controller_factory(Arc::new(BbrFactory(Arc::new(BbrConfig::default()))));
    }
    tc
}

/// BBR 拥塞控制器工厂（quinn 0.11 需经 [`ControllerFactory`] 注入）。
#[derive(Debug)]
struct BbrFactory(Arc<BbrConfig>);

impl ControllerFactory for BbrFactory {
    fn build(self: Arc<Self>, _now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Bbr::new(Arc::clone(&self.0), current_mtu))
    }
}

/// 一条已认证的 QUIC 连接（对端证书指纹在握手期提取）。
#[derive(Debug, Clone)]
pub struct Conn {
    conn: quinn::Connection,
    peer_fingerprint: Fingerprint,
}

impl Conn {
    /// 测试专用：暴露底层 quinn 连接（minimal_abstract_quinn 分层实验）。
    #[doc(hidden)]
    pub fn raw_conn(&self) -> quinn::Connection {
        self.conn.clone()
    }

    pub(crate) fn new(conn: quinn::Connection) -> anyhow::Result<Self> {
        // quinn 的 rustls 后端把对端证书链作为 Vec<CertificateDer> 放进 peer_identity
        let peer_fingerprint = conn
            .peer_identity()
            .ok_or_else(|| anyhow::anyhow!("no peer identity (handshake incomplete)"))?
            .downcast::<Vec<CertificateDer<'static>>>()
            .map_err(|_| anyhow::anyhow!("peer identity is not a certificate chain"))?
            .first()
            .map(|cert| Fingerprint::from_cert_der(cert.as_ref()))
            .ok_or_else(|| anyhow::anyhow!("peer sent empty certificate chain"))?;
        Ok(Self {
            conn,
            peer_fingerprint,
        })
    }

    /// 对端 socket 地址（loopback 自控自豁免「本机优先」门控用）。
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        self.conn.remote_address()
    }

    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.peer_fingerprint
    }

    /// 打开双向控制流（客户端调用；服务端对应 [`Conn::accept_control`]）。
    pub async fn open_control(&self) -> anyhow::Result<(ControlTx, ControlRx)> {
        let (tx, rx) = self.conn.open_bi().await?;
        Ok((ControlTx { stream: tx }, ControlRx { stream: rx }))
    }

    /// 接受对端打开的控制流（服务端调用；连接上第一个双向流）。
    ///
    /// 注意：QUIC 流的 STREAM 帧要等首字节写入才会发出，因此本方法会阻塞到
    /// 客户端写出控制流的第一帧；应用层需与客户端的 `open_control` + `send`
    /// 并发推进（各自 spawn / join），不要先串行等 accept 再让客户端发。
    pub async fn accept_control(&self) -> anyhow::Result<(ControlTx, ControlRx)> {
        let (tx, rx) = self.conn.accept_bi().await?;
        Ok((ControlTx { stream: tx }, ControlRx { stream: rx }))
    }

    /// 打开单向数据流（视频/瓦片：Agent→控制端）。
    pub async fn open_data(&self, kind: StreamKind) -> anyhow::Result<DataTx> {
        let mut stream = self.conn.open_uni().await?;
        stream
            .write_all(&[kind.as_u8()])
            .await
            .map_err(std::io::Error::from)?;
        Ok(DataTx { kind, stream })
    }

    /// 接受对端的单向数据流，返回 (种类, QUIC stream id, 接收端)。
    pub async fn accept_data(&self) -> anyhow::Result<(StreamKind, u64, DataRx)> {
        let mut stream = self.conn.accept_uni().await?;
        let kind = read_kind(&mut stream).await?;
        let stream_id = u64::from(stream.id());
        Ok((kind, stream_id, DataRx { kind, stream }))
    }

    /// 打开双向数据流（终端）。
    pub async fn open_bidi(&self, kind: StreamKind) -> anyhow::Result<(DataTx, DataRx)> {
        let (mut tx, rx) = self.conn.open_bi().await?;
        tx.write_all(&[kind.as_u8()])
            .await
            .map_err(std::io::Error::from)?;
        Ok((DataTx { kind, stream: tx }, DataRx { kind, stream: rx }))
    }

    /// 接受对端的双向数据流，返回 (种类, QUIC stream id, 发送端, 接收端)。
    pub async fn accept_bidi(&self) -> anyhow::Result<(StreamKind, u64, DataTx, DataRx)> {
        let (tx, mut rx) = self.conn.accept_bi().await?;
        let kind = read_kind(&mut rx).await?;
        let stream_id = u64::from(rx.id());
        Ok((
            kind,
            stream_id,
            DataTx { kind, stream: tx },
            DataRx { kind, stream: rx },
        ))
    }

    /// 发送 QUIC datagram（鼠标移动等低延迟输入）。
    pub fn send_datagram(&self, data: Bytes) -> anyhow::Result<()> {
        self.conn
            .send_datagram(data)
            .map_err(|e| anyhow::anyhow!("send_datagram: {e}"))
    }

    /// 读取一个 QUIC datagram。
    pub async fn read_datagram(&self) -> anyhow::Result<Bytes> {
        self.conn
            .read_datagram()
            .await
            .map_err(|e| anyhow::anyhow!("read_datagram: {e}"))
    }

    /// QUIC 连接句柄（Clone，轻量）：供 fec/datagram 适配器复用同一条连接
    /// （T-027：rd-switch 的 Channel 视图基于它实现）。
    pub fn quinn_handle(&self) -> quinn::Connection {
        self.conn.clone()
    }

    /// 平滑 RTT（码率自适应用）。
    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    /// 完整连接统计（含 cwnd / 丢包等，供码率自适应与调试）。
    pub fn stats(&self) -> quinn::ConnectionStats {
        self.conn.stats()
    }

    /// 优雅关闭连接。
    pub fn close(&self, code: u32, reason: &str) {
        self.conn.close(VarInt::from_u32(code), reason.as_bytes());
    }
}

async fn read_kind(stream: &mut quinn::RecvStream) -> anyhow::Result<StreamKind> {
    let mut kind = [0u8; 1];
    if !read_exact_or_eof(stream, &mut kind).await? {
        anyhow::bail!("data stream ended before kind byte");
    }
    StreamKind::try_from(kind[0]).map_err(|e| anyhow::anyhow!("{e}"))
}

/// 读满 `buf`；流在帧边界干净结束（读到 0 字节）返回 `Ok(false)`。
async fn read_exact_or_eof(stream: &mut quinn::RecvStream, buf: &mut [u8]) -> anyhow::Result<bool> {
    match stream.read_exact(buf).await {
        Ok(()) => Ok(true),
        Err(ReadExactError::FinishedEarly(0)) => Ok(false),
        Err(ReadExactError::FinishedEarly(n)) => {
            Err(anyhow::anyhow!("stream ended mid-frame ({n} bytes read)"))
        }
        Err(ReadExactError::ReadError(err)) => Err(std::io::Error::from(err).into()),
    }
}

/// 读满 `buf`，中途流结束视为截断错误。
async fn read_exact_full(stream: &mut quinn::RecvStream, buf: &mut [u8]) -> anyhow::Result<()> {
    match stream.read_exact(buf).await {
        Ok(()) => Ok(()),
        Err(ReadExactError::FinishedEarly(n)) => {
            Err(anyhow::anyhow!("stream ended mid-frame ({n} bytes read)"))
        }
        Err(ReadExactError::ReadError(err)) => Err(std::io::Error::from(err).into()),
    }
}

/// 控制流发送端：`u32 LE 长度 + protobuf Envelope`。
#[derive(Debug)]
pub struct ControlTx {
    stream: quinn::SendStream,
}

impl ControlTx {
    pub async fn send(&mut self, e: &rd_proto::v0::Envelope) -> anyhow::Result<()> {
        let frame = framing::encode_control_frame(e)?;
        let (len, body) = frame.split_at(LEN_PREFIX_LEN);
        self.stream
            .write_all(len)
            .await
            .map_err(std::io::Error::from)?;
        self.stream
            .write_all(body)
            .await
            .map_err(std::io::Error::from)?;
        Ok(())
    }
}

/// 控制流接收端；对端 finish 后返回 `Ok(None)`。
#[derive(Debug)]
pub struct ControlRx {
    stream: quinn::RecvStream,
}

impl ControlRx {
    pub async fn recv(&mut self) -> anyhow::Result<Option<rd_proto::v0::Envelope>> {
        let mut len_buf = [0u8; LEN_PREFIX_LEN];
        if !read_exact_or_eof(&mut self.stream, &mut len_buf).await? {
            return Ok(None);
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_CONTROL_FRAME_LEN {
            anyhow::bail!("control frame too large: {len} bytes");
        }
        let mut body = vec![0u8; len];
        read_exact_full(&mut self.stream, &mut body).await?;
        Ok(Some(
            rd_proto::v0::Envelope::decode(body.as_slice())
                .map_err(|e| anyhow::anyhow!("invalid envelope: {e}"))?,
        ))
    }
}

/// 数据流发送端（流开头已写 kind 字节）。
#[derive(Debug)]
pub struct DataTx {
    kind: StreamKind,
    stream: quinn::SendStream,
}

impl DataTx {
    /// QUIC stream id（填入 `OpenWindowAck.video_stream_id` / `TerminalOpened.stream_id`）。
    pub fn id(&self) -> u64 {
        u64::from(self.stream.id())
    }

    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    /// 发送一帧视频：25 字节帧头 + H.264 Annex-B AU。
    pub async fn send_video(&mut self, h: &VideoHeader, au: &[u8]) -> anyhow::Result<()> {
        if self.kind != StreamKind::Video {
            anyhow::bail!("send_video on a {} stream", self.kind.as_u8());
        }
        if au.len() > MAX_DATA_FRAME_LEN {
            anyhow::bail!("video frame too large: {} bytes", au.len());
        }
        self.stream
            .write_all(&framing::encode_video_header(h, au.len()))
            .await
            .map_err(std::io::Error::from)?;
        self.stream
            .write_all(au)
            .await
            .map_err(std::io::Error::from)?;
        Ok(())
    }

    /// 发送一帧瓦片：25 字节帧头 + 瓦片数据（`fmt=255` 表示失效清除）。
    pub async fn send_tile(&mut self, h: &TileHeader, d: &[u8]) -> anyhow::Result<()> {
        if self.kind != StreamKind::Tile {
            anyhow::bail!("send_tile on a {} stream", self.kind.as_u8());
        }
        if d.len() > MAX_DATA_FRAME_LEN {
            anyhow::bail!("tile frame too large: {} bytes", d.len());
        }
        self.stream
            .write_all(&framing::encode_tile_header(h, d.len()))
            .await
            .map_err(std::io::Error::from)?;
        self.stream
            .write_all(d)
            .await
            .map_err(std::io::Error::from)?;
        Ok(())
    }

    /// 发送一段终端原始字节（`u32 len + bytes`，双向通用）。
    pub async fn send_raw(&mut self, d: &[u8]) -> anyhow::Result<()> {
        if self.kind != StreamKind::Terminal {
            anyhow::bail!("send_raw is only valid on terminal streams");
        }
        let len = u32::try_from(d.len()).map_err(|_| anyhow::anyhow!("terminal frame > 4GiB"))?;
        self.stream
            .write_all(&len.to_le_bytes())
            .await
            .map_err(std::io::Error::from)?;
        self.stream
            .write_all(d)
            .await
            .map_err(std::io::Error::from)?;
        Ok(())
    }

    /// 发送 FIN（数据发完时调用；对端 [`DataRx::recv`] 将返回 `Ok(None)`）。
    pub fn finish(&mut self) -> anyhow::Result<()> {
        self.stream
            .finish()
            .map_err(|e| anyhow::anyhow!("finish: {e}"))
    }
}

/// 数据流接收端。
#[derive(Debug)]
pub struct DataRx {
    kind: StreamKind,
    stream: quinn::RecvStream,
}

impl DataRx {
    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    /// 读取下一帧；对端 finish 后返回 `Ok(None)`。
    pub async fn recv(&mut self) -> anyhow::Result<Option<DataFrame>> {
        match self.kind {
            StreamKind::Video => {
                let mut head = [0u8; framing::VIDEO_HEADER_LEN];
                if !read_exact_or_eof(&mut self.stream, &mut head).await? {
                    return Ok(None);
                }
                let (header, len) = framing::decode_video_header(&head)?;
                Ok(Some(DataFrame::Video(
                    header,
                    self.read_payload(len).await?,
                )))
            }
            StreamKind::Tile => {
                let mut head = [0u8; framing::TILE_HEADER_LEN];
                if !read_exact_or_eof(&mut self.stream, &mut head).await? {
                    return Ok(None);
                }
                let (header, len) = framing::decode_tile_header(&head)?;
                Ok(Some(DataFrame::Tile(header, self.read_payload(len).await?)))
            }
            StreamKind::Terminal => {
                let mut head = [0u8; LEN_PREFIX_LEN];
                if !read_exact_or_eof(&mut self.stream, &mut head).await? {
                    return Ok(None);
                }
                let len = u32::from_le_bytes(head);
                Ok(Some(DataFrame::Raw(self.read_payload(len).await?)))
            }
        }
    }

    async fn read_payload(&mut self, len: u32) -> anyhow::Result<Bytes> {
        let len = len as usize;
        if len > MAX_DATA_FRAME_LEN {
            anyhow::bail!("frame payload too large: {len} bytes");
        }
        let mut payload = vec![0u8; len];
        // 已读帧头，此处中途结束属异常截断
        read_exact_full(&mut self.stream, &mut payload).await?;
        Ok(Bytes::from(payload))
    }
}
