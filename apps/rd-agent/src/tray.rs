//! 托盘:图标 + 菜单(生成配对码/在线设备/断开/暂停共享/日志目录/退出)——T-018。
//!
//! 主线程在此驻留并泵 Win32 消息(`GetMessageW`):tray-icon 要求创建托盘的
//! 线程持续分发消息;菜单点击经 muda 内部窗口回调进入其全局事件通道,由本
//! 循环非阻塞取出处理。启动一个无窗口定时器(`SetTimer(NULL, …)`,每秒),
//! 系统向队列投递 `WM_TIMER`(hwnd=NULL),循环里就地刷新图标/tooltip/菜单。
//!
//! 约定(11_security 与任务卡):
//! - 配对码只在 MessageBox 展示,不落日志;弹窗跑在独立线程(模态于该线程,
//!   不阻塞托盘消息循环),同一时刻至多一个弹窗;
//! - 断开 = `PeerInfo.cancel` 取消令牌(T-016/017 的流任务 `select` 它,
//!   连接退出清理见 session.rs);
//! - 暂停共享 = `state.paused` 原子开关(画面/输入 handler 消费);
//! - 退出 = 取消所有连接后 `std::process::exit(0)`。

use std::sync::Arc;

use crate::session::AgentState;

/// 托盘主循环:Windows 跑托盘消息泵(下方 `imp`);其他平台驻留待命,
/// 行为与 `--no-tray` 一致(保持可 check/可运行)。
#[cfg(windows)]
pub fn run(state: Arc<AgentState>) -> ! {
    imp::run(state)
}

#[cfg(not(windows))]
pub fn run(state: Arc<AgentState>) -> ! {
    let _ = state;
    tracing::info!("当前平台无托盘,进程驻留待命(Ctrl+C 退出)");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tray_icon::menu::{
        CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
    };
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MessageBoxW, SetTimer, TranslateMessage, MB_ICONINFORMATION,
        MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MSG, WM_TIMER,
    };

    use crate::session::AgentState;

    /// 内嵌托盘图标资产:灰 = 空闲,绿 = 正在被控制(16/32/48 px,32bpp)。
    const IDLE_ICON_BYTES: &[u8] = include_bytes!("../assets/tray.ico");
    const ACTIVE_ICON_BYTES: &[u8] = include_bytes!("../assets/tray-active.ico");
    /// 解码托盘图标时优先选取的尺寸(px)。
    const PREFERRED_ICON_SIZE: u32 = 32;
    /// 菜单/图标状态刷新周期(任务卡:每秒)。
    const REFRESH_MS: u32 = 1000;

    // 菜单项稳定 id(muda MenuId 任意字符串)
    const ID_GENERATE: &str = "generate-code";
    const ID_CONNECTED: &str = "connected";
    const ID_DISCONNECT_ALL: &str = "disconnect-all";
    const ID_PAUSE: &str = "pause";
    const ID_OPEN_LOGS: &str = "open-logs";
    const ID_QUIT: &str = "quit";
    /// 设备菜单项 id 前缀,完整形式 `{DEVICE_ID_PREFIX}<conn_id>`。
    const DEVICE_ID_PREFIX: &str = "device-";

    /// 配对码弹窗互斥:同一时刻至多一个(防连点堆叠)。
    static CODE_DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

    /// 托盘 UI 句柄集合;`refresh` 依据 AgentState 增量更新。
    struct TrayUi {
        tray: TrayIcon,
        idle_icon: Option<Icon>,
        active_icon: Option<Icon>,
        connected: Submenu,
        device_items: Vec<MenuItem>,
        disconnect_all_item: MenuItem,
        paused_item: CheckMenuItem,
        /// 上次设备列表签名(变化才重建子菜单,避免每秒重建)。
        peers_signature: String,
        connected_count: usize,
        /// 当前是否处于"被控制"图标状态。
        active: bool,
        tooltip: String,
    }

    /// 托盘入口:构建图标/菜单/托盘,主线程泵消息直至退出(永不返回)。
    pub(super) fn run(state: Arc<AgentState>) -> ! {
        let idle_icon = load_icon(IDLE_ICON_BYTES, "tray.ico");
        let active_icon = load_icon(ACTIVE_ICON_BYTES, "tray-active.ico");

        let code_item = MenuItem::with_id(ID_GENERATE, "生成配对码", true, None);
        let connected = Submenu::with_id(ID_CONNECTED, "已连接:0", false);
        let disconnect_all_item = MenuItem::with_id(ID_DISCONNECT_ALL, "断开所有", false, None);
        let paused_item = CheckMenuItem::with_id(ID_PAUSE, "暂停共享", true, false, None);
        let open_logs = MenuItem::with_id(ID_OPEN_LOGS, "打开日志目录", true, None);
        let quit_item = MenuItem::with_id(ID_QUIT, "退出", true, None);

        let menu = Menu::new();
        let _ = menu.append(&code_item);
        let _ = menu.append(&connected);
        let _ = menu.append(&disconnect_all_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&paused_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&open_logs);
        let _ = menu.append(&quit_item);

        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .with_tooltip("rd 远程控制待命");
        if let Some(icon) = &idle_icon {
            builder = builder.with_icon(icon.clone());
        }
        let tray = match builder.build() {
            Ok(tray) => tray,
            Err(err) => {
                tracing::error!("创建托盘失败,降级为后台驻留: {err:#}");
                park_forever();
            }
        };

        let mut ui = TrayUi {
            tray,
            idle_icon,
            active_icon,
            connected,
            device_items: Vec::new(),
            disconnect_all_item,
            paused_item,
            peers_signature: String::new(),
            connected_count: 0,
            active: false,
            tooltip: String::new(),
        };
        refresh(&state, &mut ui);

        // 无窗口定时器:hwnd=NULL + 无 TimerProc → WM_TIMER(hwnd=NULL) 进线程
        // 队列,在消息循环里就地处理(无需跨线程静态状态)。
        if unsafe { SetTimer(None, 1, REFRESH_MS, None) } == 0 {
            tracing::warn!("SetTimer 失败,菜单状态将仅在交互时刷新");
        }
        tracing::info!("托盘就绪");

        let mut msg = MSG::default();
        loop {
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                handle_menu_event(&state, &mut ui, event.id.0.as_str());
            }
            // 返回值:0 = WM_QUIT,-1 = 错误,>0 = 取到消息
            let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if got.0 <= 0 {
                tracing::info!(code = got.0, "托盘消息循环结束");
                disconnect_all(&state);
                std::process::exit(0);
            }
            if msg.message == WM_TIMER && msg.hwnd.0.is_null() {
                refresh(&state, &mut ui);
                continue;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// 兜底驻留:托盘创建失败时保持服务进程存活(与旧桩行为一致)。
    fn park_forever() -> ! {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    /// 菜单点击分发;除退出外的分支最后统一 `refresh` 即时反映状态。
    fn handle_menu_event(state: &AgentState, ui: &mut TrayUi, id: &str) {
        match id {
            ID_GENERATE => {
                let code = {
                    let mut pairing = state.pairing.lock().expect("pairing 锁中毒");
                    pairing.new_code()
                };
                // 配对码只经 MessageBox 展示,不落日志;同时复制到剪贴板
                tracing::info!("托盘生成新配对码(5 分钟内有效)");
                let copied = copy_to_clipboard(&code);
                show_pair_code_dialog(
                    if copied {
                        format!(
                            "{code}
(已复制到剪贴板)"
                        )
                    } else {
                        code
                    },
                    state.local_fp.short_hex(),
                );
            }
            ID_DISCONNECT_ALL => disconnect_all(state),
            ID_PAUSE => {
                // Windows 下 muda 原生先翻转勾选,再发事件:以勾选态为准回写
                let paused = ui.paused_item.is_checked();
                state.paused.store(paused, Ordering::SeqCst);
                if paused {
                    tracing::info!("共享已暂停(画面/输入停止)");
                } else {
                    tracing::info!("共享已恢复");
                }
            }
            ID_OPEN_LOGS => open_log_dir(),
            ID_QUIT => {
                tracing::info!("托盘请求退出");
                disconnect_all(state);
                std::process::exit(0);
            }
            other => {
                if let Some(conn_id) = other
                    .strip_prefix(DEVICE_ID_PREFIX)
                    .and_then(|rest| rest.parse::<u64>().ok())
                {
                    if let Some(peer) = state.peers.lock().expect("peers 锁中毒").remove(&conn_id)
                    {
                        tracing::info!(conn = conn_id, name = %peer.name, "托盘断开该设备");
                        peer.cancel.cancel();
                    }
                }
            }
        }
        refresh(state, ui);
    }

    /// 每秒刷新:已连接数/设备子菜单、图标与 tooltip、暂停勾选同步。
    fn refresh(state: &AgentState, ui: &mut TrayUi) {
        let mut peers: Vec<(u64, String)> = state
            .peers
            .lock()
            .expect("peers 锁中毒")
            .iter()
            .map(|(conn_id, peer)| (*conn_id, peer.name.clone()))
            .collect();
        peers.sort_unstable_by_key(|(conn_id, _)| *conn_id);
        let count = peers.len();

        if count != ui.connected_count {
            ui.connected_count = count;
            ui.connected.set_text(format!("已连接:{count}"));
            ui.connected.set_enabled(count > 0);
            ui.disconnect_all_item.set_enabled(count > 0);
        }

        // 设备子项:签名变化才重建(移除旧项 → 追加新项;点击即断开)
        let signature: String = peers
            .iter()
            .map(|(conn_id, name)| format!("{conn_id}:{name};"))
            .collect();
        if signature != ui.peers_signature {
            ui.peers_signature = signature;
            for old in std::mem::take(&mut ui.device_items) {
                let _ = ui.connected.remove(&old);
            }
            ui.device_items = peers
                .iter()
                .map(|(conn_id, name)| {
                    MenuItem::with_id(
                        format!("{DEVICE_ID_PREFIX}{conn_id}"),
                        name.clone(),
                        true,
                        None,
                    )
                })
                .collect();
            if !ui.device_items.is_empty() {
                let items: Vec<&dyn IsMenuItem> = ui
                    .device_items
                    .iter()
                    .map(|item| item as &dyn IsMenuItem)
                    .collect();
                let _ = ui.connected.append_items(&items);
            }
        }

        // 图标:有连接 = active(绿),否则空闲(灰)
        let active = count > 0;
        if active != ui.active {
            ui.active = active;
            let icon = if active {
                ui.active_icon.clone()
            } else {
                ui.idle_icon.clone()
            };
            if let Some(icon) = icon {
                if let Err(err) = ui.tray.set_icon(Some(icon)) {
                    tracing::warn!("切换托盘图标失败: {err}");
                }
            }
        }

        // tooltip:被控制时点名控制端
        let tooltip = if active {
            let names = peers
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join("、");
            format!("正在被 {names} 控制")
        } else {
            "rd 远程控制待命".to_string()
        };
        if tooltip != ui.tooltip {
            ui.tooltip = tooltip.clone();
            if let Err(err) = ui.tray.set_tooltip(Some(tooltip)) {
                tracing::warn!("更新托盘 tooltip 失败: {err}");
            }
        }

        // 勾选态与共享开关保持一致(菜单点击已回写;此处兜底同步)
        let paused = state.paused.load(Ordering::SeqCst);
        if paused != ui.paused_item.is_checked() {
            ui.paused_item.set_checked(paused);
        }
    }

    /// 弹出配对码对话框(独立线程,模态于该线程不阻塞托盘循环)。
    fn show_pair_code_dialog(code: String, local_fp_hex: String) {
        if CODE_DIALOG_OPEN.swap(true, Ordering::SeqCst) {
            return;
        }
        std::thread::spawn(move || {
            let text = HSTRING::from(format!(
                "配对码 {code},5 分钟内有效\n\n本机指纹(前 8 字节):{local_fp_hex}"
            ));
            let title = HSTRING::from("rd 远程控制 · 配对");
            unsafe {
                MessageBoxW(
                    None,
                    &text,
                    &title,
                    MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
                );
            }
            CODE_DIALOG_OPEN.store(false, Ordering::SeqCst);
        });
    }

    /// 用资源管理器打开日志目录。
    ///
    /// 与 main.rs 的 `default_config_dir()` 同源:`directories::ProjectDirs`
    /// 的数据目录 + `logs/`(Windows 实际为 `%APPDATA%\rd\data\logs`;init_logging
    /// 启动时已建)。ProjectDirs 不可用时退回 `%LOCALAPPDATA%\rd\logs`。
    /// 注意:`--config-dir` 覆盖的目录托盘不可见(AgentState 不携带该路径)。
    fn open_log_dir() {
        let dir = match directories::ProjectDirs::from("", "", "rd") {
            Some(dirs) => dirs.data_dir().join("logs"),
            None => {
                let fallback = std::env::var_os("LOCALAPPDATA")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
                    .join("rd")
                    .join("logs");
                tracing::warn!(dir = %fallback.display(), "ProjectDirs 不可用,回退默认日志目录");
                fallback
            }
        };
        match std::process::Command::new("explorer").arg(&dir).spawn() {
            Ok(_) => tracing::info!(dir = %dir.display(), "打开日志目录"),
            Err(err) => tracing::warn!(dir = %dir.display(), "打开日志目录失败: {err}"),
        }
    }

    /// 取消并清空所有连接(托盘"断开所有"/退出)。
    fn disconnect_all(state: &AgentState) {
        let peers = {
            let mut peers = state.peers.lock().expect("peers 锁中毒");
            std::mem::take(&mut *peers)
        };
        let count = peers.len();
        for (_conn_id, peer) in peers {
            peer.cancel.cancel();
        }
        if count > 0 {
            tracing::info!(count, "已断开全部连接");
        }
    }

    /// 从内嵌 .ico 解码最接近 `prefer` 尺寸的 32bpp 帧 → RGBA。
    ///
    /// 资产由仓库内脚本生成,固定为 BITMAPINFOHEADER + 自底向上 BGRA + AND
    /// 掩码;其他条目类型(如 PNG)一律跳过。解码失败返回 `None` 并由调用方
    /// 记录(托盘可降级为无图标)。
    fn decode_ico_rgba(bytes: &[u8], prefer: u32) -> Option<(Vec<u8>, u32, u32)> {
        if bytes.len() < 6 {
            return None;
        }
        let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        // 候选:(与 prefer 的尺寸差, 宽, 高, 图像数据偏移)
        let mut best: Option<(u32, u32, u32, usize)> = None;
        for index in 0..count {
            let entry = 6 + index * 16;
            let dir = bytes.get(entry..entry + 16)?;
            let width = u32::from(dir[0]); // 0 表示 256,自绘资产不含
            let height = u32::from(dir[1]);
            let bit_count = u16::from_le_bytes([dir[6], dir[7]]);
            let offset = u32::from_le_bytes(dir[12..16].try_into().ok()?) as usize;
            if bit_count != 32 || width == 0 || height == 0 {
                continue;
            }
            let diff = width.abs_diff(prefer);
            if best
                .as_ref()
                .is_none_or(|(best_diff, _, _, _)| diff < *best_diff)
            {
                best = Some((diff, width, height, offset));
            }
        }
        let (_diff, width, height, offset) = best?;

        // BITMAPINFOHEADER:biSize 在前 4 字节,像素紧跟头后
        let header_size =
            u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize;
        if header_size < 40 {
            return None;
        }
        let len = width as usize * height as usize * 4;
        let pixels = bytes.get(offset + header_size..offset + header_size + len)?;
        let stride = width as usize * 4;
        let rows = height as usize;
        let mut rgba = vec![0u8; len];
        for row in 0..rows {
            // 自底向上存储:取对称行
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

    /// 内嵌资产 → tray-icon 图标;失败记录错误并返回 `None`。
    fn load_icon(bytes: &'static [u8], asset: &str) -> Option<Icon> {
        let Some((rgba, width, height)) = decode_ico_rgba(bytes, PREFERRED_ICON_SIZE) else {
            tracing::error!(asset, "托盘图标资产解码失败");
            return None;
        };
        match Icon::from_rgba(rgba, width, height) {
            Ok(icon) => Some(icon),
            Err(err) => {
                tracing::error!(asset, "托盘图标转换失败: {err}");
                None
            }
        }
    }
    /// 把 UTF-8 文本以 `CF_UNICODETEXT` 写入剪贴板(打开失败/占用时返回 false)。
    /// 配对码场景失败无害——弹窗仍展示明文。
    fn copy_to_clipboard(text: &str) -> bool {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };
        use windows::Win32::System::Ole::CF_UNICODETEXT;

        unsafe {
            if OpenClipboard(None).is_err() {
                return false;
            }
            let ok = (|| {
                let _ = EmptyClipboard();
                let mut utf16: Vec<u16> = text.encode_utf16().collect();
                utf16.push(0); // NUL 结尾
                let bytes = utf16.len() * 2;
                let h = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
                let p = GlobalLock(h) as *mut u16;
                if p.is_null() {
                    return None;
                }
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), p, utf16.len());
                let _ = GlobalUnlock(h);
                if SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(h.0))).is_err() {
                    return None;
                }
                Some(())
            })()
            .is_some();
            let _ = CloseClipboard();
            ok
        }
    }
}
