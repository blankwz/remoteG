//! 身份持久化：`identity.bin` 的加密读写（11_security「私钥泄露」对策，T-013）。
//!
//! - Windows：整个 DER 包（证书 + 私钥）用 DPAPI `CryptProtectData` 加密，
//!   绑定当前用户，无额外 nonce/版本字节；
//! - macOS：用钥匙串（`keyring` crate）按目录存取 hex 编码的 DER 包，
//!   `identity.bin` 只留一个无敏感信息的标记；
//! - 其他平台：明文落盘（文件权限 600）并打印警告，日志不含任何密钥材料。
//!
//! 明文包格式：`[u32 LE cert_len][u32 LE key_len][cert_der][key_der]`，加载时严格校验总长。

use std::fs;
use std::path::Path;

use anyhow::Context;
use rd_transport::Identity;

/// 身份文件名（Agent 用 `%LOCALAPPDATA%\rd\`，控制端由调用方传入 app data dir）。
const IDENTITY_FILE: &str = "identity.bin";

/// 加载已持久化的身份；不存在（或旧版 macOS 钥匙串条目因未签名 ACL 无法读取）
/// 则自动生成新身份并落盘，避免应用启动崩溃。
pub fn load_or_create_identity(dir: &Path, name: &str) -> anyhow::Result<Identity> {
    fs::create_dir_all(dir).with_context(|| format!("创建身份目录失败：{}", dir.display()))?;
    let path = dir.join(IDENTITY_FILE);

    if path.exists() {
        match fs::read(&path)
            .with_context(|| format!("读取身份文件失败：{}", path.display()))
            .and_then(|blob| platform::unseal(&blob, dir).context("解密身份文件失败"))
            .and_then(|plain| parse_identity(&plain).context("identity.bin 内容无效"))
        {
            Ok(identity) => return Ok(identity),
            Err(err) => {
                // 常见于 macOS 未签名版本升级/二次打开时钥匙串 ACL 拒绝，或旧文件损坏；
                // 重新生成并覆盖写入，保证应用可正常启动，不 panic 闪退。
                tracing::warn!(
                    path = %path.display(),
                    "现有身份文件无法加载（将自动重新生成）: {err:#}"
                );
            }
        }
    }

    let identity = Identity::generate(name).context("生成身份失败")?;
    let plain = serialize_identity(&identity);
    let blob = platform::seal(&plain, dir).context("加密身份失败")?;
    fs::write(&path, &blob).with_context(|| format!("写入身份文件失败：{}", path.display()))?;
    #[cfg(unix)]
    restrict_permissions(&path);

    Ok(identity)
}

/// 序列化：`[u32 LE cert_len][u32 LE key_len][cert_der][key_der]`。
fn serialize_identity(identity: &Identity) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + identity.cert_der.len() + identity.key_der.len());
    out.extend_from_slice(&(identity.cert_der.len() as u32).to_le_bytes());
    out.extend_from_slice(&(identity.key_der.len() as u32).to_le_bytes());
    out.extend_from_slice(&identity.cert_der);
    out.extend_from_slice(&identity.key_der);
    out
}

/// 反序列化：长度前缀校验总长精确匹配，拒绝截断与多余尾部。
fn parse_identity(bytes: &[u8]) -> anyhow::Result<Identity> {
    if bytes.len() < 8 {
        anyhow::bail!("长度 {} 不足以容纳两个长度前缀", bytes.len());
    }
    let cert_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let key_len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let total = 8usize
        .checked_add(cert_len)
        .and_then(|n| n.checked_add(key_len))
        .ok_or_else(|| anyhow::anyhow!("长度字段溢出"))?;
    if bytes.len() != total {
        anyhow::bail!(
            "长度不匹配：cert_len={cert_len} key_len={key_len}，应为 {total} 字节，实际 {}",
            bytes.len()
        );
    }
    Ok(Identity {
        cert_der: bytes[8..8 + cert_len].to_vec(),
        key_der: bytes[8 + cert_len..total].to_vec(),
    })
}

/// Unix 平台（macOS/Linux）：限制文件权限为 600（仅本用户可读写）。
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        tracing::warn!("identity 文件权限收紧失败：{err}");
    }
}

// ---------------------------------------------------------------------------
// 平台加密后端
// ---------------------------------------------------------------------------

/// Windows：DPAPI（CryptProtectData/CryptUnprotectData），绑定当前用户。
#[cfg(windows)]
mod platform {
    use super::*;

    pub fn seal(plain: &[u8], _dir: &Path) -> anyhow::Result<Vec<u8>> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::LocalFree;
        use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: plain.len() as u32,
                pbData: plain.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            CryptProtectData(&input, PCWSTR::null(), None, None, None, 0, &mut output)
                .context("DPAPI CryptProtectData 失败")?;

            let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
            let encrypted = slice.to_vec();
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                output.pbData.cast(),
            )));
            Ok(encrypted)
        }
    }

    pub fn unseal(blob: &[u8], _dir: &Path) -> anyhow::Result<Vec<u8>> {
        use windows::Win32::Foundation::LocalFree;
        use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: blob.len() as u32,
                pbData: blob.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            CryptUnprotectData(&input, None, None, None, None, 0, &mut output)
                .context("DPAPI CryptUnprotectData 失败（文件被其他用户/机器加密？）")?;

            let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
            let plain = slice.to_vec();
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                output.pbData.cast(),
            )));
            Ok(plain)
        }
    }
}

/// macOS：直接落盘到 `identity.bin`（权限收紧为 0600，仅当前用户可读写，
/// 与 `~/.ssh/id_ed25519` 一致）。
///
/// 不再强依赖 macOS Keychain：未公证/ad-hoc 签名的 `.app` 在第二次打开或升级后
/// Keychain 会因代码签名 ACL 不一致返回 `errSecAuthFailed`，导致二次启动直接闪退。
/// 若检测到旧版 `rd-keyring-v1` 标记文件，则兼容尝试从钥匙串迁移读取。
#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    const KEYRING_SERVICE: &str = "rd.identity";
    const LEGACY_MARKER: &[u8] = b"rd-keyring-v1";

    pub fn seal(plain: &[u8], _dir: &Path) -> anyhow::Result<Vec<u8>> {
        Ok(plain.to_vec())
    }

    pub fn unseal(blob: &[u8], dir: &Path) -> anyhow::Result<Vec<u8>> {
        if blob == LEGACY_MARKER {
            let entry = keyring::Entry::new(KEYRING_SERVICE, &dir.to_string_lossy())
                .context("打开旧版 macOS 钥匙串条目失败")?;
            let hex = entry
                .get_password()
                .context("读取旧版 macOS 钥匙串失败（将自动重建身份）")?;
            return hex_decode(&hex);
        }
        Ok(blob.to_vec())
    }

    fn hex_decode(s: &str) -> anyhow::Result<Vec<u8>> {
        if s.len() % 2 != 0 {
            anyhow::bail!("钥匙串内容不是合法 hex（奇数长度）");
        }
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(Into::into))
            .collect()
    }
}

/// 其他平台：明文 + 警告（日志不包含任何密钥材料）。
#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    pub fn seal(plain: &[u8], _dir: &Path) -> anyhow::Result<Vec<u8>> {
        tracing::warn!("identity 以明文写入磁盘：当前平台不支持加密存储");
        Ok(plain.to_vec())
    }

    pub fn unseal(blob: &[u8], _dir: &Path) -> anyhow::Result<Vec<u8>> {
        Ok(blob.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_parse_roundtrip() {
        let identity = Identity::generate("unit").expect("generate");
        let bytes = serialize_identity(&identity);
        let parsed = parse_identity(&bytes).expect("parse");
        assert_eq!(identity, parsed);
    }

    #[test]
    fn parse_rejects_truncated_and_padded() {
        let identity = Identity::generate("unit").expect("generate");
        let bytes = serialize_identity(&identity);
        // 截断
        assert!(parse_identity(&bytes[..bytes.len() - 1]).is_err());
        assert!(parse_identity(&bytes[..2]).is_err());
        // 多余尾部
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(parse_identity(&padded).is_err());
        // 空输入
        assert!(parse_identity(&[]).is_err());
    }
}
