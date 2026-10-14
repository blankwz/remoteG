//! rd-transport — QUIC（quinn）封装、自签证书、控制流/数据流帧分流。

pub mod conn;
pub mod framing;
pub mod identity;
pub mod verify;
