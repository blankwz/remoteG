//! QUIC 服务端:监听地址选择、绑定与 accept 循环(T-015)。
//!
//! 地址选择(05_api §1、11_security「暴露到公网」对策):
//! - `--listen <addr>`:显式覆盖(测试/调试);
//! - 配置 `listen_addrs` 非空:按配置绑定;
//! - 自动:绑定本机回环(同机控制/测试,不出网卡无暴露面)+ Tailscale 网段
//!   `100.64.0.0/10`;`allow_lan`(默认 true)时额外绑定私网地址(RFC1918,
//!   D-013:同 WiFi 直连是小白主路径,安全由配对码+信任列表兜底)。
//!   一个可用地址都没有则报错退出并提示安装 Tailscale。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::config::Config;
use crate::session::{self, AgentState};
use rd_transport::{Identity, Server};

/// accept 失败后的退避间隔(防坏地址打满日志)。
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// 解析最终监听地址列表:CLI `--listen` > 配置 `listen_addrs` > 自动发现。
pub fn resolve_listen_addrs(
    config: &Config,
    cli_listen: Option<SocketAddr>,
) -> anyhow::Result<Vec<SocketAddr>> {
    if let Some(addr) = cli_listen {
        return Ok(vec![addr]);
    }
    if !config.listen_addrs.is_empty() {
        return Ok(config
            .listen_addrs
            .iter()
            .map(|ip| SocketAddr::new(*ip, config.listen_port))
            .collect());
    }
    let ips = auto_ips(config.allow_lan)?;
    Ok(ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, config.listen_port))
        .collect())
}

/// 自动发现:回环必选(不出网卡,同机控制/测试用);Tailscale(CGNAT
/// `100.64.0.0/10`)与 `allow_lan` 时的 RFC1918 私网按接口枚举。
fn auto_ips(allow_lan: bool) -> anyhow::Result<Vec<IpAddr>> {
    let interfaces = if_addrs::get_if_addrs().context("枚举网络接口失败(自动监听地址需要)")?;
    let mut out = vec![IpAddr::from([127, 0, 0, 1])];
    for iface in interfaces {
        let IpAddr::V4(v4) = iface.ip() else {
            continue;
        };
        if iface.is_loopback() {
            continue;
        }
        if is_cg_nat(&v4) || (allow_lan && is_private(&v4)) {
            out.push(IpAddr::V4(v4));
        }
    }
    // 回环已必选,理论上不会走到;保留兜底以防极端环境
    if out.len() <= 1 {
        let hint = if allow_lan {
            "Tailscale(100.64.0.0/10)或私网地址"
        } else {
            "Tailscale 地址(100.64.0.0/10)"
        };
        anyhow::bail!(
            "未找到可监听的{hint}。请安装并登录 Tailscale,或在 agent.toml 的 \
             listen_addrs 中显式配置监听地址。"
        );
    }
    Ok(out)
}

/// Tailscale 网段:100.64.0.0 – 100.127.255.255(RFC6598 CGNAT)。
fn is_cg_nat(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// RFC1918 私网:10/8、172.16/12、192.168/16。
fn is_private(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

/// 在全部地址上绑定 QUIC 监听器。
pub fn bind_servers(addrs: &[SocketAddr], identity: &Identity) -> anyhow::Result<Vec<Server>> {
    addrs
        .iter()
        .map(|addr| Server::bind(*addr, identity).with_context(|| format!("监听 {addr} 失败")))
        .collect()
}

/// 为每个监听器启动 accept 循环;每个连接交给 [`session::handle_conn`]。
pub async fn serve_all(state: Arc<AgentState>, servers: Vec<Server>) {
    let mut tasks = Vec::with_capacity(servers.len());
    for server in servers {
        tasks.push(tokio::spawn(accept_loop(Arc::clone(&state), server)));
    }
    for task in tasks {
        let _ = task.await;
    }
}

async fn accept_loop(state: Arc<AgentState>, server: Server) {
    loop {
        match server.accept().await {
            Ok(conn) => {
                let peer_fp = conn.peer_fingerprint();
                tracing::info!(
                    peer = peer_fp.short_hex(),
                    "QUIC 连接已建立(双向证书认证通过)"
                );
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(err) = session::handle_conn(state, conn).await {
                        // 会话级失败不致命:记录后回到 accept 循环
                        tracing::debug!(peer = peer_fp.short_hex(), "会话结束: {err:#}");
                    }
                });
            }
            Err(err) => {
                tracing::error!("接受连接失败: {err:#}");
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
}
