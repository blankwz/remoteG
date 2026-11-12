//! EasyTier 网络配置：`EasyTierNetConfig` → easytier-core `--config-file` TOML。
//!
//! 字段名与 easytier-core `config/toml.rs` 的反序列化结构对齐（源码已核对）：
//! `network_identity` / `listeners` / `[[peer]] uri` / `[[port_forward]]
//! bind_addr,dst_addr,proto` / `[flags] no_tun`。单测快照锁定格式。

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// 转发协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardProto {
    Tcp,
    Udp,
}

/// 一条端口转发规则：本机 `bind` → mesh 内 `dst`（easytier-core gateway 承载）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRule {
    /// 本机监听（如 `127.0.0.1:21130`；rd 控制端拨这个地址）。
    pub bind: SocketAddr,
    /// mesh 内目标（如 `10.126.126.2:21118`；对端虚拟 IP + rd 端口）。
    pub dst: SocketAddr,
    pub proto: ForwardProto,
}

/// EasyTier 组网配置（serde：可直接嵌 agent.toml / 控制端设置存储）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EasyTierNetConfig {
    /// 网络名（同网才能互见）。
    pub network_name: String,
    /// 网络密钥（None = 不加密的公共网，不推荐）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_secret: Option<String>,
    /// 引导 peer（`tcp://host:port` / `udp://...`；官方公共节点或自建中继）。
    #[serde(default)]
    pub peer_urls: Vec<String>,
    /// 本节点监听（空 = easytier 默认；被其它节点直连的入口）。
    #[serde(default)]
    pub listeners: Vec<String>,
    /// 静态虚拟 IP（如 `10.126.126.2/24`）；None = DHCP。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_ip: Option<String>,
    /// 不建虚拟网卡（免管理员）。**默认 true**；TUN 模式需管理员但可直拨虚拟 IP。
    #[serde(default = "default_true")]
    pub no_tun: bool,
    /// 本节点 rpc-portal（easytier-cli / 发现查询入口）。
    #[serde(default = "default_rpc_portal")]
    pub rpc_portal: SocketAddr,
    /// no_tun 模式的端口转发规则。
    #[serde(default)]
    pub forwards: Vec<ForwardRule>,
}

fn default_true() -> bool {
    true
}

/// 默认 rpc-portal（easytier-cli 与 [`crate::discover::EasyTierDiscoverer`] 的查询入口）。
pub fn default_rpc_portal() -> SocketAddr {
    "127.0.0.1:15888".parse().expect("常量地址合法")
}

impl Default for EasyTierNetConfig {
    fn default() -> Self {
        Self {
            network_name: String::new(),
            network_secret: None,
            peer_urls: Vec::new(),
            listeners: Vec::new(),
            virtual_ip: None,
            no_tun: true,
            rpc_portal: default_rpc_portal(),
            forwards: Vec::new(),
        }
    }
}

impl EasyTierNetConfig {
    /// 生成 easytier-core `--config-file` TOML（字段与上游 toml.rs 反序列化
    /// 结构一一对应；`toml` crate 的数组表/内联表输出即上游可解析格式）。
    pub fn to_toml(&self) -> String {
        #[derive(Serialize)]
        struct PeerCfg {
            uri: String,
        }
        #[derive(Serialize)]
        struct PortForwardCfg {
            bind_addr: String,
            dst_addr: String,
            proto: &'static str,
        }
        #[derive(Serialize)]
        struct NetworkIdentity {
            network_name: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            network_secret: Option<String>,
        }
        #[derive(Serialize)]
        struct Flags {
            no_tun: bool,
        }
        #[derive(Serialize)]
        struct Root {
            #[serde(skip_serializing_if = "Option::is_none")]
            ipv4: Option<String>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            listeners: Vec<String>,
            network_identity: NetworkIdentity,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            peer: Vec<PeerCfg>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            port_forward: Vec<PortForwardCfg>,
            flags: Flags,
        }

        let root = Root {
            ipv4: self.virtual_ip.clone(),
            listeners: self.listeners.clone(),
            network_identity: NetworkIdentity {
                network_name: self.network_name.clone(),
                network_secret: self.network_secret.clone(),
            },
            peer: self
                .peer_urls
                .iter()
                .map(|uri| PeerCfg { uri: uri.clone() })
                .collect(),
            port_forward: self
                .forwards
                .iter()
                .map(|f| PortForwardCfg {
                    bind_addr: f.bind.to_string(),
                    dst_addr: f.dst.to_string(),
                    proto: match f.proto {
                        ForwardProto::Tcp => "tcp",
                        ForwardProto::Udp => "udp",
                    },
                })
                .collect(),
            flags: Flags {
                no_tun: self.no_tun,
            },
        };
        toml::to_string_pretty(&root).expect("TOML 序列化不失败（纯基础类型）")
    }

    /// easytier-core 启动参数：`--config-file <toml> --rpc-portal <addr>`。
    /// rpc-portal 必须显式给（上游 RpcPortalOptions 无默认值，不给即不开查询口）。
    pub fn core_args(&self) -> Vec<String> {
        vec!["--rpc-portal".into(), self.rpc_portal.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn 默认配置_无tun_默认portal() {
        let cfg = EasyTierNetConfig::default();
        assert!(cfg.no_tun);
        assert_eq!(
            cfg.rpc_portal,
            "127.0.0.1:15888".parse::<SocketAddr>().unwrap()
        );
    }

    /// TOML 生成快照：字段名与上游 toml.rs 对齐（network_identity/peer/
    /// port_forward/flags.no_tun）。
    #[test]
    fn toml生成_完整字段() {
        let cfg = EasyTierNetConfig {
            network_name: "rd-mesh".into(),
            network_secret: Some("s3cret".into()),
            peer_urls: vec!["tcp://public.easytier.top:11010".into()],
            listeners: vec!["udp://0.0.0.0:21116".into()],
            virtual_ip: Some("10.126.126.2/24".into()),
            no_tun: true,
            rpc_portal: "127.0.0.1:15888".parse().unwrap(),
            forwards: vec![ForwardRule {
                bind: "127.0.0.1:21130".parse().unwrap(),
                dst: "10.126.126.3:21118".parse().unwrap(),
                proto: ForwardProto::Udp,
            }],
        };
        let s = cfg.to_toml();
        // 关键键名存在（上游反序列化按这些键收）
        for key in [
            "[network_identity]",
            "network_name",
            "network_secret",
            "[[peer]]",
            "uri = \"tcp://public.easytier.top:11010\"",
            "[[port_forward]]",
            "bind_addr = \"127.0.0.1:21130\"",
            "dst_addr = \"10.126.126.3:21118\"",
            "proto = \"udp\"",
            "[flags]",
            "no_tun = true",
            "ipv4 = \"10.126.126.2/24\"",
            "udp://0.0.0.0:21116",
        ] {
            assert!(s.contains(key), "TOML 缺少 {key}:\n{s}");
        }
    }

    /// 最小配置：空 peer/listeners/forwards 被省略，不产空表头。
    #[test]
    fn toml生成_最小配置_省略空节() {
        let cfg = EasyTierNetConfig {
            network_name: "n".into(),
            ..Default::default()
        };
        let s = cfg.to_toml();
        assert!(!s.contains("[[peer]]"));
        assert!(!s.contains("[[port_forward]]"));
        assert!(!s.contains("listeners"));
        assert!(s.contains("no_tun = true"));
    }

    /// serde 往返（agent.toml 嵌入用）。
    #[test]
    fn serde_往返无损() {
        let cfg = EasyTierNetConfig {
            network_name: "n".into(),
            network_secret: Some("k".into()),
            peer_urls: vec!["udp://1.2.3.4:11010".into()],
            no_tun: false,
            ..Default::default()
        };
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: EasyTierNetConfig = toml::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }

    /// 密钥省略时不产空 network_secret（上游 Option<String> 接受缺键）。
    #[test]
    fn toml生成_无密钥_不产空secret键() {
        let cfg = EasyTierNetConfig {
            network_name: "n".into(),
            ..Default::default()
        };
        let s = cfg.to_toml();
        assert!(!s.contains("network_secret"));
        let ip: SocketAddr = format!("{}:21118", IpAddr::V4(Ipv4Addr::LOCALHOST))
            .parse()
            .unwrap();
        assert_eq!(ip.port(), 21118);
    }

    /// core_args：rpc-portal 显式出现（上游不给即不开查询口）。
    #[test]
    fn core_args_含rpc_portal() {
        let cfg = EasyTierNetConfig::default();
        let args = cfg.core_args();
        assert_eq!(args, vec!["--rpc-portal", "127.0.0.1:15888"]);
    }
}
