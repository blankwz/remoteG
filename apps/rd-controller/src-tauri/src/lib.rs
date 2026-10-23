//! rd-app（融合版，D-013 F-2）：Tauri 壳 = 控制端 UI + 被控核心（agent lib）。
//!
//! 被控核心在 setup 阶段启动（融合身份目录 `%APPDATA%\rd\app\`），托盘/主窗口
//! 由本壳承担；单实例靠端口 21118 绑定失败判定（提示已有实例后退出）。

mod commands;
mod conn;
mod tray_fusion;
mod video_pump;

use std::sync::Arc;

use tauri::Manager;

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
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(|app| {
            // 被控核心启动（F-2）：融合身份目录独立于 bin 版（K3，干净起步）
            let config_dir = rd_agent::default_config_dir().join("app");
            let agent =
                tauri::async_runtime::block_on(async { rd_agent::start(&config_dir, None, None) });
            match agent {
                Ok(runtime) => {
                    tracing::info!(
                        listen = ?runtime.bound,
                        "被控核心已启动(融合模式)"
                    );
                    app.manage(Arc::clone(&runtime.state));
                    let state_for_tray = Arc::clone(&runtime.state);
                    tray_fusion::build(app.handle(), state_for_tray)?;
                    // AgentRuntime 保活(常驻到 app 退出;丢句柄=状态与 peers 表引用断)
                    app.manage(runtime);
                }
                Err(err) => {
                    // 端口被占 = 已有实例(单实例锁,K7);其余错误同样致命提示
                    tracing::error!("被控核心启动失败: {err:#}");
                    return Err(err.into());
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // T-005：视频链路 spike（真实实现）
            commands::spike::spike_connect,
            // T-019：设备
            commands::devices::list_devices,
            commands::devices::discover_devices,
            commands::devices::pair_device,
            commands::devices::remove_device,
            commands::devices::connect_device,
            commands::devices::disconnect_device,
            commands::devices::probe_device,
            commands::devices::set_decode_caps,
            commands::devices::send_clipboard,
            commands::devices::request_clipboard,
            // T-020：窗口
            commands::windows::list_windows,
            commands::windows::subscribe_window_events,
            // T-021：会话
            commands::session::open_window,
            commands::session::close_window,
            commands::session::set_viewport,
            commands::session::subscribe_group_events,
            commands::session::subscribe_session_state,
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
