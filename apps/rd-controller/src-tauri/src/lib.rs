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
use tracing_subscriber::prelude::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 日志：stdout + 按天滚动文件双写（融合版目录 %APPDATA%\rd\app\logs\rd-app.*.log，
    // 保留 7 天；托盘"打开日志目录"与此一致）。RUST_LOG=debug 可看更多细节。
    // guard 必须存活至进程结束（run 的 event loop 不返回，局部绑定即可）。
    let _log_guard: Option<tracing_appender::non_blocking::WorkerGuard> = {
        let log_dir = rd_agent::default_config_dir().join("app").join("logs");
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        match std::fs::create_dir_all(&log_dir).ok().and_then(|()| {
            tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("rd-app")
                .filename_suffix("log")
                .max_log_files(7)
                .build(&log_dir)
                .ok()
        }) {
            Some(appender) => {
                let (writer, guard) = tracing_appender::non_blocking(appender);
                tracing_subscriber::registry()
                    .with(filter)
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(writer),
                    )
                    .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
                    .init();
                Some(guard)
            }
            None => {
                tracing_subscriber::fmt().with_env_filter(filter).init();
                None
            }
        }
    };

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
            commands::devices::host_status,
            commands::devices::generate_pair_code,
            commands::devices::edit_device,
            commands::devices::tailscale_ping,
            // T-020：窗口
            commands::windows::list_windows,
            commands::windows::subscribe_window_events,
            // T-021：会话
            commands::session::open_window,
            commands::session::close_window,
            commands::session::set_viewport,
            commands::session::set_quality,
            commands::session::set_privacy,
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
