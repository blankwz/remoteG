//! rd-security — 身份持久化、信任列表与 SPAKE2 配对（05_api §1、11_security 安全方案）。
//!
//! - [`store`]：身份证书的加密持久化（Windows DPAPI / macOS 钥匙串），`identity.bin`；
//! - [`trust`]：对端证书指纹信任列表（`trusted.json`，临时文件 + rename 原子写）；
//! - [`pairing`]：6 位一次性配对码 + SPAKE2（Ed25519）+ HMAC-SHA256 指纹绑定确认，
//!   以及 5 次失败锁定 10 分钟（Agent 全局）。

pub mod pairing;
pub mod store;
pub mod trust;

pub use pairing::{AgentPairing, ControllerPairing, PairError, PairStep};
pub use store::load_or_create_identity;
pub use trust::{TrustStore, TrustedPeer};
