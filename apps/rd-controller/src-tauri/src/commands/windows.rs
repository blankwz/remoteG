//! 窗口列表与窗口事件命令（T-020 实现具体逻辑；当前为骨架空桩）。

/// 列出被控端顶层与子窗口。
#[tauri::command]
pub async fn list_windows() -> Result<(), String> {
    Err("not implemented".into())
}

/// 订阅窗口事件流（CREATED/DESTROYED/MOVED/TITLE/...）。
#[tauri::command]
pub async fn subscribe_window_events() -> Result<(), String> {
    Err("not implemented".into())
}
