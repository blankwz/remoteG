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
/// 绑定全部监听地址：裸 socket 先自己 bind 再包给 quinn（T-034 打洞配套），
/// 返回 (服务端, 裸 socket)——打洞探测与 QUIC 共用同一个 socket/端口。
/// 同步 bind std socket 后 `from_std` 注册到 tokio（调用方保证 runtime 上下文）。
pub fn bind_servers(
    addrs: &[SocketAddr],
    identity: &Identity,
) -> anyhow::Result<Vec<(Server, std::sync::Arc<tokio::net::UdpSocket>)>> {
    let mut out = Vec::with_capacity(addrs.len());
    for addr in addrs {
        let std_sock =
            std::net::UdpSocket::bind(addr).with_context(|| format!("绑定 {addr} 失败"))?;
        std_sock.set_nonblocking(true)?;
        let sock = std::sync::Arc::new(tokio::net::UdpSocket::from_std(std_sock)?);
        let server = Server::bind_socket(Arc::clone(&sock), identity)
            .with_context(|| format!("监听 {addr} 失败"))?;
        out.push((server, sock));
    }
    Ok(out)
}

/// [`bind_servers`] 的升级重启配套版：旧进程退出与端口释放之间有窗口期，
/// `AddrInUse` 时按 500ms 间隔重试（最长 ~15s，30 次）；其余错误照常上抛。
/// 正常启动路径零成本（一次成功即返回）；e2e 用随机端口不会触发重试。
pub fn bind_servers_with_retry(
    addrs: &[SocketAddr],
    identity: &Identity,
) -> anyhow::Result<Vec<(Server, std::sync::Arc<tokio::net::UdpSocket>)>> {
    const MAX_TRIES: usize = 30;
    const INTERVAL: Duration = Duration::from_millis(500);
    for attempt in 1..=MAX_TRIES {
        match bind_servers(addrs, identity) {
            Ok(v) => return Ok(v),
            Err(err) if is_addr_in_use(&err) && attempt < MAX_TRIES => {
                tracing::warn!(
                    attempt,
                    addrs = ?addrs,
                    "监听地址被占用（疑似升级重启窗口），500ms 后重试"
                );
                std::thread::sleep(INTERVAL);
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("重试循环必在 MAX_TRIES 内返回")
}

/// 错误链里是否有 AddrInUse（bind_servers 的 anyhow 链底层是 std io::Error）。
fn is_addr_in_use(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse)
    })
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

/// 动态虚拟网卡热监听巡检：当运行期新增 EasyTier (`10.126.x.x`)、Tailscale (`100.x.x.x`)
/// 或 ZeroTier (`10.147.x.x` / `192.168.x.x`) 网卡时，免重启自动绑定 `listen_port` 接受远控连接。
pub fn spawn_dynamic_interface_watcher(state: Arc<AgentState>) {
    if !state.config.listen_addrs.is_empty() {
        return;
    }
    let allow_lan = state.config.allow_lan;
    let port = state.config.listen_port;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let Ok(ips) = auto_ips(allow_lan) else {
                continue;
            };
            let existing: std::collections::HashSet<IpAddr> = state
                .listen_addrs
                .lock()
                .expect("listen_addrs 锁中毒")
                .iter()
                .map(|a| a.ip())
                .collect();
            for ip in ips {
                if existing.contains(&ip) {
                    continue;
                }
                let target = SocketAddr::new(ip, port);
                if let Ok(mut bound_pair) = bind_servers(&[target], &state.identity) {
                    if let Some((srv, _sock)) = bound_pair.pop() {
                        let actual = srv.local_addr();
                        tracing::info!(addr = %actual, "检测到新虚拟组网/局域网接口，已免重启自动绑定监听");
                        state
                            .listen_addrs
                            .lock()
                            .expect("listen_addrs 锁中毒")
                            .push(actual);
                        let st = Arc::clone(&state);
                        tokio::spawn(async move {
                            accept_loop(st, srv).await;
                        });
                    }
                }
            }
        }
    });
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
