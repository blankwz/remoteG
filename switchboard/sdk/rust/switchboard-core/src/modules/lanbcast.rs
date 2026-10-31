//! 参考发现 `sb.lan.udp-broadcast`（L-B）：LAN 广播发现（自 rd D-013 包1 移植）。
//!
//! 安全基线（rd discover.rs 同款，规范 01 §3 MUST）：只应答不主动发送；
//! 应答内容 = 连接握手本就公开的信息（主机名/指纹短 hex/端口）；发现 ≠ 信任。
//! 端口 21121：与 rd 自有 21119 分离，避免同机部署冲突。

use async_trait::async_trait;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;

use crate::stages::{DiscoverCaps, Discoverer, Scope};
use crate::types::{EndpointCandidate, Proto, SourceKind};

pub const SB_DISCOVER_PORT: u16 = 21121;
pub const QUERY: &[u8] = b"SB_DISCOVER";
pub const REPLY_PREFIX: &str = "SB_AGENT_v1";

/// 应答格式：`SB_AGENT_v1|主机名|指纹短hex|QUIC端口`。
pub fn build_reply(host: &str, fp_short_hex: &str, quic_port: u16) -> String {
    format!("{REPLY_PREFIX}|{host}|{fp_short_hex}|{quic_port}")
}

/// 解析应答 → 候选。
pub fn parse_reply(reply: &str, from: SocketAddr) -> Option<EndpointCandidate> {
    let mut it = reply.split('|');
    if it.next()? != REPLY_PREFIX {
        return None;
    }
    let _host = it.next()?;
    let _fp = it.next()?;
    let port: u16 = it.next()?.parse().ok()?;
    Some(EndpointCandidate {
        addr: SocketAddr::new(from.ip(), port),
        proto: Proto::Udp,
        source: SourceKind::Lan,
        cost: 1,
        ttl: Some(Duration::from_secs(30)),
    })
}

/// 被控端应答循环（阻塞至进程结束；绑定失败仅降级，不影响 QUIC 服务——rd 同款）。
pub async fn serve_responder(
    port: u16,
    host: String,
    fp_short_hex: String,
    quic_port: u16,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    tracing_free_loop(socket, host, fp_short_hex, quic_port).await
}

async fn tracing_free_loop(
    socket: UdpSocket,
    host: String,
    fp_short_hex: String,
    quic_port: u16,
) -> std::io::Result<()> {
    let mut buf = [0u8; 64];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        if &buf[..n] != QUERY {
            continue;
        }
        let reply = build_reply(&host, &fp_short_hex, quic_port);
        let _ = socket.send_to(reply.as_bytes(), peer).await;
    }
}

/// 控制端发现器：广播一轮并收集应答窗口内的候选。
pub struct LanBroadcast {
    pub port: u16,
    pub window_ms: u64,
}

impl Default for LanBroadcast {
    fn default() -> Self {
        Self {
            port: SB_DISCOVER_PORT,
            window_ms: 300,
        }
    }
}

#[async_trait]
impl Discoverer for LanBroadcast {
    fn caps(&self) -> DiscoverCaps {
        DiscoverCaps { scope: Scope::Lan }
    }

    async fn poll_once(&self) -> anyhow::Result<Vec<EndpointCandidate>> {
        let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await?;
        socket.set_broadcast(true)?;
        // Windows 允许 255.255.255.255；受限网络下静默失败为空集
        let bcast: SocketAddr = SocketAddr::from(([255, 255, 255, 255], self.port));
        let _ = socket.send_to(QUERY, bcast).await;

        let mut out = Vec::new();
        let mut buf = [0u8; 128];
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.window_ms);
        loop {
            let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
            if wait.is_zero() {
                break;
            }
            match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) => {
                    if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                        if let Some(c) = parse_reply(s, from) {
                            if !out.iter().any(|e: &EndpointCandidate| e.addr == c.addr) {
                                out.push(c);
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 应答编解码往返() {
        let r = build_reply("FAMILY-PC", "ab12cd34", 21118);
        let from: SocketAddr = "192.168.1.5:40000".parse().unwrap();
        let c = parse_reply(&r, from).unwrap();
        assert_eq!(c.addr, "192.168.1.5:21118".parse::<SocketAddr>().unwrap());
        assert_eq!(c.source, SourceKind::Lan);
        assert_eq!(c.cost, 1);
    }

    #[test]
    fn 非法应答拒绝() {
        let from: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(parse_reply("garbage", from).is_none());
        assert!(parse_reply("SB_AGENT_v1|x|y|notaport", from).is_none());
    }
}
