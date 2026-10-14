//! rd-term — ConPTY 终端会话、VT 字节流、断线重连环形缓冲。

#[cfg(windows)]
pub mod conpty;
pub mod ring;
pub mod session;
