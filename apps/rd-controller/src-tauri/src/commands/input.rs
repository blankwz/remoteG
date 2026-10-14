//! 输入注入命令（T-022 实现具体逻辑；当前为骨架空桩）。

/// 鼠标移动（QUIC datagram）。
#[tauri::command]
pub async fn mouse_move() -> Result<(), String> {
    Err("not implemented".into())
}

/// 鼠标按键（控制流，可靠）。
#[tauri::command]
pub async fn mouse_button() -> Result<(), String> {
    Err("not implemented".into())
}

/// 滚轮。
#[tauri::command]
pub async fn mouse_wheel() -> Result<(), String> {
    Err("not implemented".into())
}

/// 键盘事件。
#[tauri::command]
pub async fn key_event() -> Result<(), String> {
    Err("not implemented".into())
}

/// 文本输入。
#[tauri::command]
pub async fn text_input() -> Result<(), String> {
    Err("not implemented".into())
}
