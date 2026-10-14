//! 会话（窗口打开/关闭/视口）命令（T-021 实现具体逻辑；当前为骨架空桩）。

/// 打开窗口会话（OpenWindow → OpenWindowAck）。
#[tauri::command]
pub async fn open_window() -> Result<(), String> {
    Err("not implemented".into())
}

/// 关闭窗口会话。
#[tauri::command]
pub async fn close_window() -> Result<(), String> {
    Err("not implemented".into())
}

/// 上报视口（ViewportInfo）。
#[tauri::command]
pub async fn set_viewport() -> Result<(), String> {
    Err("not implemented".into())
}

/// 订阅窗口组事件流。
#[tauri::command]
pub async fn subscribe_group_events() -> Result<(), String> {
    Err("not implemented".into())
}
