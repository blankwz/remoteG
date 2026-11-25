//! rd-capture-win — Windows Graphics Capture（WGC）per-HWND 窗口捕获。
//!
//! 按 HWND 持续输出**客户区** BGRA 帧（带时间戳、尺寸变化通知）。
//!
//! 调用约定：`WindowCapture::start` 可在任意线程调用；库内部将线程初始化为
//! COM MTA（已初始化则保持原样），客户区几何查询按 per-monitor DPI aware
//! 执行（线程级临时切换，不影响进程 DPI 设置）。

#[cfg(windows)]
pub mod crop;
#[cfg(windows)]
pub mod wgc;

#[cfg(windows)]
pub use crop::Rect;
#[cfg(windows)]
pub use wgc::{
    is_supported, list_monitors, CaptureOptions, CapturedFrame, MonitorInfo, WindowCapture,
};
