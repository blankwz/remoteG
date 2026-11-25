//! rd-mac — macOS 原生被控端系统层（宪法 #4：Mac 既可做控制端，也可做被控端）。
//!
//! 对齐 Windows 侧 `rd-capture-win` / `rd-window-win` / `rd-input-win` 契约，
//! 通过 CoreGraphics / CoreFoundation / ApplicationServices 原生 C FFI 实现：
//! - [`capture`]：按 `CGWindowID` 逐窗口高刷捕获（最高 144fps，BGRA8888 直出）；
//! - [`window`]：`CGWindowListCopyWindowInfo` 顶层窗口枚举、事件轮询、缩放/账本/静默窗；
//! - [`input`]：`CGEventPost` 鼠标/滚轮/连续滚动/PS2 扫描码→macOS Virtual Keycode/Unicode 文本注入；
//! - [`permissions`]：屏幕录制与辅助功能权限检测。

pub mod capture;
#[cfg(target_os = "macos")]
pub(crate) mod ffi;
pub mod input;
pub mod permissions;
pub mod window;
