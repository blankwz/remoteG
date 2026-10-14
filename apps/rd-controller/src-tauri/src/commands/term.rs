//! 终端会话命令（T-023 实现具体逻辑；当前为骨架空桩）。

/// 打开终端（双向 QUIC 流）。
#[tauri::command]
pub async fn open_terminal() -> Result<(), String> {
    Err("not implemented".into())
}

/// 终端输入字节。
#[tauri::command]
pub async fn term_input() -> Result<(), String> {
    Err("not implemented".into())
}

/// 终端尺寸变更。
#[tauri::command]
pub async fn term_resize() -> Result<(), String> {
    Err("not implemented".into())
}
