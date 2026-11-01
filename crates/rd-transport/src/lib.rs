//! rd-transport — QUIC（quinn）封装、自签证书、控制流/数据流帧分流。
//!
//! - [`identity`]：Ed25519 自签身份与 SHA-256 证书指纹（信任锚，05_api §1）；
//! - [`verify`]：TLS 层自定义验证器（接受任意自签证书，校验有效期与握手签名）；
//! - [`conn`]：`Server::bind` / [`connect`]，ALPN `rd/0`，控制流 + 数据流 + datagram；
//! - [`framing`]：控制流 `u32 LE + Envelope`；数据流帧头（05_api §3）。

pub mod conn;
pub mod framing;
pub mod identity;
pub mod relay;
pub mod relay_ws;
pub mod verify;

pub use conn::{connect, Conn, ControlRx, ControlTx, DataRx, DataTx, Server, ALPN};
pub use framing::{DataFrame, StreamKind, TileHeader, VideoHeader};
pub use identity::{Fingerprint, Identity};
pub use relay::{bind_via_relay, connect_via_relay, parse_relay_addr};
pub use relay_ws::{bind_via_relay_ep, connect_via_relay_ep, parse_relay_endpoint, RelayEndpoint};
