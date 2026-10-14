//! 证书校验策略（05_api §1、11_security 安全方案）：
//!
//! 信任锚是**证书指纹信任列表**（应用层，T-013），而非 CA。因此 TLS 层的自定义验证器
//! 接受**任意自签证书**，只做两项机械校验：
//! 1. 证书在有效期内（解析 X.509 `validity`，DER 手写解析，见 [`cert_validity`]）；
//! 2. TLS 1.3 握手签名有效（CertificateVerify，用 rustls provider 的 webpki 算法表）。
//!
//! 对端指纹由 [`crate::Conn::peer_fingerprint`] 暴露给应用层比对信任列表；
//! 客户端可传 `expected` 指纹，不匹配则在握手期直接失败。

use std::fmt;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};

use crate::identity::Fingerprint;

/// rd 的双向证书验证器：同时实现服务端（校验客户端证书）与客户端（校验服务端证书）
/// 两个 rustls danger trait。
///
/// - `expected = None`：接受任意自签证书，指纹交应用层（服务端场景 / 未配对首连）。
/// - `expected = Some(fp)`：对端证书指纹必须等于 `fp`，否则握手失败（控制端直连场景）。
#[derive(Clone, Copy)]
pub struct PeerVerifier {
    algs: WebPkiSupportedAlgorithms,
    expected: Option<Fingerprint>,
}

impl PeerVerifier {
    pub fn new(algs: WebPkiSupportedAlgorithms, expected: Option<Fingerprint>) -> Self {
        Self { algs, expected }
    }

    fn check_end_entity(&self, cert: &CertificateDer<'_>, now: UnixTime) -> Result<(), Error> {
        let (not_before, not_after) = cert_validity(cert.as_ref())
            .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?;
        if now < not_before {
            return Err(Error::InvalidCertificate(
                CertificateError::NotValidYetContext {
                    time: now,
                    not_before,
                },
            ));
        }
        if now > not_after {
            return Err(Error::InvalidCertificate(
                CertificateError::ExpiredContext {
                    time: now,
                    not_after,
                },
            ));
        }
        if let Some(expected) = self.expected {
            if Fingerprint::from_cert_der(cert.as_ref()) != expected {
                return Err(Error::General(
                    "rd-transport: peer certificate fingerprint mismatch".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn verify_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }
}

impl fmt::Debug for PeerVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerVerifier")
            .field("expected", &self.expected.map(|fp| fp.short_hex()))
            .finish()
    }
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.check_end_entity(end_entity, now)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl ClientCertVerifier for PeerVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check_end_entity(end_entity, now)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid X.509 certificate DER: {0}")]
pub struct CertParseError(&'static str);

const ERR_TRUNCATED: &str = "truncated";
const ERR_UNEXPECTED: &str = "unexpected structure";

/// 从证书 DER 中解析有效期 `(not_before, not_after)`。
///
/// 仅支持确定性 DER 编码的 X.509 v3 证书（RFC 5280）：TBSCertificate 中
/// `[0] version`（可选）、`serialNumber`、`signature`、`issuer` 之后即为
/// `validity ::= SEQUENCE { notBefore, notAfter }`，时间为 UTCTime（年份 <2050）
/// 或 GeneralizedTime，均要求 `...Z`（UTC）形式。
pub fn cert_validity(der: &[u8]) -> Result<(UnixTime, UnixTime), CertParseError> {
    let err = || CertParseError(ERR_TRUNCATED);
    let cert = read_tlv(der).ok_or_else(err)?;
    expect_tag(cert, 0x30)?;
    let tbs = read_tlv(cert.content).ok_or_else(err)?;
    expect_tag(tbs, 0x30)?;

    let mut cur = tbs.content;
    // 可选 [0] EXPLICIT version
    if let Some(first) = read_tlv(cur) {
        if first.tag == 0xA0 {
            cur = first.rest;
        }
    }
    let serial = read_tlv(cur).ok_or_else(err)?;
    expect_tag(serial, 0x02)?;
    let sig_alg = read_tlv(serial.rest).ok_or_else(err)?;
    expect_tag(sig_alg, 0x30)?;
    let issuer = read_tlv(sig_alg.rest).ok_or_else(err)?;
    expect_tag(issuer, 0x30)?;
    let validity = read_tlv(issuer.rest).ok_or_else(err)?;
    expect_tag(validity, 0x30)?;

    let not_before = read_tlv(validity.content).ok_or_else(err)?;
    let not_after = read_tlv(not_before.rest).ok_or_else(err)?;
    let nb = parse_asn1_time(not_before.tag, not_before.content)?;
    let na = parse_asn1_time(not_after.tag, not_after.content)?;
    Ok((nb, na))
}

#[derive(Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
    rest: &'a [u8],
}

fn expect_tag(tlv: Tlv<'_>, tag: u8) -> Result<Tlv<'_>, CertParseError> {
    if tlv.tag == tag {
        Ok(tlv)
    } else {
        Err(CertParseError(ERR_UNEXPECTED))
    }
}

/// 解析一个确定性 DER TLV（只支持定长长度，长度 ≤ u32::MAX）。
fn read_tlv(buf: &[u8]) -> Option<Tlv<'_>> {
    if buf.len() < 2 {
        return None;
    }
    let tag = buf[0];
    let (len, header_len) = if buf[1] & 0x80 == 0 {
        (usize::from(buf[1]), 2)
    } else {
        let n = usize::from(buf[1] & 0x7F);
        if n == 0 || n > 4 || buf.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for byte in &buf[2..2 + n] {
            len = (len << 8) | usize::from(*byte);
        }
        (len, 2 + n)
    };
    let content_end = header_len.checked_add(len)?;
    if buf.len() < content_end {
        return None;
    }
    Some(Tlv {
        tag,
        content: &buf[header_len..content_end],
        rest: &buf[content_end..],
    })
}

/// 解析 UTCTime(0x17，`YYMMDDHHMMSSZ`) / GeneralizedTime(0x18，`YYYYMMDDHHMMSSZ`)。
fn parse_asn1_time(tag: u8, raw: &[u8]) -> Result<UnixTime, CertParseError> {
    let s = std::str::from_utf8(raw).map_err(|_| CertParseError(ERR_UNEXPECTED))?;
    let (year, rest) = match tag {
        0x17 => {
            if s.len() < 13 || !s.ends_with('Z') {
                return Err(CertParseError(ERR_UNEXPECTED));
            }
            let yy: i64 = s[0..2]
                .parse()
                .map_err(|_| CertParseError(ERR_UNEXPECTED))?;
            let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
            (year, &s[2..])
        }
        0x18 => {
            if s.len() < 15 || !s.ends_with('Z') {
                return Err(CertParseError(ERR_UNEXPECTED));
            }
            let year: i64 = s[0..4]
                .parse()
                .map_err(|_| CertParseError(ERR_UNEXPECTED))?;
            (year, &s[4..])
        }
        _ => return Err(CertParseError(ERR_UNEXPECTED)),
    };
    if rest.len() < 10 {
        return Err(CertParseError(ERR_TRUNCATED));
    }
    let num = |r: &str| -> Result<i64, CertParseError> {
        r.parse().map_err(|_| CertParseError(ERR_UNEXPECTED))
    };
    let month = num(&rest[0..2])?;
    let day = num(&rest[2..4])?;
    let hour = num(&rest[4..6])?;
    let minute = num(&rest[6..8])?;
    let second = num(&rest[8..10])?;
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    // UnixTime 为 epoch 起的无符号秒；epoch 前的时刻钳到 0（对 notBefore/notAfter
    // 的比较语义等价：视为“自 epoch 起即有效/已过期”）
    let secs = u64::try_from(secs.max(0)).map_err(|_| CertParseError(ERR_UNEXPECTED))?;
    Ok(UnixTime::since_unix_epoch(std::time::Duration::from_secs(
        secs,
    )))
}

/// 平格里历 → Unix 天数（Howard Hinnant `days_from_civil`）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_known_values() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 1, 1), 20_454);
    }

    #[test]
    fn parse_utc_time() {
        let t = parse_asn1_time(0x17, b"260101000000Z").expect("parse");
        assert_eq!(t.as_secs(), 1_767_225_600); // 2026-01-01T00:00:00Z
                                                // 49 以下属 21 世纪
        let t = parse_asn1_time(0x17, b"490101000000Z").expect("parse");
        assert_eq!(t.as_secs(), 2_493_072_000); // 2049-01-01
                                                // 50 起属 20 世纪；epoch 前的 1950 钳到 0
        let t = parse_asn1_time(0x17, b"500101000000Z").expect("parse");
        assert_eq!(t.as_secs(), 0); // 1950-01-01 → epoch 前，钳为 0
    }

    #[test]
    fn parse_generalized_time() {
        let t = parse_asn1_time(0x18, b"21240101000000Z").expect("parse");
        assert_eq!(t.as_secs(), 4_859_740_800); // 2124-01-01T00:00:00Z（56247 天）
    }

    #[test]
    fn parse_time_rejects_garbage() {
        assert!(parse_asn1_time(0x17, b"zzzz").is_err());
        assert!(parse_asn1_time(0x18, b"21240101000000").is_err()); // 缺 Z
        assert!(parse_asn1_time(0x05, b"260101000000Z").is_err()); // 非 Time 标签
    }

    /// 生成的自签证书必须能解出与 rcgen 参数一致的有效期
    /// （not_before=UTCTime <2050，not_after=GeneralizedTime ≥2050，两种编码都被覆盖）。
    #[test]
    fn generated_cert_validity_roundtrip() {
        let id = crate::identity::Identity::generate("verify-test").expect("generate");
        let (nb, na) = cert_validity(&id.cert_der).expect("parse validity");
        assert_eq!(
            nb.as_secs(),
            u64::try_from(days_from_civil(2024, 1, 1) * 86_400).unwrap()
        );
        assert_eq!(
            na.as_secs(),
            u64::try_from(days_from_civil(2124, 1, 1) * 86_400).unwrap()
        );
        let now = UnixTime::now();
        assert!(nb <= now && now <= na);
    }

    #[test]
    fn cert_validity_rejects_garbage() {
        assert!(cert_validity(b"not a cert").is_err());
        assert!(cert_validity(&[]).is_err());
        // 合法 SEQUENCE 但内容截断
        assert!(cert_validity(&[0x30, 0x03, 0x30, 0x01, 0x02]).is_err());
    }

    /// PeerVerifier 在证书过期时应报 Expired；expected 指纹不匹配时报错。
    #[test]
    fn verifier_rejects_expired_and_mismatched() {
        let provider = rustls::crypto::ring::default_provider();
        let algs = provider.signature_verification_algorithms;
        let wrong_fp = Fingerprint([9u8; 32]);

        let id = crate::identity::Identity::generate("verifier").expect("generate");
        let cert = CertificateDer::from(id.cert_der.clone());

        // 有效证书 + 错误 expected → 指纹不匹配
        let v = PeerVerifier::new(algs, Some(wrong_fp));
        let err = v
            .check_end_entity(&cert, UnixTime::now())
            .expect_err("mismatch must fail");
        assert!(err.to_string().contains("fingerprint mismatch"));

        // 正确 expected（或 None）→ 通过
        let v = PeerVerifier::new(algs, Some(id.fingerprint()));
        assert!(v.check_end_entity(&cert, UnixTime::now()).is_ok());
        let v = PeerVerifier::new(algs, None);
        assert!(v.check_end_entity(&cert, UnixTime::now()).is_ok());

        // 有效期外的时刻 → NotValidYet / Expired
        let v = PeerVerifier::new(algs, None);
        let before = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1));
        assert!(v.check_end_entity(&cert, before).is_err());
        let after = UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            u64::try_from(days_from_civil(2124, 1, 2) * 86_400).unwrap(),
        ));
        assert!(v.check_end_entity(&cert, after).is_err());

        // 垃圾 DER → 解析失败
        let bad = CertificateDer::from(b"garbage".to_vec());
        assert!(v.check_end_entity(&bad, UnixTime::now()).is_err());
    }
}
