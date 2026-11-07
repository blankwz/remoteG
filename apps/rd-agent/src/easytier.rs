//! EasyTier 组网托管（可选，agent.toml `[easytier]`）。
//!
//! 语义与原生中继同哲学：**失败仅降级**——节点起不来/中途退出只告警，
//! 直连与中继路径完全不受影响。宿主线程持有 [`EasyTierNode`]（Drop/Job
//! 双保险收割子进程），共享槽位暴露本机虚拟 IP 供直连候选采集（TUN 模式；
//! no_tun 模式由控制端侧转发，本机无需暴露候选）。
//!
//! 自身虚拟 IP 解析顺序：`virtual_ip` 静态配置 → `easytier-cli route --json`
//! 的 `node_info.ipv4_addr`（cli 缺失则放弃，仅告警）。

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;

use crate::config::EasyTierAgentConfig;
use switchboard_mod_easytier::cli_json::parse_self_ipv4;
use switchboard_mod_easytier::{EasyTierNetConfig, EasyTierNode, ForwardProto, ForwardRule};

/// 就绪后巡检间隔（进程意外退出时清空槽位）。
const WATCH_INTERVAL: Duration = Duration::from_secs(10);

/// EasyTier 托管句柄：虚拟 IP 槽位（候选采集消费）。
#[derive(Clone)]
pub struct EasyTierHost {
    /// 本机在 mesh 内的虚拟 IP（None = 未启用/未就绪/已退出）。
    pub virtual_ip: Arc<Mutex<Option<IpAddr>>>,
}

impl EasyTierHost {
    /// 当前虚拟 IP（槽位快照）。
    pub fn current_ip(&self) -> Option<IpAddr> {
        *self.virtual_ip.lock().expect("easytier ip 锁中毒")
    }

    /// 等待就绪（宿主启动含 easytier-core 冷启动，上限 `timeout`）。
    pub async fn wait_ready(&self, timeout: Duration) -> Option<IpAddr> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(ip) = self.current_ip() {
                return Some(ip);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// 解析转发规则串 `"对端虚拟IP:端口@本地端口"`（如 `"10.126.126.3:21118@21130"`）
/// → 本机回环 UDP 转发规则。格式非法返回 None（调用方跳过并告警）。
pub fn parse_forward_rule(s: &str) -> Option<ForwardRule> {
    let (dst, bind_port) = s.rsplit_once('@')?;
    let dst: std::net::SocketAddr = dst.trim().parse().ok()?;
    let bind_port: u16 = bind_port.trim().parse().ok()?;
    if bind_port == 0 {
        return None;
    }
    Some(ForwardRule {
        bind: format!("127.0.0.1:{bind_port}").parse().ok()?,
        dst,
        proto: ForwardProto::Udp,
    })
}

/// 全部转发规则的本地入口地址（控制端连接候选用；非法项跳过）。
pub fn forward_bind_addrs(cfg: &EasyTierAgentConfig) -> Vec<std::net::SocketAddr> {
    cfg.forwards
        .iter()
        .filter_map(|s| parse_forward_rule(s))
        .map(|r| r.bind)
        .collect()
}

/// agent `[easytier]` 配置 → EasyTier 网络配置；无效返回 None（静默不启动）。
fn build_net_config(cfg: &EasyTierAgentConfig) -> Option<EasyTierNetConfig> {
    if !cfg.enabled || cfg.network_name.trim().is_empty() {
        return None;
    }
    let forwards: Vec<ForwardRule> = cfg
        .forwards
        .iter()
        .filter_map(|s| parse_forward_rule(s))
        .collect();
    Some(EasyTierNetConfig {
        network_name: cfg.network_name.clone(),
        network_secret: (!cfg.network_secret.is_empty()).then(|| cfg.network_secret.clone()),
        peer_urls: cfg.peer_urls.iter().filter(|s| !s.trim().is_empty()).cloned().collect(),
        listeners: Vec::new(),
        virtual_ip: (!cfg.virtual_ip.is_empty()).then(|| cfg.virtual_ip.clone()),
        no_tun: cfg.no_tun,
        rpc_portal: switchboard_mod_easytier::config::default_rpc_portal(),
        forwards,
    })
}

/// cli 路径解析：显式 `cli_path` → binary_path 同目录的 easytier-cli(.exe)。
fn resolve_cli_path(cfg: &EasyTierAgentConfig) -> Option<PathBuf> {
    if !cfg.cli_path.trim().is_empty() {
        return Some(PathBuf::from(&cfg.cli_path));
    }
    let bin = PathBuf::from(&cfg.binary_path);
    let exe = if cfg!(windows) { "easytier-cli.exe" } else { "easytier-cli" };
    bin.parent().map(|dir| dir.join(exe))
}

/// 启动 EasyTier 节点托管。未启用/配置无效返回 None；启动失败返回 Err 带原因
/// （调用方决定是否降级——本项目统一降级告警）。
pub fn start(cfg: &EasyTierAgentConfig, work_dir: &std::path::Path) -> anyhow::Result<EasyTierHost> {
    let Some(net_cfg) = build_net_config(cfg) else {
        return Ok(EasyTierHost {
            virtual_ip: Arc::new(Mutex::new(None)),
        });
    };
    let binary = PathBuf::from(&cfg.binary_path);
    let cli = resolve_cli_path(cfg);
    let static_ip = (!cfg.virtual_ip.is_empty())
        .then(|| cfg.virtual_ip.split('/').next().unwrap_or("").to_string())
        .and_then(|s| s.parse::<IpAddr>().ok());

    let host = EasyTierHost {
        virtual_ip: Arc::new(Mutex::new(None)),
    };
    let slot = Arc::clone(&host.virtual_ip);
    let work_dir = work_dir.to_path_buf();
    std::thread::Builder::new()
        .name("rd-agent-easytier".into())
        .spawn(move || {
            let mut node = match EasyTierNode::launch(&binary, &net_cfg, &work_dir) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(err = %e, "EasyTier 节点启动失败（降级：仅直连/中继路径）");
                    return;
                }
            };
            tracing::info!(
                portal = %node.rpc_portal(),
                config = %node.config_path().display(),
                "EasyTier 节点已启动"
            );

            // 自身虚拟 IP：静态配置优先；否则 cli 查 node_info
            let ip = static_ip.or_else(|| {
                let Some(cli) = cli else {
                    tracing::warn!("未配 virtual_ip 且 easytier-cli 不可用：无法自查虚拟 IP（TUN 直连候选缺失）");
                    return None;
                };
                query_self_ipv4(&cli, node.rpc_portal())
            });
            if let Some(ip) = ip {
                tracing::info!(%ip, "EasyTier 虚拟 IP 就绪");
                *slot.lock().expect("easytier ip 锁中毒") = Some(ip);
            }

            // 巡检：意外退出则告警并清槽（不自动重启——重启由 agent 进程重启承担）
            loop {
                std::thread::sleep(WATCH_INTERVAL);
                if !node.is_running() {
                    tracing::warn!("EasyTier 节点进程退出（降级：仅直连/中继路径）");
                    *slot.lock().expect("easytier ip 锁中毒") = None;
                    return;
                }
            }
        })
        .context("EasyTier 宿主线程启动失败")?;
    Ok(host)
}

/// 经 easytier-cli 查本节点虚拟 IP（同步，宿主线程内）。
fn query_self_ipv4(cli: &std::path::Path, portal: std::net::SocketAddr) -> Option<IpAddr> {
    let out = std::process::Command::new(cli)
        .arg("--rpc-portal")
        .arg(portal.to_string())
        .arg("--output")
        .arg("json")
        .arg("route")
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&out.stderr),
            "easytier-cli route 查询失败"
        );
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    parse_self_ipv4(&raw).and_then(|s| s.parse::<IpAddr>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> EasyTierAgentConfig {
        EasyTierAgentConfig {
            enabled,
            binary_path: String::new(),
            cli_path: String::new(),
            network_name: String::new(),
            network_secret: String::new(),
            peer_urls: Vec::new(),
            virtual_ip: String::new(),
            no_tun: true,
            forwards: Vec::new(),
        }
    }

    #[test]
    fn 未启用_或无网络名_返回none() {
        assert!(build_net_config(&cfg(false)).is_none());
        assert!(build_net_config(&cfg(true)).is_none()); // network_name 空
    }

    #[test]
    fn 完整配置_转换无损() {
        let mut c = cfg(true);
        c.network_name = "rd-mesh".into();
        c.network_secret = "s".into();
        c.peer_urls = vec!["tcp://1.2.3.4:11010".into(), "  ".into()];
        c.virtual_ip = "10.126.126.2/24".into();
        c.no_tun = false;
        let n = build_net_config(&c).expect("有效配置");
        assert_eq!(n.network_name, "rd-mesh");
        assert_eq!(n.network_secret.as_deref(), Some("s"));
        assert_eq!(n.peer_urls, vec!["tcp://1.2.3.4:11010"]); // 空白项被滤
        assert_eq!(n.virtual_ip.as_deref(), Some("10.126.126.2/24"));
        assert!(!n.no_tun);
        assert!(n.forwards.is_empty()); // 未配转发规则
    }

    #[test]
    fn 转发规则解析() {
        let r = parse_forward_rule("10.126.126.3:21118@21130").expect("合法格式");
        assert_eq!(r.bind, "127.0.0.1:21130".parse::<std::net::SocketAddr>().unwrap());
        assert_eq!(r.dst, "10.126.126.3:21118".parse::<std::net::SocketAddr>().unwrap());
        assert_eq!(r.proto, ForwardProto::Udp);
        // 非法：缺 @ / 端口非法 / bind 端口 0
        assert!(parse_forward_rule("1.2.3.4:21118").is_none());
        assert!(parse_forward_rule("1.2.3.4:abc@21130").is_none());
        assert!(parse_forward_rule("1.2.3.4:21118@0").is_none());
    }

    #[test]
    fn 转发入口地址_非法项跳过() {
        let mut c = cfg(true);
        c.forwards = vec![
            "10.126.126.3:21118@21130".into(),
            "坏的格式".into(),
            "10.126.126.4:21118@21131".into(),
        ];
        let addrs = forward_bind_addrs(&c);
        assert_eq!(
            addrs,
            vec![
                "127.0.0.1:21130".parse::<std::net::SocketAddr>().unwrap(),
                "127.0.0.1:21131".parse::<std::net::SocketAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn cli路径_同目录兜底() {
        let mut c = cfg(true);
        c.binary_path = r"C:\tools\easytier\easytier-core.exe".into();
        assert_eq!(
            resolve_cli_path(&c),
            Some(PathBuf::from(r"C:\tools\easytier\easytier-cli.exe"))
        );
        c.cli_path = r"D:\custom\cli.exe".into();
        assert_eq!(resolve_cli_path(&c), Some(PathBuf::from(r"D:\custom\cli.exe")));
    }

    #[test]
    fn 静态ip_前缀剥离解析() {
        let s = "10.126.126.2/24".split('/').next().unwrap();
        let ip: IpAddr = s.parse().unwrap();
        assert_eq!(ip, "10.126.126.2".parse::<IpAddr>().unwrap());
    }
}
