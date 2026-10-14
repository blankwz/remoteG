//! rd-window-win — 窗口枚举、事件跟踪、窗口组、1:1 适配 resize。
//!
//! - [`enumerate`]：可捕获顶层窗口列表（`is_capturable` 过滤规则）与窗口图标
//! - [`events`]：WinEvent 窗口事件监听（创建/销毁/移动/标题/菜单弹出）
//! - [`group`]：窗口组（根窗口 + owned 对话框 + 同线程弹出菜单）
//! - [`resize`]：1:1 适配（T-009）
//!
//! 所有坐标均为屏幕物理像素（per-monitor DPI aware 上下文），字段与
//! `rd-capture-win` 的捕获坐标系一致。

#[cfg(windows)]
pub mod enumerate;
#[cfg(windows)]
pub mod events;
#[cfg(windows)]
pub mod group;
#[cfg(windows)]
pub mod resize;

/// 矩形（物理像素）。字段与 `rd-capture-win::Rect` 相同：`x`/`y` 为左上角坐标，
/// `w`/`h` 为尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[cfg(windows)]
pub use enumerate::{is_capturable, list_windows, window_icon_png, WindowAttrs};
#[cfg(windows)]
pub use events::{spawn_event_watcher, WatcherHandle, WindowEventRaw};
#[cfg(windows)]
pub use group::{window_group, GroupMember};
