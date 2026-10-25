//! 模块注册表（规范 06 篇）：manifest 加载 + L-A 自动校验。

use serde::Deserialize;
use std::path::Path;

/// 模块落地形态（05 §1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Binding {
    #[serde(rename = "native-rust")]
    NativeRust,
    Wasm,
    Process,
}

/// 合规等级（06 §1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub enum Compliance {
    /// 登记：manifest + caps + 遥测校验。
    #[serde(rename = "L-A")]
    LA,
    /// 可用：单阶段完整实现 + T0 跑通。
    #[serde(rename = "L-B")]
    LB,
    /// 认证：全计划 + 实网剖面 + bench.json。
    #[serde(rename = "L-C")]
    LC,
}

/// 阶段名（06 §2 manifest）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleStage {
    Discovery,
    Punch,
    Relay,
    Transport,
    Media,
    Codec,
}

/// 06 §2 模块 manifest。
#[derive(Clone, Debug, Deserialize)]
pub struct ModuleManifest {
    pub id: String,
    pub version: String,
    pub stage: ModuleStage,
    pub binding: Binding,
    #[serde(default)]
    pub provides: Option<serde_json::Value>,
    pub telemetry: String,
    pub compliance: Compliance,
    #[serde(default)]
    pub bench: Option<serde_json::Value>,
    #[serde(default)]
    pub origin: Option<serde_json::Value>,
    #[serde(default)]
    pub principles_card: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RegistryIndex {
    pub spec: String,
    pub modules: Vec<ModuleManifest>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("IO：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON：{0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest 无效：{0}")]
    Invalid(String),
}

/// id 规范：`sb.<域>.<名>`（06 §2）。
pub fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
}

/// L-A 校验（06 §1）：id/telemetry/阶段齐全。
pub fn validate(m: &ModuleManifest) -> Result<(), RegistryError> {
    if !valid_id(&m.id) {
        return Err(RegistryError::Invalid(format!("id 不符合 sb.<域>.<名>: {}", m.id)));
    }
    if m.telemetry.is_empty() {
        return Err(RegistryError::Invalid(format!("{}: telemetry 缺失", m.id)));
    }
    Ok(())
}

/// 从 registry/index.json 加载并校验全部 manifest。
pub fn load_index(path: &Path) -> Result<RegistryIndex, RegistryError> {
    let raw = std::fs::read_to_string(path)?;
    let idx: RegistryIndex = serde_json::from_str(&raw)?;
    if idx.spec != "switchboard/registry/1" {
        return Err(RegistryError::Invalid(format!("registry spec: {}", idx.spec)));
    }
    for m in &idx.modules {
        validate(m)?;
    }
    Ok(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str) -> ModuleManifest {
        ModuleManifest {
            id: id.into(),
            version: "0.1.0".into(),
            stage: ModuleStage::Transport,
            binding: Binding::NativeRust,
            provides: None,
            telemetry: "switchboard/telemetry-v1".into(),
            compliance: Compliance::LB,
            bench: None,
            origin: None,
            principles_card: None,
            license: None,
        }
    }

    #[test]
    fn id_格式校验() {
        assert!(valid_id("sb.core.loopback"));
        assert!(valid_id("sb.moonlight.fec-shards"));
        assert!(!valid_id("core.loopback"));
        assert!(!valid_id("sb.Core.Loopback"));
    }

    #[test]
    fn manifest_la_校验() {
        assert!(validate(&m("sb.core.loopback")).is_ok());
        assert!(validate(&m("bad_id")).is_err());
    }
}
