//! 设备发现与配对命令（T-019 实现具体逻辑；当前为骨架空桩）。
//!
//! 命令已在 lib.rs 注册，后续任务只替换本文件实现，不改 lib.rs。

/// 列出已配对设备。
#[tauri::command]
pub async fn list_devices() -> Result<(), String> {
    Err("not implemented".into())
}

/// 用托盘显示的 6 位配对码配对新设备。
#[tauri::command]
pub async fn pair_device() -> Result<(), String> {
    Err("not implemented".into())
}

/// 从信任列表移除设备。
#[tauri::command]
pub async fn remove_device() -> Result<(), String> {
    Err("not implemented".into())
}

/// 连接指定设备（建立 QUIC 并登记到 ConnManager）。
#[tauri::command]
pub async fn connect_device() -> Result<(), String> {
    Err("not implemented".into())
}

/// 断开指定设备。
#[tauri::command]
pub async fn disconnect_device() -> Result<(), String> {
    Err("not implemented".into())
}

/// 探测设备可达性。
#[tauri::command]
pub async fn probe_device() -> Result<(), String> {
    Err("not implemented".into())
}

/// 设置解码能力（上报 Hello.decode_caps）。
#[tauri::command]
pub async fn set_decode_caps() -> Result<(), String> {
    Err("not implemented".into())
}
