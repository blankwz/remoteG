//! rd-controller Tauri 入口（T-005）。
//!
//! 注册全部命令：spike_*（真实实现）+ devices/windows/session/input/term 桩。
//! 后续任务只替换 `commands/` 下对应文件实现，不改本文件。

mod commands;
mod conn;
mod video_pump;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 日志：RUST_LOG 可覆盖（如 RUST_LOG=debug 看 control envelope）
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state = conn::ConnManager::new().expect("generate ephemeral controller identity");
    tracing::info!(
        fingerprint = %state.identity().fingerprint().short_hex(),
        "controller identity ready (ephemeral, persisted in T-013)"
    );

    tauri::Builder::default()
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            // T-005：视频链路 spike（真实实现）
            commands::spike::spike_connect,
            // T-019：设备
            commands::devices::list_devices,
            commands::devices::pair_device,
            commands::devices::remove_device,
            commands::devices::connect_device,
            commands::devices::disconnect_device,
            commands::devices::probe_device,
            commands::devices::set_decode_caps,
            // T-020：窗口
            commands::windows::list_windows,
            commands::windows::subscribe_window_events,
            // T-021：会话
            commands::session::open_window,
            commands::session::close_window,
            commands::session::set_viewport,
            commands::session::subscribe_group_events,
            // T-022：输入
            commands::input::mouse_move,
            commands::input::mouse_button,
            commands::input::mouse_wheel,
            commands::input::key_event,
            commands::input::text_input,
            // T-023：终端
            commands::term::open_terminal,
            commands::term::term_input,
            commands::term::term_resize,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
