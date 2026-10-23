//! 融合模式托盘（D-013 F-3）：把 rd-agent bin 的托盘能力移植到 Tauri tray API。
//!
//! 菜单：生成配对码 / 已连接（点名断开）/ 断开所有 / 暂停共享 / 开机自启 /
//! 信任设备（撤销）/ 打开日志目录 / 退出。每秒刷新连接数/图标/tooltip。
//!
//! 与 bin 版差异：配对码弹窗仍用 MessageBox（独立线程，不阻塞主线程）；
//! 「信任设备」列表与撤销走同一 AgentState。

use std::path::PathBuf;
use std::sync::Arc;

use rd_agent::session::AgentState;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
};
use windows::core::HSTRING;

/// 内嵌托盘图标资产：灰 = 空闲，绿 = 正在被控制（16/32/48 px，32bpp）。
const IDLE_ICON_BYTES: &[u8] = include_bytes!("../../../rd-agent/assets/tray.ico");
const ACTIVE_ICON_BYTES: &[u8] = include_bytes!("../../../rd-agent/assets/tray-active.ico");
/// 菜单/图标状态刷新周期。
const REFRESH_MS: u64 = 1000;

// 菜单项稳定 id
const ID_GENERATE: &str = "generate-code";
const ID_CONNECTED: &str = "connected";
const ID_DISCONNECT_ALL: &str = "disconnect-all";
const ID_PAUSE: &str = "pause";
const ID_AUTOSTART: &str = "autostart";
const ID_OPEN_LOGS: &str = "open-logs";
const ID_QUIT: &str = "quit";
/// 信任设备菜单项 id 前缀,完整形式 `{TRUST_PREFIX}<fp_hex>`。
const TRUST_PREFIX: &str = "trust-";
/// 设备菜单项 id 前缀,完整形式 `{DEVICE_ID_PREFIX}<conn_id>`。
const DEVICE_ID_PREFIX: &str = "device-";
/// HKCU Run 自启键。
const AUTOSTART_RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const AUTOSTART_VALUE: &str = "rd-agent-app";

/// HKCU Run 自启当前是否已登记。
fn autostart_enabled() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(AUTOSTART_RUN_KEY)
        .and_then(|key| key.get_value::<String, _>(AUTOSTART_VALUE))
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// 写/删 HKCU Run 自启项。
fn set_autostart(enable: bool) -> anyhow::Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let key = hkcu.open_subkey_with_flags(AUTOSTART_RUN_KEY, winreg::enums::KEY_SET_VALUE)?;
    if enable {
        let exe = std::env::current_exe().context("定位当前 exe 失败")?;
        key.set_value(AUTOSTART_VALUE, &exe.to_string_lossy().to_string())
            .context("写入 Run 键失败")?;
    } else {
        key.delete_value(AUTOSTART_VALUE)
            .context("删除 Run 键失败")?;
    }
    Ok(())
}

use anyhow::Context as _;

/// 从内嵌 .ico 解码 32bpp 帧 → RGBA（与 bin 版 decode_ico_rgba 同源）。
fn decode_ico_rgba(bytes: &[u8], prefer: u32) -> Option<(Vec<u8>, u32, u32)> {
    if bytes.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut best: Option<(u32, u32, u32, usize)> = None;
    for index in 0..count {
        let entry = 6 + index * 16;
        let dir = bytes.get(entry..entry + 16)?;
        let width = u32::from(dir[0]);
        let height = u32::from(dir[1]);
        let bit_count = u16::from_le_bytes([dir[6], dir[7]]);
        let offset = u32::from_le_bytes(dir[12..16].try_into().ok()?) as usize;
        if bit_count != 32 || width == 0 || height == 0 {
            continue;
        }
        let diff = width.abs_diff(prefer);
        if best.as_ref().is_none_or(|(bd, _, _, _)| diff < *bd) {
            best = Some((diff, width, height, offset));
        }
    }
    let (_diff, width, height, offset) = best?;
    let header_size = u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize;
    if header_size < 40 {
        return None;
    }
    let len = width as usize * height as usize * 4;
    let pixels = bytes.get(offset + header_size..offset + header_size + len)?;
    let stride = width as usize * 4;
    let rows = height as usize;
    let mut rgba = vec![0u8; len];
    for row in 0..rows {
        let src = &pixels[(rows - 1 - row) * stride..(rows - row) * stride];
        let dst = &mut rgba[row * stride..(row + 1) * stride];
        for px in 0..width as usize {
            let (s, d) = (px * 4, px * 4);
            dst[d] = src[s + 2];
            dst[d + 1] = src[s + 1];
            dst[d + 2] = src[s];
            dst[d + 3] = src[s + 3];
        }
    }
    Some((rgba, width, height))
}

fn tauri_icon(bytes: &[u8]) -> Option<tauri::image::Image<'static>> {
    let (rgba, w, h) = decode_ico_rgba(bytes, 32)?;
    Some(tauri::image::Image::new_owned(rgba, w, h))
}

/// 用资源管理器打开日志目录（融合版目录 = app 子目录）。
fn open_log_dir(app: &AppHandle) {
    let dir: PathBuf = app
        .path()
        .app_data_dir()
        .map(|d| d.join("rd").join("app").join("logs"))
        .unwrap_or_else(|_| rd_agent::default_config_dir().join("app").join("logs"));
    let _ = std::fs::create_dir_all(&dir);
    match std::process::Command::new("explorer").arg(&dir).spawn() {
        Ok(_) => tracing::info!(dir = %dir.display(), "打开日志目录"),
        Err(err) => tracing::warn!(dir = %dir.display(), "打开日志目录失败: {err}"),
    }
}

/// 配对码弹窗（独立线程;内容不落日志）+ 剪贴板复制。
fn show_pair_code_dialog(code: String, addrs: Vec<String>) {
    std::thread::spawn(move || {
        let copied = rd_agent::clipboard::write_text(&code);
        let code_line = if copied {
            format!("{code}(已复制到剪贴板)")
        } else {
            code
        };
        let addr_lines = if addrs.is_empty() {
            "(监听地址未就绪)".to_string()
        } else {
            addrs.join("\n")
        };
        let text = HSTRING::from(format!(
            "把下面地址填进控制端「添加设备」:\n{addr_lines}\n\n配对码:{code_line}"
        ));
        let title = HSTRING::from("rd · 配对");
        unsafe {
            MessageBoxW(
                None,
                &text,
                &title,
                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
            );
        }
    });
}

/// 撤销信任 + 断开在线连接（与 bin 版语义一致）。
fn revoke_trust(state: &AgentState, fp_hex: &str) {
    let mut bytes = [0u8; 32];
    if fp_hex.len() != 64 {
        return;
    }
    for (i, b) in bytes.iter_mut().enumerate() {
        match u8::from_str_radix(fp_hex.get(i * 2..i * 2 + 2).unwrap_or(""), 16) {
            Ok(v) => *b = v,
            Err(_) => return,
        }
    }
    let fp = rd_transport::Fingerprint(bytes);
    let removed = {
        let mut trust = state.trust.lock().expect("trust 锁中毒");
        trust.remove(&fp).is_ok()
    };
    let cut: Vec<_> = {
        let mut map = state.peers.lock().expect("peers 锁中毒");
        let ids: Vec<u64> = map
            .iter()
            .filter(|(_, p)| p.fp.hex() == fp.hex())
            .map(|(k, _)| *k)
            .collect();
        ids.into_iter().filter_map(|k| map.remove(&k)).collect()
    };
    for peer in &cut {
        peer.cancel.cancel();
    }
    tracing::info!(removed, online_cut = cut.len(), "撤销信任完成");
}

/// 构建托盘（菜单 + 事件 + 每秒刷新线程）。
pub fn build(app: &AppHandle, state: Arc<AgentState>) -> tauri::Result<TrayIcon> {
    let generate = MenuItem::with_id(app, ID_GENERATE, "生成配对码", true, None::<&str>)?;
    let connected = Submenu::with_id(app, ID_CONNECTED, "已连接:0", false)?;
    let disconnect_all =
        MenuItem::with_id(app, ID_DISCONNECT_ALL, "断开所有", false, None::<&str>)?;
    let paused = CheckMenuItem::with_id(app, ID_PAUSE, "暂停共享", true, false, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        ID_AUTOSTART,
        "开机自启",
        true,
        autostart_enabled(),
        None::<&str>,
    )?;
    let trust = Submenu::with_id(app, "trust", "信任设备", true)?;
    let open_logs = MenuItem::with_id(app, ID_OPEN_LOGS, "打开日志目录", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ID_QUIT, "退出", true, None::<&str>)?;

    let menu = Menu::new(app)?;
    menu.append(&generate)?;
    menu.append(&connected)?;
    menu.append(&disconnect_all)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&paused)?;
    menu.append(&autostart)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&trust)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&open_logs)?;
    menu.append(&quit)?;

    let mut builder = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("rd 远程控制待命")
        .on_menu_event(|app, event| on_menu_event(app, event.id().as_ref()))
        .on_tray_icon_event(|_tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // 左键单击:显示主窗口(融合版主交互面)
                if let Some(win) = _tray.app_handle().get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
        });
    if let Some(icon) = tauri_icon(IDLE_ICON_BYTES) {
        builder = builder.icon(icon);
    }
    let tray = builder.build(app)?;

    // 每秒刷新线程:连接数/设备子菜单/图标/tooltip
    let app2 = app.clone();
    let tray_for_thread = tray.clone();
    std::thread::spawn(move || refresh_loop(app2, state, tray_for_thread, menu));
    Ok(tray)
}

/// 托盘菜单点击分发。
fn on_menu_event(app: &AppHandle, id: &str) {
    let state = match app.try_state::<Arc<AgentState>>() {
        Some(s) => Arc::clone(s.inner()),
        None => return,
    };
    match id {
        ID_GENERATE => {
            let code = {
                let mut pairing = state.pairing.lock().expect("pairing 锁中毒");
                pairing.new_code()
            };
            tracing::info!("托盘生成新配对码(5 分钟内有效)");
            let addrs: Vec<String> = state
                .listen_addrs
                .lock()
                .expect("listen_addrs 锁中毒")
                .iter()
                .filter(|a| !a.ip().is_loopback())
                .map(|a| a.to_string())
                .collect();
            show_pair_code_dialog(code, addrs);
        }
        ID_DISCONNECT_ALL => {
            let count = state.peers.lock().expect("peers 锁中毒").len();
            for peer in state.peers.lock().expect("peers 锁中毒").values() {
                peer.cancel.cancel();
            }
            if count > 0 {
                tracing::info!(count, "已断开全部连接");
            }
        }
        ID_PAUSE => {
            // 勾选态由 muda 先翻转;以事件携带的 checkbox state 为准无法拿到,
            // 直接查菜单项:经 app.menu? 简化:读当前 paused 取反(与 bin 语义等价)
            let paused = !state.paused.load(std::sync::atomic::Ordering::SeqCst);
            state
                .paused
                .store(paused, std::sync::atomic::Ordering::SeqCst);
            if paused {
                rd_agent::term_handler::pause_all(&state);
                // 通知控制端(经各连接控制流,见 session push;此处走事件给前端)
                let _ = app.emit("session-state", 2i32);
                tracing::info!("共享已暂停(画面/输入/终端全部停止)");
            } else {
                let _ = app.emit("session-state", 3i32);
                tracing::info!("共享已恢复");
            }
        }
        ID_AUTOSTART => {
            // 勾选态已由原生翻转:读注册表确认目标状态
            let target = !autostart_enabled();
            match set_autostart(target) {
                Ok(()) => tracing::info!(enable = target, "开机自启已更新"),
                Err(err) => tracing::error!("设置开机自启失败: {err:#}"),
            }
        }
        ID_OPEN_LOGS => open_log_dir(app),
        ID_QUIT => {
            tracing::info!("托盘请求退出");
            app.exit(0);
        }
        other => {
            if let Some(conn_id) = other
                .strip_prefix(DEVICE_ID_PREFIX)
                .and_then(|rest| rest.parse::<u64>().ok())
            {
                if let Some(peer) = state.peers.lock().expect("peers 锁中毒").remove(&conn_id) {
                    tracing::info!(conn = conn_id, name = %peer.name, "托盘断开该设备");
                    peer.cancel.cancel();
                }
            } else if let Some(fp_hex) = other.strip_prefix(TRUST_PREFIX) {
                revoke_trust(&state, fp_hex);
            }
        }
    }
}

/// 每秒刷新:已连接子菜单/图标/tooltip。
fn refresh_loop(
    app: tauri::AppHandle,
    state: Arc<AgentState>,
    tray: TrayIcon,
    menu: tauri::menu::Menu<tauri::Wry>,
) {
    let idle_icon = tauri_icon(IDLE_ICON_BYTES);
    let active_icon = tauri_icon(ACTIVE_ICON_BYTES);
    let mut active = false;
    let mut signature = String::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS));
        let peers: Vec<(u64, String)> = state
            .peers
            .lock()
            .expect("peers 锁中毒")
            .iter()
            .map(|(id, p)| (*id, p.name.clone()))
            .collect();
        let count = peers.len();

        // 菜单项集合每轮重建(简单正确优先;1Hz 开销可忽略)
        if format!("{peers:?}") != signature {
            signature = format!("{peers:?}");
            if let Some(item) = menu.get(ID_CONNECTED) {
                if let Some(sub) = item.as_submenu() {
                    let _ = sub.remove_at(0);
                    if peers.is_empty() {
                        if let Ok(hint) =
                            MenuItem::with_id(&app, "connected-empty", "(空)", false, None::<&str>)
                        {
                            let _ = sub.append(&hint);
                        }
                    } else {
                        let mut items: Vec<MenuItem<_>> = Vec::new();
                        for (id, name) in &peers {
                            match MenuItem::with_id(
                                &app,
                                format!("{DEVICE_ID_PREFIX}{id}"),
                                name.clone(),
                                true,
                                None::<&str>,
                            ) {
                                Ok(item) => items.push(item),
                                Err(_) => break,
                            }
                        }
                        let refs: Vec<&dyn IsMenuItem<_>> =
                            items.iter().map(|i| i as &dyn IsMenuItem<_>).collect();
                        let _ = sub.append_items(&refs);
                    }
                }
                if let Some(dis) = menu
                    .get(ID_DISCONNECT_ALL)
                    .as_ref()
                    .and_then(|i| i.as_menuitem())
                {
                    let _ = dis.set_enabled(count > 0);
                }
            }
        }

        let now_active = count > 0;
        if now_active != active {
            active = now_active;
            if let Some(icon) = if active { &active_icon } else { &idle_icon } {
                let _ = tray.set_icon(Some(icon.clone()));
            }
        }
        let tooltip = if active {
            let names = peers
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join("、");
            format!("正在被 {names} 控制")
        } else {
            "rd 远程控制待命".to_string()
        };
        let _ = tray.set_tooltip(Some(tooltip));
    }
}
