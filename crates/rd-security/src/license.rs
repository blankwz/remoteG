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
//! - **2026-11-06 已轮换正式密钥**（上线检查清单第 1 条）：内嵌公钥为随机正式
//!   种子派生，种子由持有者存密码管理器；
//! - 开发默认种子（[`dev_seed`]，固定字符串派生，公开可复现）**仅 debug 构建
//!   的验签兜底**：`cargo test`/e2e 用它签测试授权码，debug 下可验签通过；
//!   正式发布为 release 构建，兜底分支编译期剔除——**开发码在发布版一律被拒**
//!   （由 `dev_roundtrip_and_guards` 的双档位断言锚定）。

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

/// 内嵌验签公钥（正式种子派生；种子绝不进仓库，见模块注释）。
pub const LICENSE_PUBKEY_HEX: &str =
    "70c6b09003f06f3df3dd4c96cef3d2ac0fa9d349b1ed67ec8e7eb4b703cc40ac";

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

/// 内嵌公钥对应的验签键（正式种子派生，2026-11-06 轮换）。
fn pubkey() -> VerifyingKey {
    let bytes: [u8; 32] = hex_dec(LICENSE_PUBKEY_HEX)
        .try_into()
        .expect("LICENSE_PUBKEY_HEX 必须是 32 字节 hex");
    VerifyingKey::from_bytes(&bytes).expect("内嵌公钥必须可解析")
}

/// 验签：正式公钥为唯一锚点；debug 构建（dev/e2e）额外接受开发种子
/// （公开可复现，仅本地开发），release 构建编译期剔除该分支——
/// 发布版拒绝一切非正式种子签发的码。
fn verify_sig(payload: &[u8], sig: &Signature) -> bool {
    if pubkey().verify_strict(payload, sig).is_ok() {
        return true;
    }
    #[cfg(debug_assertions)]
    if dev_key()
        .verifying_key()
        .verify_strict(payload, sig)
        .is_ok()
    {
        return true;
    }
    false
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
    if !verify_sig(&payload, &sig) {
        return Err(LicenseError::BadSignature);
    }
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

/// 开发默认种子的 hex（e2e/测试签发用；debug 构建可验签，release 被拒）。
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

/// 从签名种子派生公钥 hex（密钥轮换 SOP 用：种子→公钥，单向）。
/// 供 `examples/show_pubkey.rs` 使用；调用方负责种子的保管与清痕。
#[doc(hidden)]
pub fn pubkey_hex_from_seed(seed_hex: &str) -> anyhow::Result<String> {
    use ed25519_dalek::SigningKey;
    let key = SigningKey::from_bytes(&decode_seed(seed_hex)?);
    Ok(hex_enc(&key.verifying_key().to_bytes()))
}

/// 开发公钥 = dev_seed 派生（debug 构建验签兜底对应键；与正式公钥不同）。
pub fn dev_pubkey_hex() -> String {
    hex_enc(&dev_key().verifying_key().to_bytes())
}

fn dev_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&dev_seed())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 密钥轮换双档位锚定（2026-11-06 轮换）：
    /// - debug 构建：开发种子签发 → dev 兜底验签通过（锚定 e2e/测试路径可用），
    ///   且 dev 公钥 ≠ 正式公钥（兜底不是回退到旧正式钥）；
    /// - release 构建：开发种子签发 → **被拒**（上线检查清单第 1 条：发布版
    ///   只认正式种子）。
    ///
    /// 公共断言（两档位都成立）：随机密钥拒绝、篡改拒绝、过期、格式错。
    #[test]
    fn dev_roundtrip_and_guards() {
        let payload = serde_json::json!({
            "plan": "pro", "name": "开发测试", "devices_max": 99,
            "expires": null, "issued": 1_730_000_000u64
        })
        .to_string();

        #[cfg(debug_assertions)]
        {
            let code = sign_payload(&payload, &dev_seed_hex()).expect("签发成功");
            let license = verify_license(&code, 1_730_000_100).expect("debug 下开发码应验签通过");
            assert_eq!(license.plan, Plan::Pro);
            assert_eq!(license.devices_max, 99);

            // dev 兜底与正式公钥必须是两把不同的钥匙
            assert_ne!(
                dev_pubkey_hex(),
                LICENSE_PUBKEY_HEX,
                "开发公钥不得与正式公钥相同（兜底失去意义）"
            );
        }
        #[cfg(not(debug_assertions))]
        {
            let code = sign_payload(&payload, &dev_seed_hex()).expect("签发成功");
            assert!(
                matches!(
                    verify_license(&code, 1_730_000_100),
                    Err(LicenseError::BadSignature)
                ),
                "release 构建必须拒绝开发种子签发的码"
            );
        }

        // 非本产品签发（随机密钥）→ 验签失败（两档位都成立）
        let other_seed = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
        let forged = sign_payload(&payload, other_seed).expect("签名成功");
        assert!(
            matches!(
                verify_license(&forged, 1_730_000_100),
                Err(LicenseError::BadSignature)
            ),
            "随机种子签的码必须被拒"
        );

        // 篡改 payload → 验签失败
        let mut tampered = sign_payload(&payload, &dev_seed_hex()).expect("签发成功");
        tampered.replace_range(6..8, "XX");
        assert!(matches!(
            verify_license(&tampered, 1_730_000_100),
            Err(LicenseError::BadSignature) | Err(LicenseError::Format)
        ));

        // 过期 → Expired 且带人话时长（用随机种子签，走公共拒绝路径之外的成功
        // 路径：debug 下 dev 兜底通过；release 下……会被拒——故此断言仅 debug）
        #[cfg(debug_assertions)]
        {
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
        }

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
