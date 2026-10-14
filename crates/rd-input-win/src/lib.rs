//! rd-input-win — 键鼠注入（SendInput）、坐标映射、置前。

#[cfg(windows)]
pub mod focus;
#[cfg(windows)]
pub mod keyboard;
#[cfg(windows)]
pub mod mouse;
