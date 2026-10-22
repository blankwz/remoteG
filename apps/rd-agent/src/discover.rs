//! UDP 局域网发现应答(21119/UDP,D-013 包1)。
//!
//! 控制端向本网段广播 `RD_DISCOVER`,本机应答
//! `RD_AGENT_v1|<主机名>|<指纹短hex>|<QUIC端口>`——让控制端免手填 IP。
//!
//! 安全考量:只应答不主动发送;应答内容(主机名/指纹短 hex/端口)均为
//! 连接握手本来就可见或无敏感性的公开信息,不因此授予任何权限;
//! 发现 ≠ 信任,配对仍需被控端托盘生成 6 位码。
//!
//! 端口用 21119 而非复用 QUIC 的 21118:同一 UDP 端口已被 quinn 绑定,
//! Windows 上第二个 socket 绑同端口收广播不可靠。

use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::session::AgentState;

/// 发现协议端口(D-013 包1;05_api §1 端口表同步)。
pub const DISCOVER_PORT: u16 = 21119;
/// 控制端广播的探测报文。
pub const QUERY: &[u8] = b"RD_DISCOVER";
/// 应答前缀。
const REPLY_PREFIX: &str = "RD_AGENT_v1";

/// 应答循环:阻塞至进程结束;绑定失败仅记录降级(不影响 QUIC 服务)。
pub async fn serve(state: Arc<AgentState>) {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], DISCOVER_PORT));
    let socket = match UdpSocket::bind(addr).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(
                port = DISCOVER_PORT,
                "UDP 发现端口绑定失败,发现功能停用: {err}"
            );
            return;
        }
    };
    tracing::info!(port = DISCOVER_PORT, "UDP 局域网发现应答就绪");

    let mut buf = [0u8; 64];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(err) => {
                tracing::debug!("UDP 发现接收失败: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        if buf[..n] != *QUERY {
            continue;
        }
        let reply = build_reply(&state);
        if let Err(err) = socket.send_to(reply.as_bytes(), peer).await {
            tracing::debug!(%peer, "UDP 发现应答发送失败: {err}");
        }
    }
}

/// 组装应答:`RD_AGENT_v1|主机名|指纹短hex|QUIC端口`。
fn build_reply(state: &AgentState) -> String {
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into());
    // QUIC 端口取任一监听地址的端口(bind 未完成时回落配置端口)
    let port = state
        .listen_addrs
        .lock()
        .expect("listen_addrs 锁中毒")
        .first()
        .map_or(crate::config::DEFAULT_LISTEN_PORT, |a| a.port());
    format!(
        "{REPLY_PREFIX}|{host}|{}|{port}",
        state.local_fp.short_hex()
    )
}
