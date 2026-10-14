//! rd-term — ConPTY 终端会话、VT 字节流、断线重连环形缓冲。
//!
//! - [`TermManager`]：会话生命周期、输出广播、空闲回收（session.rs）
//! - [`RingBuffer`]：1 MiB 回放缓冲，截断对齐转义序列边界（ring.rs）
//! - `conpty`：portable-pty 封装，Windows ConPTY / Unix openpty（conpty.rs）

pub mod conpty;
pub mod ring;
pub mod session;

pub use conpty::spawn_pty;
pub use ring::RingBuffer;
pub use session::{TermManager, TermOutput};
