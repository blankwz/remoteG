//! 授权码（商业化收费能力的最小闭环）：Ed25519 离线验签，不联网、不依赖
//! 授权服务器——独立开发者起步阶段的最稳方案（ToDesk 早期同款思路）。
//!
//! ## 授权码格式
//! `RD1.<base64url(payload)>.<base64url(sig)>`
//! - `payload`：JSON `{"plan":"pro","name":"...","devices_max":99,"expires":null,"issued":1730000000}`
//! - `sig`：对 payload 原始字节的 Ed25519 签名
//!
//! ## 密钥治理（重要）
//! - 公钥内嵌 [`LICENSE_PUBKEY_HEX`]，随产品分发；
//! - 私钥（SigningKey seed）**绝不进仓库**：签发工具
//!   `examples/issue_license.rs` 从 `RD_LICENSE_SEED` 环境变量读取（64 位 hex）；
//! - 仓库内置的开发默认种子由固定字符串派生（[`dev_seed`]，公开可复现），
//!   **仅供开发/演示**：任何拿到源码的人都能用同一 seed 造出有效授权码，
//!   因此上线前必须生成随机正式种子并替换公钥常量（上线检查清单第 1 条）。

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey, SECRET_KEY_LENGTH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 授权档位。`Free` 随产品分发（限设备/连接数），`Pro` 由授权码解锁。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Free,
    Pro,
}

/// 授权载荷（签名保护的部分；字段只增不改，向前兼容靠「未知字段忽略」）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct License {
    /// 授权档位。
    pub plan: Plan,
    /// 授权给谁（展示用；不参与门控）。
    #[serde(default)]
    pub name: String,
    /// 允许的受信任设备数上限（Free 语义为 2，Pro 通常 99）。
    pub devices_max: u32,
    /// 过期时间（Unix 秒；`None` = 永久）。
    #[serde(default)]
    pub expires: Option<u64>,
    /// 签发时间（Unix 秒；仅展示）。
    pub issued: u64,
}

/// 授权校验结果细节（供 UI 展示「为什么是免费版」）。
#[derive(Debug, thiserror::Error)]
pub enum LicenseError {
    #[error("授权码格式错误（应为 RD1.payload.sig）")]
    Format,
    #[error("授权码不是本产品签发的（验签失败）")]
    BadSignature,
    #[error("授权码已过期（{0}）")]
    Expired(String),
    #[error("授权码内容不合法：{0}")]
    Malformed(String),
}

/// 内嵌公钥（开发默认对；上线前替换，见模块注释）。
pub const LICENSE_PUBKEY_HEX: &str =
    "2ec01a082e84d43fb5dad1e4b8d45a1ba1c2ff37f3bcaeb1073daf6f85bf7796";

/// base64url 无填充（授权码文本编码；URL/文件名安全）。
fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// 开发默认种子：固定字符串派生（公开可复现 ⇒ 仅开发/演示用）。
fn dev_seed() -> [u8; SECRET_KEY_LENGTH] {
    let mut seed = [0u8; SECRET_KEY_LENGTH];
    let digest = Sha256::digest(b"rd-license-dev-seed-DO-NOT-USE-IN-PRODUCTION");
    seed.copy_from_slice(&digest);
    seed
}

/// 内嵌公钥对应的验签键（开发默认对 = `dev_seed()` 派生）。
fn pubkey() -> VerifyingKey {
    // 开发公钥与 dev_seed 的一致性由 verify_license 的往返单测锚定；
    // 替换正式密钥时：SigningKey::from_bytes(&random_seed).verifying_key()
    let bytes: [u8; 32] = hex_dec(LICENSE_PUBKEY_HEX)
        .try_into()
        .expect("LICENSE_PUBKEY_HEX 必须是 32 字节 hex");
    VerifyingKey::from_bytes(&bytes).expect("内嵌公钥必须可解析")
}

fn hex_dec(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("公钥常量必须是合法 hex"))
        .collect()
}

/// 校验授权码文本。`now_unix` 由调用方传入（可测性；生产传当前时间）。
///
/// 返回解析后的 [`License`]；任何一步失败都带原因（UI 直读展示）。
pub fn verify_license(text: &str, now_unix: u64) -> Result<License, LicenseError> {
    let text = text.trim();
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 3 || parts[0] != "RD1" {
        return Err(LicenseError::Format);
    }
    let payload = b64().decode(parts[1]).map_err(|_| LicenseError::Format)?;
    let sig_bytes = b64().decode(parts[2]).map_err(|_| LicenseError::Format)?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| LicenseError::Format)?;
    pubkey()
        .verify_strict(&payload, &sig)
        .map_err(|_| LicenseError::BadSignature)?;
    let license: License =
        serde_json::from_slice(&payload).map_err(|e| LicenseError::Malformed(e.to_string()))?;
    if let Some(exp) = license.expires {
        if now_unix >= exp {
            return Err(LicenseError::Expired(humantime_seconds(exp, now_unix)));
        }
    }
    Ok(license)
}

/// 过期提示的人话时长（过期多久了）。
fn humantime_seconds(exp: u64, now: u64) -> String {
    let days = (now.saturating_sub(exp)) / 86_400;
    if days > 0 {
        format!("已过期 {days} 天")
    } else {
        "已过期（不足一天）".to_string()
    }
}

/// 从 `<config_dir>/license.key` 加载授权；文件缺失/无效一律回落 Free
/// （**永不因授权问题阻断被控服务**——免费版是产品基线，不是故障态）。
pub fn load_license(config_dir: &std::path::Path) -> Option<License> {
    let path = config_dir.join("license.key");
    let text = std::fs::read_to_string(&path).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match verify_license(&text, now) {
        Ok(license) => {
            tracing::info!(plan = ?license.plan, name = %license.name, "授权已加载");
            Some(license)
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), "授权码无效，按免费版运行: {err}");
            None
        }
    }
}

/// 免费版门控常量：受信任设备数上限。
pub const FREE_DEVICE_MAX: u32 = 2;
/// 免费版门控常量：并发控制端连接数上限（帮家人修电脑的主场景一台够用）。
pub const FREE_CONN_MAX: usize = 1;

/// 开发默认种子的 hex（签发工具 `RD_LICENSE_SEED` 缺省值；仅开发用）。
pub fn dev_seed_hex() -> String {
    hex_enc(&dev_seed())
}

fn hex_enc(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 签发（供 examples/issue_license.rs 使用；lib 不暴露生产签发路径）。
#[doc(hidden)]
pub fn sign_payload(payload_json: &str, seed_hex: &str) -> anyhow::Result<String> {
    use ed25519_dalek::Signer;
    let seed = decode_seed(seed_hex)?;
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let sig = key.sign(payload_json.as_bytes());
    Ok(format!(
        "RD1.{}.{}",
        b64().encode(payload_json.as_bytes()),
        b64().encode(sig.to_bytes())
    ))
}

fn decode_seed(seed_hex: &str) -> anyhow::Result<[u8; SECRET_KEY_LENGTH]> {
    let bytes = hex_dec(seed_hex.trim());
    anyhow::ensure!(
        bytes.len() == SECRET_KEY_LENGTH,
        "RD_LICENSE_SEED 必须是 64 位 hex（32 字节）"
    );
    Ok(bytes.try_into().expect("长度已校验"))
}

/// 开发默认公钥 = dev_seed 派生公钥（模块注释一致性锚点）。
pub fn dev_pubkey_hex() -> String {
    hex_enc(&dev_key().verifying_key().to_bytes())
}

fn dev_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&dev_seed())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 往返：开发种子签发 → 内嵌公钥验签通过（锚定公钥常量与种子一致性；
    /// 任何一方单独替换都会挂掉这条测试——上线换正式密钥时同步更新两处）。
    #[test]
    fn dev_roundtrip_and_guards() {
        let payload = serde_json::json!({
            "plan": "pro", "name": "开发测试", "devices_max": 99,
            "expires": null, "issued": 1_730_000_000u64
        })
        .to_string();
        let code = sign_payload(&payload, &dev_seed_hex()).expect("签发成功");
        let license = verify_license(&code, 1_730_000_100).expect("验签通过");
        assert_eq!(license.plan, Plan::Pro);
        assert_eq!(license.devices_max, 99);

        // 篡改 payload → 验签失败
        let mut tampered = code.clone();
        tampered.replace_range(6..8, "XX");
        assert!(matches!(
            verify_license(&tampered, 1_730_000_100),
            Err(LicenseError::BadSignature) | Err(LicenseError::Format)
        ));

        // 非本产品签发（随机密钥）→ 验签失败
        let other_seed = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
        let forged = sign_payload(&payload, other_seed).expect("签名成功");
        assert!(
            matches!(
                verify_license(&forged, 1_730_000_100),
                Err(LicenseError::BadSignature)
            ),
            "随机种子签的码必须被拒"
        );

        // 过期 → Expired 且带人话时长
        let expired_payload = serde_json::json!({
            "plan": "pro", "name": "过期件", "devices_max": 99,
            "expires": 1_700_000_000u64, "issued": 1_690_000_000u64
        })
        .to_string();
        let expired = sign_payload(&expired_payload, &dev_seed_hex()).unwrap();
        assert!(matches!(
            verify_license(&expired, 1_730_000_100),
            Err(LicenseError::Expired(_))
        ));

        // 格式错 → Format
        assert!(matches!(
            verify_license("garbage", 0),
            Err(LicenseError::Format)
        ));
    }

    /// Free 语义常量与产品预期一致（改这里 = 改商业策略，需评审）。
    #[test]
    fn free_tier_constants() {
        assert_eq!(FREE_DEVICE_MAX, 2);
        assert_eq!(FREE_CONN_MAX, 1);
    }
}
