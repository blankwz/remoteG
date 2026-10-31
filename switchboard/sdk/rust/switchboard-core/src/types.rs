//! 基础数据类型（规范 01 §2）：候选端点、路径句柄。

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;

/// 传输协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Udp,
    Tcp,
    Both,
}

/// 候选来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// 同网段发现（广播/mDNS）。
    Lan,
    /// 会合服务器（RustDesk hbbs 型）。
    Rendezvous,
    /// STUN 测绘。
    Stun,
    /// DNS/MagicDNS。
    Dns,
    /// 用户手填。
    UserManual,
}

/// 路径来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    Direct,
    Relay,
    Loopback,
}

/// S1 产出：候选端点（01 §2）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EndpointCandidate {
    pub addr: SocketAddr,
    pub proto: Proto,
    pub source: SourceKind,
    /// 成本先验：0=loopback 1=LAN 2=WAN直连 3=中继 9=未知。
    pub cost: u8,
    /// 候选有效期（LAN 候选通常 30s）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Duration>,
}

/// 握手期实测（供传输面做先验）。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Measured {
    pub rtt_ms: f32,
    pub mtu: u16,
    pub loss_ratio: f32,
}

/// 原始管道收发原语（v0.1 参考模块为内存管道；真实打洞模块注入 UDP socket）。
#[async_trait::async_trait]
pub trait PathSocket: Send + Sync {
    async fn send_to(&self, data: &[u8], peer: SocketAddr) -> std::io::Result<()>;
    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)>;
}

/// S2/S3 产物：已验证的原始管道（01 §2）。
pub struct PathHandle {
    /// v0.1 预留：参考实现为 None（内存路径）；0.2 起真实打洞/中继必须携带。
    pub socket: Option<std::sync::Arc<dyn PathSocket>>,
    pub kind: PathKind,
    /// 产出模块 id（如 `sb.rustdesk.punch-v2`）。
    pub via: String,
    pub measured: Measured,
    /// 对端地址（真实传输面建立连接用；Loopback 内存路径为 None）。
    pub addr: Option<SocketAddr>,
}

impl PathHandle {
    pub fn memory(kind: PathKind, via: &str, measured: Measured) -> Self {
        Self {
            socket: None,
            kind,
            via: via.to_string(),
            measured,
            addr: None,
        }
    }

    pub fn to_addr(kind: PathKind, via: &str, measured: Measured, addr: SocketAddr) -> Self {
        Self {
            socket: None,
            kind,
            via: via.to_string(),
            measured,
            addr: Some(addr),
        }
    }
}
