//! rd-proto — 协议消息（prost 生成，源 proto 见仓库根 `proto/rd.proto`）。

pub mod v0 {
    include!(concat!(env!("OUT_DIR"), "/rd.v0.rs"));
}

/// 完整桌面（显示器）的虚拟 `window_id` 基址（高 32 位为 `0xFFFF_0000`，低 32 位为显示器索引 0, 1, 2…）。
///
/// 与 Windows HWND / macOS CGWindowID 零冲突，全链路协议（`ListWindows`/`OpenWindow`/视频流/键鼠）无缝复用。
pub const DESKTOP_WINDOW_ID_BASE: u64 = 0xFFFF_0000_0000_0000;

/// 生成指定显示器索引（0 = 主显示器）对应的虚拟 `window_id`。
pub const fn desktop_window_id(monitor_index: u32) -> u64 {
    DESKTOP_WINDOW_ID_BASE | (monitor_index as u64)
}

/// 若 `window_id` 代表完整桌面显示器，返回其显示器索引（0, 1, 2…）。
pub const fn desktop_monitor_index(window_id: u64) -> Option<u32> {
    if (window_id & 0xFFFF_FFFF_0000_0000) == DESKTOP_WINDOW_ID_BASE {
        Some((window_id & 0xFFFF_FFFF) as u32)
    } else {
        None
    }
}

/// 判断 `window_id` 是否为完整桌面（整屏）会话。
pub const fn is_desktop_window_id(window_id: u64) -> bool {
    desktop_monitor_index(window_id).is_some()
}
