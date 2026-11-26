// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 必须在创建任何顶层窗口 / WebView2 之前声明进程级 Per-Monitor-V2 DPI 感知，
    // 确保融合版内嵌被控核心在高分屏（125%/150%/200% 缩放）下捕获与键鼠注入均使用 1:1 物理像素。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    rd_controller_lib::run()
}
