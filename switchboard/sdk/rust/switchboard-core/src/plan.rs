//! Plan DSL（规范 02 篇）：TOML 解析 + 校验 + 默认预算。

use serde::Deserialize;

use crate::SPEC_VERSION;

/// 02 §5 默认预算。
pub mod budgets {
    pub const DISCOVERY_MS: u64 = 2_000;
    pub const PUNCH_MS: u64 = 3_000;
    pub const RELAY_MS: u64 = 5_000;
    pub const TRANSPORT_MS: u64 = 5_000;
    /// rd 教训：DERP 链路 RTT 1.2s+ 时 5s 必超时（02 §5）。
    pub const TRANSPORT_RELAY_MS: u64 = 15_000;
    pub const PATH_MS: u64 = 15_000;
}

#[derive(Debug, Deserialize)]
pub struct PlanFile {
    pub spec: String,
    pub name: String,
    #[serde(default)]
    pub session: Option<toml::Value>,
    #[serde(default)]
    pub requirements: Option<Requirements>,
    /// TOML `[[path]]` 数组表（DSL 关键字是单数 path）。
    #[serde(default, rename = "path")]
    pub paths: Vec<PathCfg>,
    #[serde(default)]
    pub media: Option<toml::Value>,
    #[serde(default)]
    pub upgrade: Option<toml::Value>,
}

/// 02 §2.3 全局 caps 门控。
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Requirements {
    #[serde(default)]
    pub min_mtu: u16,
    #[serde(default)]
    pub datagram: bool,
}

/// 02 §1 单条候选路径。
#[derive(Debug, Deserialize)]
pub struct PathCfg {
    pub name: String,
    #[serde(default)]
    pub discovery: Option<String>,
    #[serde(default)]
    pub punch: Option<String>,
    pub transport: String,
    /// punch 失败 → 该中继（同 transport）。
    #[serde(default)]
    pub fallback_relay: Option<String>,
    #[serde(default)]
    pub budget_ms: Option<u64>,
}

/// 解析后的计划。
#[derive(Debug)]
pub struct Plan {
    pub name: String,
    pub requirements: Requirements,
    pub paths: Vec<PathCfg>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("spec 版本不匹配：期望 {expected}，实际 {actual}")]
    SpecMismatch { expected: String, actual: String },
    #[error("TOML 解析失败：{0}")]
    Parse(#[from] toml::de::Error),
    #[error("计划无效：{0}")]
    Invalid(String),
}

impl Plan {
    pub fn from_toml(s: &str) -> Result<Self, PlanError> {
        let f: PlanFile = toml::from_str(s)?;
        if f.spec != SPEC_VERSION {
            return Err(PlanError::SpecMismatch {
                expected: SPEC_VERSION.to_string(),
                actual: f.spec,
            });
        }
        if f.paths.is_empty() {
            return Err(PlanError::Invalid("paths 不能为空".into()));
        }
        for (i, p) in f.paths.iter().enumerate() {
            if p.transport.trim().is_empty() {
                return Err(PlanError::Invalid(format!("paths[{i}].transport 为空")));
            }
        }
        Ok(Self {
            name: f.name,
            requirements: f.requirements.unwrap_or_default(),
            paths: f.paths,
        })
    }

    /// 02 §2.3：构造 transport requirement（media profile 的隐含要求 + 全局要求）。
    pub fn transport_requirement(&self, media: Option<crate::stages::MediaProfile>) -> crate::caps::ChannelCaps {
        let mut req = media.map(|m| m.transport_requirement(self.requirements.min_mtu))
            .unwrap_or_default();
        if self.requirements.min_mtu > req.mtu {
            req.mtu = self.requirements.min_mtu;
        }
        if self.requirements.datagram && req.datagram.is_none() {
            req.datagram = Some(crate::caps::DatagramCaps { max_size: 1024, lossy_ok: true });
        }
        req
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &str = r#"
spec = "switchboard/0.1"
name = "demo"

[[path]]
name = "lan"
discovery = "sb.lan.udp-broadcast"
transport = "sb.core.loopback"

[[path]]
name = "punch"
punch = "sb.rustdesk.punch-v2"
transport = "sb.core.loopback"
fallback_relay = "sb.derp.self-hk"

[requirements]
min_mtu = 1200
"#;

    #[test]
    fn 解析演示计划() {
        let p = Plan::from_toml(DEMO).unwrap();
        assert_eq!(p.name, "demo");
        assert_eq!(p.paths.len(), 2);
        assert_eq!(p.paths[1].fallback_relay.as_deref(), Some("sb.derp.self-hk"));
        assert_eq!(p.requirements.min_mtu, 1200);
    }

    #[test]
    fn 版本不匹配_拒绝() {
        let bad = DEMO.replace("switchboard/0.1", "switchboard/9.9");
        assert!(matches!(
            Plan::from_toml(&bad),
            Err(PlanError::SpecMismatch { .. })
        ));
    }

    #[test]
    fn 空路径_拒绝() {
        let bad = "spec = \"switchboard/0.1\"\nname = \"x\"\n";
        assert!(matches!(Plan::from_toml(bad), Err(PlanError::Invalid(_))));
    }
}
