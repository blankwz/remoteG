//! rd-window-win — 窗口枚举、事件跟踪、窗口组、1:1 适配 resize。

#[cfg(windows)]
pub mod enumerate;
#[cfg(windows)]
pub mod events;
#[cfg(windows)]
pub mod group;
#[cfg(windows)]
pub mod resize;
