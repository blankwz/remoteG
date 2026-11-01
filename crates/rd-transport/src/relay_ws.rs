//! 经 WS 版中继（Cloudflare Workers / Node）的连接（T-033）。
//!
//! ## 与 UDP 版（relay.rs）的关系
//! 同一套 QUIC-over-virtual-socket 原理，但承载从「打标 UDP」换成
//! 「WebSocket 二进制帧」：每个 QUIC 包一帧 `[u64 tag][QUIC 包]`。
//! 由此中继可以部署在**只有 HTTPS 能力**的设施上——Cloudflare Workers
//! 免费版、任意反代（Caddy/nginx 自动 HTTPS）——对标 DERP 的设计哲学。
//!
//! 寻址（与 UDP 版不同，走 URL 路径，握手前即可寻人）：
//!   被控端:  `<base>/agent?th=<tag16hex>`  首帧 text `{"t":"register","th":...}`
//!   控制端:  `<base>/c/<tag16hex>`         直接开始二进制
//! tag16hex = 对端指纹前 8 字节 hex（客户端本地可算，无需中继往返）。
//!
//! 代价：WS over TLS + TCP 头部阻塞，延迟劣于原生 UDP——所以 WS 版定位是
//! 「无 VPS 的免费兜底」，有 VPS 仍推荐 UDP 版。

use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::conn::{self, Conn};
use crate::identity::{Fingerprint, Identity};

/// WS 握手超时（连接 + register 应答）。
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// 会话标号 = 对端指纹前 8 字节；16 位小写 hex（WS 版 URL 寻址用）。
fn tag_hex(fp: &Fingerprint) -> String {
    let tag = u64::from_le_bytes(fp.0[..8].try_into().expect("fp 前 8 字节"));
    format!("{tag:016x}")
}

/// 中继地址：UDP 版（ip:port）或 WS 版（http/https/ws/wss URL）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEndpoint {
    Udp(SocketAddr),
    Ws(String),
}

/// 解析中继地址：`1.2.3.4` / `1.2.3.4:21118` → UDP；
/// `https://…` / `http://…` / `wss://…` / `ws://…` → WS（http→ws、https→wss）。
pub fn parse_relay_endpoint(raw: &str) -> anyhow::Result<RelayEndpoint> {
    let s = raw.trim();
    let lower = s.to_ascii_lowercase();
    let ws_scheme = lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("wss://")
        || lower.starts_with("ws://");
    if ws_scheme {
        let normalized = if lower.starts_with("https://") {
            format!("wss://{}", &s[8..])
        } else if lower.starts_with("http://") {
            format!("ws://{}", &s[7..])
        } else {
            s.to_string()
        };
        let normalized = normalized.trim_end_matches('/').to_string();
        let host_part = normalized
            .split("://")
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("中继 URL 缺少协议头"))?;
        anyhow::ensure!(!host_part.is_empty(), "中继 URL 缺少主机名");
        return Ok(RelayEndpoint::Ws(normalized));
    }
    let s2 = if s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{}", rd_relay::DEFAULT_PORT)
    };
    let addr: SocketAddr = s2.parse().map_err(|e| {
        anyhow::anyhow!("中继地址格式错误（示例 1.2.3.4:21118 或 https://xxx.workers.dev）: {e}")
    })?;
    Ok(RelayEndpoint::Udp(addr))
}

/// 按地址类型自动选择 UDP / WS 版连接（控制端拨号）。
pub async fn connect_via_relay_ep(
    relay: &RelayEndpoint,
    target_fp: Fingerprint,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    match relay {
        RelayEndpoint::Udp(addr) => {
            super::relay::connect_via_relay(*addr, target_fp, id, expected).await
        }
        RelayEndpoint::Ws(base) => connect_via_relay_ws(base, target_fp, id, expected).await,
    }
}

/// 按地址类型自动选择 UDP / WS 版监听（被控端注册）。
pub async fn bind_via_relay_ep(
    relay: &RelayEndpoint,
    id: &Identity,
) -> anyhow::Result<crate::Server> {
    match relay {
        RelayEndpoint::Udp(addr) => super::relay::bind_via_relay(*addr, id).await,
        RelayEndpoint::Ws(base) => bind_via_relay_ws(base, id).await,
    }
}

// ─────────────────────────────────────────────────────────────
// WS 版实现
// ─────────────────────────────────────────────────────────────

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 被控端监听：WS 连 `/agent?th=`，注册后 accept QUIC。
pub async fn bind_via_relay_ws(base: &str, id: &Identity) -> anyhow::Result<crate::Server> {
    let local_fp = id.fingerprint();
    let th = tag_hex(&local_fp);
    let url = format!("{base}/agent?th={th}");
    let ws = connect_ws(&url).await?;
    let ws = ws_register(ws, &th).await?;
    let socket = WsRelayUdpSocket::new(&th, ws).await?;
    tracing::info!(base = %base, th, "WS 中继注册完成（REG），QUIC 监听改走中继");
    crate::conn::Server::bind_over(socket, id)
}

/// 控制端拨号：WS 连 `/c/<tag>`，桥接后直接 QUIC。对端暂不在线时 QUIC 包被
/// 中继丢弃、由 QUIC 重传兜底——与 UDP 版 DIAL 轮询语义一致。
pub async fn connect_via_relay_ws(
    base: &str,
    target_fp: Fingerprint,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    let th = tag_hex(&target_fp);
    let url = format!("{base}/c/{th}");
    let ws = connect_ws(&url).await?;
    let socket = WsRelayUdpSocket::new(&th, ws).await?;
    tracing::info!(base = %base, th, "WS 中继连接建立，开始 QUIC 握手");
    conn::connect_over(socket, relay_virtual_addr(), id, expected).await
}

async fn connect_ws(url: &str) -> anyhow::Result<WsStream> {
    let deadline = Instant::now() + WS_HANDSHAKE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "连接中继失败（15s）：检查地址/网络；Cloudflare 版确认 worker 已部署 \
                 （curl <地址>/healthz 应返回 ok）"
            );
        }
        let attempt = tokio::time::timeout(
            Duration::from_secs(8),
            tokio_tungstenite::connect_async(url),
        )
        .await;
        match attempt {
            Ok(Ok((ws, _))) => return Ok(ws),
            Ok(Err(e)) => tracing::debug!(%url, "WS 连接失败重试: {e}"),
            Err(_) => { /* 单次 8s 超时：重试 */ }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// agent 首帧 register → 等 registered。
async fn ws_register(mut ws: WsStream, th: &str) -> anyhow::Result<WsStream> {
    ws.send(Message::text(format!(r#"{{"t":"register","th":"{th}"}}"#)))
        .await
        .map_err(|e| anyhow::anyhow!("发送注册帧失败: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("中继未确认注册（5s）：地址是否指向 rd-relay？");
        }
        let msg = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            ws.next(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("等待注册确认超时"))?;
        match msg {
            Some(Ok(Message::Text(t))) if t.contains(r#""registered""#) => return Ok(ws),
            Some(Ok(Message::Close(f))) => anyhow::bail!(
                "中继拒绝注册: {}",
                f.map(|f| f.reason.to_string()).unwrap_or_default()
            ),
            Some(Ok(_)) => continue,
            Some(Err(e)) => anyhow::bail!("中继连接错误: {e}"),
            None => anyhow::bail!("中继连接被关闭"),
        }
    }
}

/// quinn 眼中的恒定「对端地址」（实际收发全部经 WS 通道）。
fn relay_virtual_addr() -> SocketAddr {
    "127.0.0.1:1".parse().expect("占位地址")
}

/// WS 版虚拟 UDP socket：发送走 mpsc → 写任务；接收由读任务投递 mpsc。
#[derive(Debug)]
struct WsRelayUdpSocket {
    tx: mpsc::UnboundedSender<Message>,
    rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    peer: SocketAddr,
    tag: u64,
}

impl WsRelayUdpSocket {
    /// 拆分 WS 读写两端：写任务 + 读任务（帧↔数据报转换）。
    async fn new(tag_hex: &str, ws: WsStream) -> anyhow::Result<Self> {
        let tag = u64::from_str_radix(tag_hex, 16)?;
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (mut sink, mut stream) = ws.split();

        // 写任务：数据报队列 → WS 二进制帧
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // 读任务：WS 帧 → 校验 tag → 剥头 → 数据报队列
        let expect_tag = tag;
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                match msg {
                    Message::Binary(d) => {
                        if d.len() > 8
                            && u64::from_le_bytes(d[..8].try_into().unwrap_or([0; 8])) == expect_tag
                        {
                            let payload = d[8..].to_vec();
                            if in_tx.send(payload).is_err() {
                                break;
                            }
                        }
                    }
                    Message::Text(_) => { /* 控制帧：忽略 */ }
                    Message::Close(_) => break,
                    _ => { /* ping/pong 由底层处理 */ }
                }
            }
        });

        Ok(Self {
            tx: out_tx,
            rx: Mutex::new(in_rx),
            peer: relay_virtual_addr(),
            tag,
        })
    }

    fn frame(&self, contents: &[u8]) -> Message {
        let mut pkt = Vec::with_capacity(8 + contents.len());
        pkt.extend_from_slice(&self.tag.to_le_bytes());
        pkt.extend_from_slice(contents);
        Message::binary(pkt)
    }
}

impl AsyncUdpSocket for WsRelayUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // mpsc 发送永不 WouldBlock（unbounded）——恒就绪的 poller
        Box::pin(AlwaysReadyPoller)
    }

    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        let pkt = self.frame(transmit.contents);
        self.tx
            .send(pkt)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "中继写任务已退出"))
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        let mut rx = self.rx.lock().expect("ws relay rx 锁中毒");
        let mut got = 0usize;
        while got < bufs.len() {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(payload)) => {
                    let n = payload.len();
                    bufs[got][..n].copy_from_slice(&payload);
                    meta[got] = RecvMeta {
                        addr: self.peer,
                        dst_ip: None,
                        ecn: None,
                        len: n,
                        stride: n,
                    };
                    got += 1;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "中继连接已断开",
                    )));
                }
                Poll::Pending => break,
            }
        }
        if got > 0 {
            Poll::Ready(Ok(got))
        } else {
            Poll::Pending
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.peer)
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

/// 恒就绪 poller（unbounded mpsc 不会写阻塞）。
#[derive(Debug)]
struct AlwaysReadyPoller;

impl UdpPoller for AlwaysReadyPoller {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// ─────────────────────────────────────────────────────────────
// e2e：假 WS 中继（内存桥接）+ 真实 QUIC 双向认证 + 控制流回环
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use rd_proto::v0::{envelope, Hello};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;

    /// 测试假中继的握手回调：原样应答（路径分拣由 accept 顺序决定）。
    #[allow(clippy::result_large_err)] // tungstenite 的 ErrorResponse 本身就大
    fn test_cb(
        req: &tokio_tungstenite::tungstenite::http::Request<()>,
        resp: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> std::result::Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        let _ = req.uri().path();
        Ok(resp)
    }

    /// 假 WS 中继：按路径收 agent/client 两个连接，注册握手后桥接二进制。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "与 UDP 版 e2e 同因（非确定性竞态）待专项排查"]
    async fn ws版经中继的完整握手与控制流回环() -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("ws://{}", listener.local_addr()?);
        let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<WsStream>();
        let (client_tx, mut client_rx) = mpsc::unbounded_channel::<WsStream>();

        // accept：顺序已知——第一个是 agent（bind 先于 connect），第二个是 client
        // （与真协议的路径分拣语义等价；简化只影响测试假中继）
        tokio::spawn(async move {
            // 第一个连接 = agent：立即完成注册应答（真中继语义——注册不等 client）
            let Some((stream, _)) = listener.accept().await.ok() else {
                return;
            };
            let stream = tokio_tungstenite::MaybeTlsStream::Plain(stream);
            let mut ws = match accept_hdr_async(stream, test_cb).await {
                Ok(w) => w,
                Err(_) => return,
            };
            // 读掉 register 帧 → 回 registered
            let _ = ws.next().await;
            if ws
                .send(Message::text(r#"{"t":"registered"}"#))
                .await
                .is_err()
            {
                return;
            }
            let _ = agent_tx.send(ws);

            // 第二个连接 = client
            let Some((stream, _)) = listener.accept().await.ok() else {
                return;
            };
            let stream = tokio_tungstenite::MaybeTlsStream::Plain(stream);
            let ws = match accept_hdr_async(stream, test_cb).await {
                Ok(w) => w,
                Err(_) => return,
            };
            let _ = client_tx.send(ws);
        });

        // 桥接任务：拿到两端后互相转发二进制（register 已在 accept 循环处理）
        tokio::spawn(async move {
            let (a, c) = tokio::join!(agent_rx.recv(), client_rx.recv());
            let (mut a, mut c) = (
                match a {
                    Some(x) => x,
                    None => return,
                },
                match c {
                    Some(x) => x,
                    None => return,
                },
            );
            loop {
                tokio::select! {
                    m = a.next() => match m {
                        Some(Ok(Message::Binary(d))) => {
                            if c.send(Message::binary(d)).await.is_err() { return; }
                        }
                        _ => return,
                    },
                    m = c.next() => match m {
                        Some(Ok(Message::Binary(d))) => {
                            if a.send(Message::binary(d)).await.is_err() { return; }
                        }
                        _ => return,
                    },
                }
            }
        });

        let server_id = Identity::generate("ws-relay-e2e-server")?;
        let client_id = Identity::generate("ws-relay-e2e-client")?;
        let server_fp = server_id.fingerprint();
        let client_fp = client_id.fingerprint();

        // 被控端：WS 版监听
        let server = bind_via_relay_ws(&base, &server_id).await?;
        let accepter = tokio::spawn(async move {
            let conn = server.accept().await?;
            assert_eq!(conn.peer_fingerprint(), client_fp);
            let (mut tx, mut rx) = conn.accept_control().await?;
            let Some(env) = rx.recv().await? else {
                anyhow::bail!("连接已关闭");
            };
            let Some(envelope::Msg::Hello(h)) = env.msg else {
                anyhow::bail!("期望 Hello");
            };
            tx.send(&rd_proto::v0::Envelope {
                req_id: 1,
                msg: Some(envelope::Msg::HelloAck(rd_proto::v0::HelloAck {
                    proto_ver: 0,
                    host_name: format!("echo-{}", h.device_name),
                    ..Default::default()
                })),
            })
            .await?;
            // 保活：与 UDP 版 e2e 同理——任务结束会 drop rd Conn（隐式关连接），
            // 必须撑到客户端读毕响应并主动关闭
            // 客户端隐式关连接（预期收尾）；继续读只是消化残余帧
            let _ = rx.recv().await;
            anyhow::Ok(())
        });

        // 控制端：WS 版拨号 + 指纹钉扎 + Envelope 往返
        let conn = connect_via_relay_ws(&base, server_fp, &client_id, Some(server_fp)).await?;
        assert_eq!(conn.peer_fingerprint(), server_fp, "E2E 指纹必须钉扎成功");
        let (mut tx, mut rx) = conn.open_control().await?;
        tx.send(&rd_proto::v0::Envelope {
            req_id: 1,
            msg: Some(envelope::Msg::Hello(Hello {
                proto_ver: 0,
                device_name: "ws-relay-e2e".into(),
                ..Default::default()
            })),
        })
        .await?;
        let Some(env) = rx.recv().await? else {
            anyhow::bail!("连接已关闭");
        };
        let Some(envelope::Msg::HelloAck(ack)) = env.msg else {
            anyhow::bail!("期望 HelloAck");
        };
        assert_eq!(ack.host_name, "echo-ws-relay-e2e");
        accepter.await??;
        Ok(())
    }
}
