//! 身份：自签 Ed25519 证书的生成与指纹（05_api §1、11_security 安全方案）。
//!
//! 指纹 = SHA-256(证书 DER)，用于应用层信任列表比对（持久化与配对由 T-013 负责，
//! 本模块只负责内存态的生成与序列化）。

use std::fmt;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// 对端/本机证书指纹：SHA-256(证书 DER)。
///
/// [`Display`](fmt::Display) 输出 64 字符小写 hex；UI 展示用 [`Fingerprint::short_hex`]
/// （前 8 字节，16 字符）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    /// 对一段证书 DER 计算 SHA-256 指纹。
    pub fn from_cert_der(cert_der: &[u8]) -> Self {
        Self(Sha256::digest(cert_der).into())
    }

    /// 完整 64 字符小写 hex。
    pub fn hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for b in self.0 {
            use fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// 前 8 字节 hex（16 字符），用于 UI 展示。
    pub fn short_hex(&self) -> String {
        let mut out = String::with_capacity(16);
        for b in self.0.iter().take(8) {
            use fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.hex())
    }
}

/// 本机身份：自签 Ed25519 证书 + PKCS#8 私钥（均为 DER）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl Identity {
    /// 生成一对新的 Ed25519 自签证书。
    ///
    /// CN 为 `name`；有效期固定为 2024-01-01 ~ 2124-01-01（自签体系以指纹为信任锚，
    /// 宽有效期避免时钟偏移导致握手失败）。
    pub fn generate(name: &str) -> anyhow::Result<Self> {
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2124, 1, 1);
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let cert = params.self_signed(&key_pair)?;
        Ok(Self {
            cert_der: cert.der().to_vec(),
            key_der: key_pair.serialize_der(),
        })
    }

    /// 本机证书指纹。
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::from_cert_der(&self.cert_der)
    }

    /// 转为 rustls 配置所需的 DER 类型。
    pub(crate) fn rustls_identity(
        &self,
    ) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let cert = CertificateDer::from(self.cert_der.clone());
        let key = PrivateKeyDer::Pkcs8(self.key_der.clone().into());
        Ok((vec![cert], key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_hex_and_short() {
        let fp = Fingerprint::from_cert_der(b"hello");
        assert_eq!(fp.hex().len(), 64);
        assert_eq!(fp.short_hex().len(), 16);
        assert_eq!(fp.hex()[..16], fp.short_hex());
        assert_eq!(fp.to_string(), fp.hex());
        assert_eq!(format!("{fp:?}"), format!("Fingerprint({})", fp.hex()));
        // SHA-256("hello") 的前 8 字节
        assert_eq!(fp.short_hex(), "2cf24dba5fb0a30e");
    }

    #[test]
    fn generate_produces_der_and_fingerprint() {
        let id = Identity::generate("unit-test").expect("generate");
        assert!(!id.cert_der.is_empty());
        assert!(!id.key_der.is_empty());
        assert_eq!(id.fingerprint(), Fingerprint::from_cert_der(&id.cert_der));
        // PEM 头校验：Ed25519 私钥为 PKCS#8 DER，不可读出明文
        assert_ne!(&id.key_der[..5], b"-----");
    }

    #[test]
    fn generate_is_unique() {
        let a = Identity::generate("a").expect("a");
        let b = Identity::generate("b").expect("b");
        assert_ne!(a.fingerprint(), b.fingerprint());
    }
}
