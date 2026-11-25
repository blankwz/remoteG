//! rd-app（融合版，D-013 F-2）：Tauri 壳 = 控制端 UI + 被控核心（agent lib）。
//!
//! 被控核心在 setup 阶段启动（融合身份目录 `%APPDATA%\rd\app\`），托盘/主窗口
//! 由本壳承担；单实例靠端口 21118 绑定失败判定（提示已有实例后退出）。

mod commands;
mod conn;
mod platform;
#[cfg(windows)]
mod tray_fusion;
mod video_pump;

#[cfg(any(windows, target_os = "macos"))]
use std::sync::Arc;

#[cfg(any(windows, target_os = "macos"))]
use tauri::Manager;
use tracing_subscriber::prelude::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 日志：stdout + 按天滚动文件双写（保留 7 天）。RUST_LOG=debug 可看更多细节。
    // guard 必须存活至进程结束（run 的 event loop 不返回，局部绑定即可）。
    let _log_guard: Option<tracing_appender::non_blocking::WorkerGuard> = {
        let log_dir = platform::app_log_dir();
        // 崩溃上报（T-040）：panic hook → logs/crash/*.json（元数据，无内存转储）
        rd_crash::install_panic_hook(
            "rd-controller",
            env!("CARGO_PKG_VERSION"),
            rd_crash::crash_dir(&log_dir),
        );
        // 人为崩溃测试开关（T-040 验收）：RD_TEST_CRASH=1 验证 dump 落盘。
        // 仅 debug 构建可达（code-review LOW：release 收紧）
        #[cfg(debug_assertions)]
        if std::env::var("RD_TEST_CRASH").as_deref() == Ok("1") {
            panic!("RD_TEST_CRASH 触发的测试崩溃（验证 dump 落盘）");
        }
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

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init());

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

    builder
        .manage(state)
        .setup(|_app| {
            // 被控核心启动（Windows 与 macOS 融合版均内嵌 rd-agent；iOS / Android 作为纯控制端运行）
            #[cfg(any(windows, target_os = "macos"))]
            {
                let config_dir = platform::app_config_dir();
                let agent = tauri::async_runtime::block_on(async {
                    rd_agent::start(&config_dir, None, None)
                });
                match agent {
                    Ok(runtime) => {
                        tracing::info!(
                            listen = ?runtime.bound,
                            "被控核心已启动(融合模式)"
                        );
                        _app.manage(Arc::clone(&runtime.state));
                        #[cfg(windows)]
                        {
                            let state_for_tray = Arc::clone(&runtime.state);
                            tray_fusion::build(_app.handle(), state_for_tray)?;
                        }
                        // AgentRuntime 保活(常驻到 app 退出;丢句柄=状态与 peers 表引用断)
                        _app.manage(runtime);
                    }
                    Err(err) => {
                        // 优雅降级：端口被占时控制端照常可用，绝不闪退
                        tracing::warn!(
                            "被控核心未启动(已有实例或端口/目录冲突)，本窗口仅作为控制端: {err:#}"
                        );
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // T-040：崩溃报告
            commands::crash::crash_dumps,
            commands::crash::crash_clear,
            commands::crash::crash_report_url,
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
            commands::devices::local_clipboard_read,
            commands::devices::local_clipboard_write,
            commands::devices::open_studio,
            commands::devices::set_media_fec,
            commands::devices::get_media_fec,
            commands::devices::get_relay_addr,
            commands::devices::set_relay_addr,
            commands::devices::get_easytier,
            commands::devices::set_easytier,
            commands::devices::get_license_status,
            commands::devices::activate_license,
            commands::devices::host_status,
            commands::devices::generate_pair_code,
            commands::devices::set_host_paused,
            commands::devices::edit_device,
            commands::devices::tailscale_ping,
            commands::devices::wake_device,
            // T-031：文件传输
            commands::files::file_list,
            commands::files::file_send,
            commands::files::file_pull,
            commands::files::file_cancel,
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
            commands::touch::touch_gesture,
            commands::session::subscribe_vdisplay_state,
            commands::session::set_display_width,
            commands::input::key_event,
            commands::input::text_input,
            // T-023：终端
            commands::term::open_terminal,
            commands::term::term_input,
            commands::term::term_resize,
            // T-038：更新
            commands::update::restart_app,
            // macOS 首次隐私权限检测与一键直达《隐私与安全性》
            platform::get_mac_permissions,
            platform::open_mac_privacy_settings,
            // 宪法 #3：SSH 一键部署自建中转服务器
            commands::relay_deploy::deploy_relay_ssh,
            // v0.4：方案实验室（RD Geek Lab 原生极客台）
            commands::lab::lab_get_state,
            commands::lab::lab_apply_tuning,
            commands::lab::lab_apply_transport,
            commands::lab::lab_set_netem,
            commands::lab::lab_panic_reset,
            commands::lab::lab_start_sandbox,
            commands::lab::lab_stop_sandbox,
            commands::lab::lab_run_ab_bench,
            commands::lab::lab_benchmark_all_routes,
            commands::lab::lab_list_profiles,
            commands::lab::lab_save_profile,
            commands::lab::lab_delete_profile,
            commands::lab::lab_probe_target_plan,
            commands::lab::lab_fetch_public_nodes,
            commands::lab::lab_probe_node,
            // 方案 B：EasyTier 一键组网口令 & 在线成员探测
            commands::devices::easytier_detect_status,
            commands::devices::easytier_generate_invite,
            commands::devices::easytier_join_invite,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
