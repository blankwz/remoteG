//! `sb.easytier.mesh`（process 绑定，规范 01 S1）：EasyTier 组网适配器。
//!
//! # 定位
//!
//! EasyTier（去中心化 mesh VPN）自带 UDP/TCP 打洞 + 中继 + 多协议回退，
//! 发生在 mesh 内部，对本编排器透明——rd 视角它只是一张「底层网」：本模块
//! 负责把 easytier-core 进程跑起来、把 mesh 内对端可达地址变成
//! [`switchboard_core::types::EndpointCandidate`]，传输复用 `sb.quinn.quic-rd`
//! （QUIC 直拨候选地址，指纹钉扎不变）。
//!
//! 与 spec/07 `sb.ext.tailscale` 同构：**process 绑定**（spawn easytier-core.exe /
//! 查询 easytier-cli），不引入 easytier Rust 依赖（上游 edition 2024 / rust 1.95 /
//! 依赖树巨大，且跨 workspace 继承解析是已踩过的坑）。
//!
//! # 两种运行模式
//!
//! - **TUN**（管理员）：rd QUIC 直拨对端虚拟 IP（route 表查出，拼 rd 端口）；
//! - **no_tun + UDP 端口转发**（默认，免管理员）：本节点 `[[port_forward]]`
//!   把 `127.0.0.1:本地端口` 映射到对端虚拟 IP:端口，候选即转发地址。
//!
//! # 使用
//!
//! 异步入口内（trait [`Discoverer`](switchboard_core::stages::Discoverer)）：
//!
//! ```no_run
//! use switchboard_mod_easytier::{EasyTierNetConfig, EasyTierNode, EasyTierDiscoverer};
//! use switchboard_core::stages::Discoverer as _;
//! # async fn demo() -> anyhow::Result<()> {
//! let cfg = EasyTierNetConfig {
//!     network_name: "rd-mesh".into(),
//!     network_secret: Some("secret".into()),
//!     ..Default::default()
//! };
//! let node = EasyTierNode::launch(
//!     std::path::Path::new("easytier-core.exe"),
//!     &cfg,
//!     std::path::Path::new("."),
//! )?;
//! let discoverer = EasyTierDiscoverer::new(
//!     std::path::Path::new("easytier-cli.exe"),
//!     node.rpc_portal(),
//!     21118, // 对端 rd 监听端口
//!     vec![], // no_tun 转发地址（127.0.0.1:port）
//!     cfg.no_tun,
//! );
//! let cands: Vec<_> = discoverer.poll_once().await?;
//! drop((node, discoverer, cands));
//! # Ok(())
//! # }
//! ```

pub mod cli_json;
pub mod config;
pub mod discover;
pub mod proc;

/// spec/07 模块 id（process 绑定，阶段 S1；S2/S3 由 mesh 内部承担）。
pub const MODULE_ID: &str = "sb.easytier.mesh";

pub use config::{EasyTierNetConfig, ForwardProto, ForwardRule};
pub use discover::EasyTierDiscoverer;
pub use proc::EasyTierNode;

#[cfg(test)]
mod tests {
    use super::*;
    use switchboard_core::stages::{DiscoverCaps, Discoverer, Scope};

    /// 模块 id 遵循 spec 07 `sb.<来源>.<机制>` 命名。
    #[test]
    fn 模块id_遵循命名约定() {
        assert_eq!(MODULE_ID, "sb.easytier.mesh");
        assert!(MODULE_ID.starts_with("sb.easytier."));
    }

    /// Discoverer caps：EasyTier 属组网工具范围（Tailnet）。
    #[test]
    fn 发现器_caps_为组网范围() {
        let d = EasyTierDiscoverer::new(
            std::path::Path::new("easytier-cli"),
            "127.0.0.1:15888".parse().unwrap(),
            21118,
            vec![],
            true,
        );
        let DiscoverCaps { scope } = d.caps();
        assert_eq!(scope, Scope::Tailnet);
    }

    /// no_tun 模式：候选 = 转发地址（本地回环），不产出虚拟 IP 直拨候选。
    #[tokio::test]
    async fn no_tun模式_候选为转发地址() {
        let fwd: std::net::SocketAddr = "127.0.0.1:21130".parse().unwrap();
        let d = EasyTierDiscoverer::new(
            std::path::Path::new("easytier-cli"),
            "127.0.0.1:15888".parse().unwrap(),
            21118,
            vec![fwd],
            true,
        );
        let cands = d.poll_once().await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].addr, fwd);
        assert_eq!(
            cands[0].source,
            switchboard_core::types::SourceKind::Mesh
        );
        // 转发地址是本机入口，成本最低（先于虚拟 IP 直拨）
        assert_eq!(cands[0].cost, 1);
    }

    /// TUN 模式 + cli 失败：route 查询失败只丢虚拟 IP 候选，
    /// 转发地址候选照常产出（fail-open：mesh 半通也算通）。
    #[tokio::test]
    async fn tun模式_cli失败_转发地址仍产出() {
        let fwd: std::net::SocketAddr = "127.0.0.1:21132".parse().unwrap();
        let d = EasyTierDiscoverer::new(
            std::path::Path::new("easytier-cli-不存在的路径"),
            "127.0.0.1:1".parse().unwrap(),
            21118,
            vec![fwd],
            false,
        );
        // TUN 模式转发地址同样有效（用户配了转发就意味着这是可达入口）
        let cands = d.poll_once().await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].addr, fwd);
    }
}
