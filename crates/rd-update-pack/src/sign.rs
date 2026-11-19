//! update.json 的 ed25519 签名/验签。
//!
//! 发布端对 `update.json` 文件字节签名（旁挂 `update.json.sig`，hex）；Agent
//! 端用内置公钥锚定验证（对齐 T-036 `LICENSE_PUBKEY_HEX` 的锚定思路：公钥
//! 编译期常量，签名不对则整个更新拒绝）。公钥占位待 T-039 正式密钥替换。
//!
//! 密钥格式：ed25519 seed 32 字节 hex（64 字符）；签名 64 字节 hex（128 字符）。

use ed25519_dalek::{Signer, Verifier};

/// 签名格式错误。
#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("密钥不是 64 位 hex（ed25519 seed）")]
    BadKey,
    #[error("签名不是 128 位 hex")]
    BadSignature,
}

/// seed hex（64 字符）→ 签名 hex（128 字符）。
pub fn sign_bytes(seed_hex: &str, data: &[u8]) -> Result<String, SignError> {
    let seed = decode_32(seed_hex)?;
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let sig = key.sign(data);
    Ok(sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect())
}

/// 验证：公钥 hex（64 字符）+ 数据 + 签名 hex（128 字符）。
pub fn verify_bytes(pub_key_hex: &str, data: &[u8], sig_hex: &str) -> Result<bool, SignError> {
    let pk = decode_fixed::<32>(pub_key_hex).ok_or(SignError::BadKey)?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&pk).map_err(|_| SignError::BadKey)?;
    let sig_bytes = decode_fixed::<64>(sig_hex).ok_or(SignError::BadSignature)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    Ok(key.verify(data, &sig).is_ok())
}

fn decode_32(hex: &str) -> Result<[u8; 32], SignError> {
    decode_fixed::<32>(hex).ok_or(SignError::BadKey)
}

/// hex → 定长字节数组；长度不符或含非 hex 字符 → None。
fn decode_fixed<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// 生成一对新密钥（seed hex, 公钥 hex）——发布端初始化用（rd-pack-cli keygen）。
pub fn generate_keypair() -> (String, String) {
    let mut seed = [0u8; 32];
    // 随机性来自 OS CSPRNG（getrandom，ed25519-dalek 的依赖链自带）
    getrandom(&mut seed);
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let seed_hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    let pub_hex: String = key
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (seed_hex, pub_hex)
}

fn getrandom(buf: &mut [u8]) {
    getrandom::getrandom(buf).expect("OS 随机源不可用");
}
