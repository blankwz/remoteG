//! 直连候选采集（T-035）：本机网卡地址 + UPnP 路由器开门。
//!
//! 候选随 REG 上报给中继 → DIAL ACK 下发 → 控制端对每个候选直接发起 QUIC
//! 竞速（[`rd_transport::connect_via_relay_punch`]）。候选 = 「对端可能可达
//! 的直连入口」，与反射打洞互补：
//! - **本机网卡地址**（v4 局域网 / v6 全局）：同网段秒连；双端都有公网 IPv6
//!   时免打洞直连（国内宽带已普遍分配 v6）；
//! - **UPnP 外部映射**：路由器开门后，控制端直连「路由器公网 IP:21118」，
//!   对被控端 NAT 是最硬的可达入口（家庭路由器支持率不低）。
//!
//! 全部 best-effort：单项失败静默跳过，不影响任何现有路径。
//! 开关：`RD_PUNCH=0` 全关（总闸）；`RD_PUNCH_UPNP=0` 只关 UPnP。

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use rd_transport::MAX_CANDIDATES;

/// UPnP 映射租期（秒）。**必须有限**（安全审计 F2）：lease=0 是永久映射，
/// agent 崩溃/卸载后路由器上的公网入口不消失；30 分钟租期 + 运行期续租
/// 保证进程死亡后入口最迟 30 分钟自动关闭。
const UPNP_LEASE_SECS: u32 = 1800;
/// 续租间隔（租期的 1/3，连续两次续租失败映射才可能过期）。
const UPNP_RENEW_INTERVAL: Duration = Duration::from_secs(600);

/// 采集直连候选（顺序即优先级：v4 局域网 → v6 全局 → UPnP 外部映射），
/// 上限 [`MAX_CANDIDATES`]（超出截断）。`listen_port` 为 QUIC 监听端口。
pub async fn collect(listen_port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = crate::netinfo::local_ip_addrs()
        .into_iter()
        .map(|ip| SocketAddr::new(ip, listen_port))
        .collect();

    if std::env::var("RD_PUNCH_UPNP").as_deref() != Ok("0") {
        if let Some(ext) = upnp_external_addr(listen_port).await {
            tracing::info!(%ext, "UPnP 端口映射成功，纳入直连候选");
            spawn_upnp_renewal(listen_port);
            out.push(ext);
        } else {
            tracing::debug!("UPnP 端口映射不可用（路由器不支持或已禁用），跳过");
        }
    }

    out.truncate(MAX_CANDIDATES);
    tracing::info!(?out, "直连候选采集完成");
    out
}

/// UPnP 映射续租任务：每 [`UPNP_RENEW_INTERVAL`] 重做一次 add_port（幂等覆盖），
/// 进程退出即停——租期有限，映射最迟 [`UPNP_LEASE_SECS`] 秒后自动失效。
/// 静默 best-effort：路由器重启/网关更换导致失败不重试轰击（下个周期再试）。
fn spawn_upnp_renewal(port: u16) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(UPNP_RENEW_INTERVAL).await;
            if upnp_add_mapping(port).await.is_none() {
                tracing::debug!("UPnP 续租失败（路由器不可达或已禁用），下个周期再试");
            }
        }
    });
}

/// UPnP/IGD：在路由器上映射 `UDP port → 本机 port` 并取路由器公网 IP。
/// 网络 IO 可能阻塞（网关搜索数秒），放独立线程 + 10s 超时兜底。
async fn upnp_external_addr(port: u16) -> Option<SocketAddr> {
    let ext = upnp_add_mapping(port).await?;
    Some(SocketAddr::new(IpAddr::from(ext), port))
}

/// 实际建立（或续租）UPnP 映射，返回路由器外部 IPv4；失败返回 None。
async fn upnp_add_mapping(port: u16) -> Option<std::net::Ipv4Addr> {
    let v4 = crate::netinfo::local_ip_addrs()
        .into_iter()
        .find_map(|ip| match ip {
            IpAddr::V4(v) => Some(v),
            IpAddr::V6(_) => None,
        })?;
    let task = tokio::task::spawn_blocking(move || -> Option<std::net::Ipv4Addr> {
        let gw = igd::search_gateway(Default::default()).ok()?;
        let ext = gw.get_external_ip().ok()?;
        // 有界租期（安全审计 F2）：不用 lease=0 永久映射——agent 进程意外
        // 退出时映射随租期自动消失；运行期由续租任务保持有效
        gw.add_port(
            igd::PortMappingProtocol::UDP,
            port,
            std::net::SocketAddrV4::new(v4, port),
            UPNP_LEASE_SECS,
            "rd punch",
        )
        .ok()?;
        Some(ext)
    });
    match tokio::time::timeout(std::time::Duration::from_secs(10), task).await {
        Ok(Ok(Some(ip))) => Some(ip),
        _ => None,
    }
}
