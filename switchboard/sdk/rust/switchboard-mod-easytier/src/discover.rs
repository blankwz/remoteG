//! S1 发现：mesh 路由表 → `EndpointCandidate`。
//!
//! - **TUN 模式**：`easytier-cli --rpc-portal <addr> --output json route` 查
//!   对端虚拟 IP，拼 `rd_port` 产出直拨候选（cost=2，WAN 直连档）；
//! - **no_tun 模式**：虚拟 IP 不可直拨，候选 = 调用方配置的转发地址
//!   `127.0.0.1:<port>`（cost=1，本机入口）。
//!
//! fail-open：route 查询失败只丢虚拟 IP 候选，转发地址照常产出——mesh 半通
//! 也算通，选路/竞速交给 Plan 执行器。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use switchboard_core::stages::{DiscoverCaps, Discoverer, Scope};
use switchboard_core::types::{EndpointCandidate, Proto, SourceKind};
use tokio::process::Command;

/// 单次 cli 查询超时（本机子进程，2s 足够）。
const CLI_TIMEOUT: Duration = Duration::from_secs(2);

/// EasyTier mesh 发现器（spec 01 S1，Scope::Tailnet）。
pub struct EasyTierDiscoverer {
    cli: PathBuf,
    rpc_portal: SocketAddr,
    /// 对端 rd 监听端口（虚拟 IP 候选拼端口用）。
    rd_port: u16,
    /// no_tun/转发地址候选（`127.0.0.1:<port>`，完整 SocketAddr）。
    forward_addrs: Vec<SocketAddr>,
    /// 本节点是否 no_tun（true 时不产虚拟 IP 直拨候选）。
    no_tun: bool,
}

impl EasyTierDiscoverer {
    /// 构造。`forward_addrs` 为本节点已配置的 `[[port_forward]]` 本地入口。
    pub fn new(
        cli: &Path,
        rpc_portal: SocketAddr,
        rd_port: u16,
        forward_addrs: Vec<SocketAddr>,
        no_tun: bool,
    ) -> Self {
        Self {
            cli: cli.to_path_buf(),
            rpc_portal,
            rd_port,
            forward_addrs,
            no_tun,
        }
    }

    /// 查 mesh 路由表，返回对端虚拟 IP（TUN 模式直拨候选来源）。
    async fn query_route_ips(&self) -> anyhow::Result<Vec<String>> {
        let fut = Command::new(&self.cli)
            .arg("--rpc-portal")
            .arg(self.rpc_portal.to_string())
            .arg("--output")
            .arg("json")
            .arg("route")
            .output();
        let out = tokio::time::timeout(CLI_TIMEOUT, fut)
            .await
            .map_err(|_| anyhow::anyhow!("easytier-cli 查询超时（{CLI_TIMEOUT:?}）"))?
            .map_err(|e| anyhow::anyhow!("运行 easytier-cli 失败: {e}"))?;
        anyhow::ensure!(
            out.status.success(),
            "easytier-cli route 失败: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let entries = super::cli_json::parse_route_json(&stdout)?;
        // 只取 IPv4 虚拟地址（rd 拨号目标）；去重保持顺序
        let mut seen = std::collections::HashSet::new();
        Ok(entries
            .into_iter()
            .filter_map(|e| e.ipv4)
            .filter(|ip| seen.insert(ip.clone()))
            .collect())
    }
}

#[async_trait]
impl Discoverer for EasyTierDiscoverer {
    fn caps(&self) -> DiscoverCaps {
        DiscoverCaps {
            scope: Scope::Tailnet,
        }
    }

    async fn poll_once(&self) -> anyhow::Result<Vec<EndpointCandidate>> {
        // 转发地址候选（两种模式都有效；本机入口成本最低）
        let mut out: Vec<EndpointCandidate> = self
            .forward_addrs
            .iter()
            .map(|addr| EndpointCandidate {
                addr: *addr,
                proto: Proto::Udp,
                source: SourceKind::Mesh,
                cost: 1,
                ttl: None,
            })
            .collect();

        // 虚拟 IP 直拨候选（仅 TUN 模式；查询失败不报错——见模块文档 fail-open）
        if !self.no_tun {
            match self.query_route_ips().await {
                Ok(ips) => {
                    for ip in ips {
                        if let Ok(addr) = format!("{ip}:{}", self.rd_port).parse::<SocketAddr>() {
                            out.push(EndpointCandidate {
                                addr,
                                proto: Proto::Udp,
                                source: SourceKind::Mesh,
                                cost: 2,
                                ttl: Some(Duration::from_secs(30)),
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(err = %e, "EasyTier route 查询失败（丢弃虚拟 IP 候选）");
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TUN 模式 + 假 cli：route 查询失败 → 只剩转发地址候选，poll_once 不 Err。
    #[tokio::test]
    async fn poll_once_route失败_fail_open() {
        let fwd: SocketAddr = "127.0.0.1:21199".parse().unwrap();
        let d = EasyTierDiscoverer::new(
            Path::new("easytier-cli-不存在"),
            "127.0.0.1:1".parse().unwrap(),
            21118,
            vec![fwd],
            false,
        );
        let cands = d.poll_once().await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].addr, fwd);
        assert_eq!(cands[0].cost, 1);
    }
}
