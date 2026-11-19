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

/// 托盘主循环:Windows 跑托盘消息泵(下方 `imp`);其他平台驻留待命,
/// 行为与 `--no-tray` 一致(保持可 check/可运行)。
#[cfg(windows)]
pub fn run(runtime: &rd_agent::AgentRuntime) -> ! {
    imp::run(runtime)
}

/// 更新完整链（Windows）：托盘菜单与 main.rs 的 e2e 自动触发钩子共用
/// （RD_UPDATE_TEST_AUTO，t11 无托盘验证 spawn 对调重启）。
#[cfg(windows)]
pub(crate) use imp::update_flow;

#[cfg(not(windows))]
pub fn run(runtime: &rd_agent::AgentRuntime) -> ! {
    let _ = runtime;
    tracing::info!("当前平台无托盘,进程驻留待命(Ctrl+C 退出)");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use anyhow::Context as _;
    use rd_transport::Fingerprint;
    use tray_icon::menu::{
        CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
    };
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MessageBoxW, SetTimer, TranslateMessage, IDYES,
        MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MSG,
        WM_TIMER,
    };

    use rd_agent::session::AgentState;

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
    const ID_AUTOSTART: &str = "autostart";
    const ID_OPEN_LOGS: &str = "open-logs";
    const ID_REPORT_CRASH: &str = "report-crash";
    const ID_LICENSE: &str = "license";
    const ID_CHECK_UPDATE: &str = "check-update";
    const ID_QUIT: &str = "quit";
    /// 设备菜单项 id 前缀,完整形式 `{DEVICE_ID_PREFIX}<conn_id>`。
    const DEVICE_ID_PREFIX: &str = "device-";
    /// 信任设备菜单项 id 前缀,完整形式 `{TRUST_PREFIX}<fp_hex>`(点击撤销信任)。
    const TRUST_PREFIX: &str = "trust-";
    /// HKCU Run 自启键(D-013 包1:用户态自启,免 UAC 免服务)。
    const AUTOSTART_RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const AUTOSTART_VALUE: &str = "rd-agent";

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
        autostart_item: CheckMenuItem,
        /// 信任设备子菜单(点击条目撤销信任,D-013 包3/E4)。
        trust: Submenu,
        trust_items: Vec<MenuItem>,
        /// 上次信任列表签名(变化才重建子菜单)。
        trust_signature: String,
        /// 上次设备列表签名(变化才重建子菜单,避免每秒重建)。
        peers_signature: String,
        connected_count: usize,
        /// 当前是否处于"被控制"图标状态。
        active: bool,
        tooltip: String,
    }

    /// 托盘入口:构建图标/菜单/托盘,主线程泵消息直至退出(永不返回)。
    pub(super) fn run(runtime: &rd_agent::AgentRuntime) -> ! {
        // 自动检查更新（改造项 A 收口）：常驻线程静默轮询，未配渠道时零打扰
        spawn_auto_update_check(runtime);
        let state: Arc<AgentState> = Arc::clone(&runtime.state);
        let idle_icon = load_icon(IDLE_ICON_BYTES, "tray.ico");
        let active_icon = load_icon(ACTIVE_ICON_BYTES, "tray-active.ico");

        let code_item = MenuItem::with_id(ID_GENERATE, "生成配对码", true, None);
        let connected = Submenu::with_id(ID_CONNECTED, "已连接:0", false);
        let disconnect_all_item = MenuItem::with_id(ID_DISCONNECT_ALL, "断开所有", false, None);
        let paused_item = CheckMenuItem::with_id(ID_PAUSE, "暂停共享", true, false, None);
        let autostart_item =
            CheckMenuItem::with_id(ID_AUTOSTART, "开机自启", true, autostart_enabled(), None);
        let trust = Submenu::with_id("trust", "信任设备", true);
        let open_logs = MenuItem::with_id(ID_OPEN_LOGS, "打开日志目录", true, None);
        let report_crash_item = MenuItem::with_id(ID_REPORT_CRASH, "上报崩溃…", true, None);
        let license_item = MenuItem::with_id(ID_LICENSE, "授权…", true, None);
        let check_update_item = MenuItem::with_id(ID_CHECK_UPDATE, "检查更新", true, None);
        let quit_item = MenuItem::with_id(ID_QUIT, "退出", true, None);

        let menu = Menu::new();
        let _ = menu.append(&code_item);
        let _ = menu.append(&connected);
        let _ = menu.append(&disconnect_all_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&paused_item);
        let _ = menu.append(&autostart_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&trust);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&check_update_item);
        let _ = menu.append(&open_logs);
        let _ = menu.append(&report_crash_item);
        let _ = menu.append(&license_item);
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
            autostart_item,
            trust,
            trust_items: Vec::new(),
            trust_signature: String::new(),
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
                handle_menu_event(runtime, &mut ui, event.id.0.as_str());
            }
            // 返回值:0 = WM_QUIT,-1 = 错误,>0 = 取到消息
            let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if got.0 <= 0 {
                tracing::info!(code = got.0, "托盘消息循环结束");
                disconnect_all(runtime);
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
    fn handle_menu_event(runtime: &rd_agent::AgentRuntime, ui: &mut TrayUi, id: &str) {
        let state: Arc<AgentState> = Arc::clone(&runtime.state);
        match id {
            ID_GENERATE => {
                let code = {
                    let mut pairing = state.pairing.lock().expect("pairing 锁中毒");
                    pairing.new_code()
                };
                // 配对码只经 MessageBox 展示,不落日志;同时复制到剪贴板
                tracing::info!("托盘生成新配对码(5 分钟内有效)");
                let copied = copy_to_clipboard(&code);
                let code_line = if copied {
                    format!("{code}(已复制到剪贴板)")
                } else {
                    code
                };
                // 首行展示本机可达地址(D-013:码+地址同屏,家人只需念两条信息)
                let addrs: Vec<String> = state
                    .listen_addrs
                    .lock()
                    .expect("listen_addrs 锁中毒")
                    .iter()
                    .filter(|a| !a.ip().is_loopback())
                    .map(|a| a.to_string())
                    .collect();
                let addr_lines = if addrs.is_empty() {
                    "(监听地址未就绪,稍后再试)".to_string()
                } else {
                    addrs.join("\n")
                };
                show_pair_code_dialog(format!(
                    "把下面地址填进控制端「添加设备」:\n{addr_lines}\n\n配对码:{code_line}"
                ));
            }
            ID_AUTOSTART => {
                let enable = ui.autostart_item.is_checked();
                match set_autostart(enable) {
                    Ok(()) => tracing::info!(enable, "开机自启已更新"),
                    Err(err) => {
                        tracing::error!("设置开机自启失败: {err:#}");
                        // 回滚勾选,与注册表实际状态一致
                        ui.autostart_item.set_checked(!enable);
                    }
                }
            }
            ID_DISCONNECT_ALL => disconnect_all(runtime),
            ID_PAUSE => {
                // Windows 下 muda 原生先翻转勾选,再发事件:以勾选态为准回写
                let paused = ui.paused_item.is_checked();
                state.paused.store(paused, Ordering::SeqCst);
                if paused {
                    // 终端一并终止(D-013 包3/E4):活 shell 不能在暂停后存活
                    rd_agent::term_handler::pause_all(&state);
                    broadcast_session_state(&state, SESSION_STATE_PAUSED, "共享已暂停");
                    tracing::info!("共享已暂停(画面/输入/终端全部停止)");
                } else {
                    broadcast_session_state(&state, SESSION_STATE_RESUMED, "");
                    tracing::info!("共享已恢复");
                }
            }
            ID_OPEN_LOGS => open_log_dir(),
            ID_REPORT_CRASH => report_crash(),
            ID_CHECK_UPDATE => check_update(runtime),
            ID_LICENSE => {
                // 授权状态弹窗（T-029）：当前档位 + 升级操作说明（大白话）
                show_pair_code_dialog(license_dialog_text(&state));
            }
            ID_QUIT => {
                tracing::info!("托盘请求退出");
                disconnect_all(runtime);
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
                } else if let Some(fp_hex) = other.strip_prefix(TRUST_PREFIX) {
                    revoke_trust(&state, fp_hex);
                }
            }
        }
        refresh(&state, ui);
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

        // 信任设备子项(D-013 包3):列表变化才重建;点击条目撤销信任
        let trusted: Vec<(String, String)> = {
            let trust = state.trust.lock().expect("trust 锁中毒");
            trust
                .list()
                .into_iter()
                .map(|p| (p.fingerprint_hex, p.name))
                .collect()
        };
        let t_sig: String = trusted
            .iter()
            .map(|(fp, name)| format!("{fp}:{name};"))
            .collect();
        if t_sig != ui.trust_signature {
            ui.trust_signature = t_sig;
            for old in std::mem::take(&mut ui.trust_items) {
                let _ = ui.trust.remove(&old);
            }
            ui.trust_items = trusted
                .iter()
                .map(|(fp, name)| {
                    MenuItem::with_id(
                        format!("{TRUST_PREFIX}{fp}"),
                        format!("{name} (撤销信任)"),
                        true,
                        None,
                    )
                })
                .collect();
            if ui.trust_items.is_empty() {
                let hint = MenuItem::with_id("trust-empty", "(空)", false, None);
                let _ = ui.trust.append(&hint);
                ui.trust_items.push(hint);
            } else {
                let items: Vec<&dyn IsMenuItem> = ui
                    .trust_items
                    .iter()
                    .map(|item| item as &dyn IsMenuItem)
                    .collect();
                let _ = ui.trust.append_items(&items);
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

    /// 弹窗文本的日志安全截断：直通路径把文本写入日志，截断防长文刷屏；
    /// 配对码等不该进日志的内容（生产路径本就不落日志）不因直通泄漏全文。
    #[cfg(debug_assertions)]
    fn truncate_for_log(text: &str) -> String {
        const LIMIT: usize = 160;
        let mut s: String = text.chars().take(LIMIT).collect();
        if text.chars().count() > LIMIT {
            s.push('…');
        }
        s
    }

    /// 弹出配对对话框(独立线程,模态于该线程不阻塞托盘循环)。
    ///
    /// 文案 = 本机可达地址(多条逐行)+ 配对码;指纹不再展示(D-013:MITM
    /// 防护由 SPAKE2 MAC 绑定指纹保证,与是否肉眼比对无关,概念最小化)。
    fn show_pair_code_dialog(text: String) {
        // e2e 直通（仅 debug + 显式环境变量）：不弹窗、文本截断落日志——
        // 无人值守环境弹窗会挂在无人点击的桌面上（t11 更新失败路径的诊断出口）。
        #[cfg(debug_assertions)]
        if std::env::var("RD_UPDATE_TEST_CONFIRM").as_deref() == Ok("1") {
            tracing::info!(
                "[update-e2e] dialog 直通(不弹): {}",
                truncate_for_log(&text)
            );
            return;
        }
        if CODE_DIALOG_OPEN.swap(true, Ordering::SeqCst) {
            return;
        }
        std::thread::spawn(move || {
            let text = HSTRING::from(text);
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

    /// 授权弹窗内容（T-029）：档位、限制余量与升级操作，一次说清。
    fn license_dialog_text(state: &AgentState) -> String {
        let trusted = state.trust.lock().expect("trust 锁中毒").list().len();
        match state.license.as_ref() {
            Some(l) if l.plan == rd_security::Plan::Pro => format!(
                "当前版本:专业版(Pro)\n授权给:{}\n可信任设备数:{}\n\n感谢支持!",
                if l.name.is_empty() { "(未署名)" } else { &l.name },
                l.devices_max
            ),
            _ => format!(
                "当前版本:免费版\n已信任 {trusted}/{} 台设备,同一时间支持 1 台电脑连接。\n\n升级专业版:\n1. 联系购买获得一串「授权码」\n2. 把授权码保存为文件: {}\\license.key\n3. 重启本程序即生效\n\n(免费版永久可用,无时间限制)",
                rd_security::FREE_DEVICE_MAX,
                rd_agent::default_config_dir().display()
            ),
        }
    }

    /// 撤销对某设备的信任(D-013 包3/E4:被控端此前无撤销入口,控制端设备
    /// 丢失后家人无法断绝信任):移除信任条目并断开其在线连接。
    fn revoke_trust(state: &AgentState, fp_hex: &str) {
        let Some(fp) = fp_from_hex(fp_hex) else {
            tracing::warn!(len = fp_hex.len(), "撤销信任:指纹格式非法");
            return;
        };
        let removed = {
            let mut trust = state.trust.lock().expect("trust 锁中毒");
            trust.remove(&fp).is_ok()
        };
        // 在线连接一并断开:撤销必须立即生效,否则对方在旧连接里仍是全权
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

    /// 64 字符 hex → 指纹(托盘菜单 id 携带的完整指纹还原)。
    fn fp_from_hex(hex: &str) -> Option<Fingerprint> {
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Fingerprint(bytes))
    }

    /// HKCU Run 自启当前是否已登记。
    fn autostart_enabled() -> bool {
        winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey(AUTOSTART_RUN_KEY)
            .and_then(|key| key.get_value::<String, _>(AUTOSTART_VALUE))
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// 写/删 HKCU Run 自启项(指向当前 exe;免 UAC、用户登录即拉起)。
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

    /// 自动检查更新节奏：启动后首次延迟（避开启动风暴与新旧进程 15s 交接
    /// 窗口），之后每 24h 一查（对齐控制端 useUpdater 的 24h 节流）。未配
    /// 渠道 = 完全静默，T-039 配上渠道即自动生效。
    const AUTO_UPDATE_FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(5 * 60);
    const AUTO_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

    /// 更新链防重入（手动连点 / 自动撞手动：apply 的 .old 对调不可并发）。
    static UPDATE_BUSY: AtomicBool = AtomicBool::new(false);

    /// 托盘「检查更新」（改造项 A）：独立线程跑完整链（见 `update_flow`）。
    fn check_update(runtime: &rd_agent::AgentRuntime) {
        let rt = runtime.rt.clone();
        let state = std::sync::Arc::clone(&runtime.state);
        std::thread::spawn(move || update_flow(&rt, &state, false));
    }

    /// 自动检查更新（常驻线程）：静默轮询，发现新版本才打扰用户——家人
    /// 不会主动点「检查更新」，自动弹窗是渠道触达目标用户的唯一途径。
    fn spawn_auto_update_check(runtime: &rd_agent::AgentRuntime) {
        let rt = runtime.rt.clone();
        let state = std::sync::Arc::clone(&runtime.state);
        std::thread::spawn(move || {
            std::thread::sleep(AUTO_UPDATE_FIRST_DELAY);
            loop {
                update_flow(&rt, &state, true);
                std::thread::sleep(AUTO_UPDATE_INTERVAL);
            }
        });
    }

    /// 更新完整链：检查（验签 + 版本比较）→ 确认 → 下载拼装对调 → 确认重启
    /// → 断开会话 → spawn 新 exe → exit(0)。手动（silent=false）失败逐项
    /// 弹窗反馈；自动（silent=true）失败一律静默（日志可查），仅发现新版本
    /// 与「文件已替换」两处必须弹——前者是自动检查的存在意义，后者动了
    /// 用户磁盘文件必须告知。MessageBox 模态自身线程，托盘消息循环不受阻；
    /// 下载期间远程会话由 tokio runtime 线程照常服务。
    ///
    /// 更新完整链：main.rs 的 e2e 自动触发钩子（RD_UPDATE_TEST_AUTO，t11）
    /// 与托盘菜单共用本函数（经模块顶层 re-export 为 tray::update_flow；
    /// imp 模块私有，此可见性不会外泄出 crate）。
    pub(crate) fn update_flow(rt: &tokio::runtime::Handle, state: &Arc<AgentState>, silent: bool) {
        if UPDATE_BUSY.swap(true, Ordering::SeqCst) {
            return; // 已有更新链在跑（手动连点 / 自动撞手动）
        }
        update_flow_inner(rt, state, silent);
        // exit(0)（重启成功路径）不返回到这里；其余路径统一归位
        UPDATE_BUSY.store(false, Ordering::SeqCst);
    }

    fn update_flow_inner(rt: &tokio::runtime::Handle, state: &Arc<AgentState>, silent: bool) {
        let settings = rd_agent::updater::UpdateSettings::from_env();
        if !settings.enabled() {
            if !silent {
                show_pair_code_dialog(
                    "更新功能未启用（未配置更新渠道 RD_UPDATE_ENDPOINT）。".to_string(),
                );
            }
            return;
        }
        let http = match reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(600))
            .build()
        {
            Ok(c) => c,
            Err(err) => {
                if silent {
                    tracing::warn!("自动检查更新：HTTP 客户端初始化失败: {err}");
                } else {
                    show_pair_code_dialog(format!("HTTP 客户端初始化失败: {err}"));
                }
                return;
            }
        };

        // 1) 检查（验签 + 防回滚版本比较）
        let current = env!("CARGO_PKG_VERSION");
        let manifest = match rt.block_on(rd_agent::updater::check(&http, &settings, current)) {
            Ok(Some(m)) => m,
            Ok(None) => {
                if silent {
                    tracing::info!("自动检查更新：已是最新版本");
                } else {
                    show_pair_code_dialog("已是最新版本。".to_string());
                }
                return;
            }
            Err(err) => {
                if silent {
                    tracing::warn!("自动检查更新失败（下个周期重试）: {err:#}");
                } else {
                    show_pair_code_dialog(format!("检查更新失败:\n{err:#}"));
                }
                return;
            }
        };

        // 2) 用户确认（更新会短暂断开会话）——手动/自动都弹
        if !confirm_box(&format!(
            "发现新版本 {}（当前 {}），现在更新吗？\n\n更新会下载新版本并替换文件，\
             完成后自动重启（会话会短暂断开）。",
            manifest.version, current
        )) {
            tracing::info!(
                version = %manifest.version,
                "用户暂不更新（本周期不再打扰，下个周期再查）"
            );
            return;
        }

        // 3) 下载 → 拼装 → 对调
        let plan = match rt.block_on(rd_agent::updater::apply(&http, &settings, &manifest)) {
            Ok(p) => p,
            Err(err) => {
                if silent {
                    tracing::warn!("自动更新失败（已放弃替换）: {err:#}");
                } else {
                    show_pair_code_dialog(format!("更新失败（已放弃替换）:\n{err:#}"));
                }
                return;
            }
        };

        // 4) 重启确认 + 执行：断会话 → spawn 新 exe → exit(0)
        // （文件已替换必须问——动了用户磁盘必须告知；「否」= 下次启动生效）
        if !confirm_box(&format!(
            "文件已替换完成，现在重启进入 {} 吗？（点「否」则下次启动生效）",
            manifest.version
        )) {
            tracing::info!(version = %manifest.version, "用户选择稍后重启，下次启动生效");
            return;
        }
        state.disconnect_all_peers(rt, "升级重启");
        if let Err(err) = rd_agent::updater::restart_now(&plan) {
            show_pair_code_dialog(format!(
                "新版启动失败（文件已替换，手动重启 rd-agent 即可用新版本）:\n{err:#}"
            ));
            return;
        }
        std::process::exit(0);
    }

    /// 是/否确认框。
    fn confirm_box(text: &str) -> bool {
        // e2e 直通（仅 debug + 显式环境变量；照 main.rs RD_TEST_CRASH 先例）：
        // 自动确认「是」，文本截断落日志供 t11 断言更新链路径。release 无此分支。
        #[cfg(debug_assertions)]
        if std::env::var("RD_UPDATE_TEST_CONFIRM").as_deref() == Ok("1") {
            tracing::info!(
                "[update-e2e] confirm_box 直通(=是): {}",
                truncate_for_log(text)
            );
            return true;
        }
        let text = HSTRING::from(text);
        let title = HSTRING::from("rd 远程控制 · 检查更新");
        unsafe {
            MessageBoxW(
                None,
                &text,
                &title,
                MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
            ) == IDYES
        }
    }

    /// 崩溃上报（T-040 / R2 修复 C-1）：预览本地 dump（元数据）→ 用户确认
    /// → 系统浏览器打开预填 Issue。dump 目录与 main.rs panic hook 写入处同源
    /// （`default_config_dir()/logs/crash`；`--config-dir` 覆盖场景与
    /// 「打开日志目录」同款不可见限制）。
    ///
    /// C-1：URL 含字面 `&`，`cmd /C start` 会把它当命令分隔符拆断、且中文
    /// urlencode 后易超 cmd 8191 字符上限——改用 `explorer <url>`（不经
    /// shell 解析，交默认浏览器）；body 上限同步收紧（M-4：预览=全量上报）。
    fn report_crash() {
        let dir = rd_agent::default_config_dir().join("logs").join("crash");
        let dumps = rd_crash::list_dumps(&dir);
        if dumps.is_empty() {
            show_pair_code_dialog("没有崩溃记录，无需上报。".to_string());
            return;
        }
        let url = rd_crash::issue_new_url(&dumps, 1500);
        let mut preview = format!(
            "共 {} 条崩溃记录（只含元数据，无内存内容）：\n\n",
            dumps.len()
        );
        // M-4：预览列出全部将上报的条目（条数=URL 实际携带数，所见即所发）
        for d in &dumps {
            preview.push_str(&format!(
                "· {} v{} @ {} | {}\n  {} ({})\n",
                d.app, d.version, d.time_unix, d.thread, d.message, d.location
            ));
        }
        preview.push_str("\n点「是」在浏览器打开上报页面（内容可先检查再提交）；点「否」取消。");
        std::thread::spawn(move || {
            let text = HSTRING::from(preview);
            let title = HSTRING::from("rd 远程控制 · 崩溃上报");
            let confirmed = unsafe {
                MessageBoxW(
                    None,
                    &text,
                    &title,
                    MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
                ) == IDYES
            };
            if confirmed {
                // explorer 不经 shell 解析 URL（无 & 拆断/长度上限问题）
                match std::process::Command::new("explorer").arg(&url).spawn() {
                    Ok(_) => tracing::info!("已打开崩溃上报页面"),
                    Err(err) => tracing::warn!("打开崩溃上报页面失败: {err}"),
                }
            } else {
                tracing::info!("用户取消崩溃上报（dump 保留本地）");
            }
        });
    }

    /// SessionState.Kind 原始值(v0.1,proto SessionState::Kind;AGENT_CLOSING=1
    /// 由 AgentRuntime::disconnect_all 内部发送)。
    const SESSION_STATE_PAUSED: i32 = 2;
    const SESSION_STATE_RESUMED: i32 = 3;

    /// 对全部在线控制端推会话状态(托盘线程非 runtime 线程,经 PeerInfo.rt
    /// block_on;发送失败静默——对端多半已断)。
    fn broadcast_session_state(state: &AgentState, kind: i32, reason: &str) {
        let peers: Vec<_> = state
            .peers
            .lock()
            .expect("peers 锁中毒")
            .values()
            .cloned()
            .collect();
        for peer in peers {
            let reason = reason.to_string();
            let ctl = Arc::clone(&peer.ctl);
            peer.rt.block_on(async move {
                let mut ctl = ctl.lock().await;
                let _ = ctl
                    .send(&rd_proto::v0::Envelope {
                        req_id: 0,
                        msg: Some(rd_proto::v0::envelope::Msg::SessionState(
                            rd_proto::v0::SessionState { kind, reason },
                        )),
                    })
                    .await;
            });
        }
    }

    /// 取消并清空所有连接(托盘"断开所有"/退出):先推 AGENT_CLOSING 让控制端
    /// 区分「Agent 退出」与「网络断」(v0.1/D-013 包4),再取消令牌。
    fn disconnect_all(runtime: &rd_agent::AgentRuntime) {
        let count = runtime.state.peers.lock().expect("peers 锁中毒").len();
        runtime.disconnect_all("被控端已退出 rd-agent");
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
