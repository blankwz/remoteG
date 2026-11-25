//! 输入处理:协议输入消息 / datagram → rd-input-win 注入(T-017)。
//!
//! 注入门(任务卡步骤 1 + DoD「未打开的窗口不接受鼠标注入」):
//! - `state.paused`(托盘「暂停远程输入」,T-018 消费)置位时全部丢弃;
//! - 鼠标点击/滚轮/移动只作用于本会话**已打开**的窗口
//!   (`ctx.windows.is_open`,`window_id` 即目标 hwnd);未打开一律丢弃;
//! - 鼠标**按下**前先 `ensure_foreground` 置前;置前失败不注入 —— 否则点击
//!   会落在遮挡在前的其他程序上;
//! - 键盘/文本协议上没有 `window_id`:以 `GetForegroundWindow()` 返回的
//!   hwnd 为目标窗口,仅当前台属于本会话已打开窗口(`is_open(前台 hwnd)`)
//!   才注入,防止向无关程序打字。`window_id` 与 hwnd 同源,该判定与
//!   「`GetForegroundWindow() as u64 == window_id` 且 `is_open(window_id)`」
//!   等价,只是方向相反(由前台 hwnd 反查窗口集合);
//! - 断开时 `release_all` 抬起全部按下状态,防目标窗口卡键。
//!
//! 日志只记录事件类别与窗口 id,**绝不记录按键与文本内容**(DoD)。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

use rd_proto::v0::{ContinuousScroll, KeyEvent, MouseButton, MouseWheel, TextInput};

use crate::session::SessionCtx;

/// 键鼠注入器:Windows 下为 rd-input-win 实现；macOS 下为 rd-mac CoreGraphics CGEvent 实现。
#[cfg(windows)]
pub use rd_input_win::Injector;
#[cfg(not(windows))]
pub use rd_mac::input::Injector;

/// 控制流承载的输入消息(点击/按键需可靠送达,05_api 摘要)。
#[derive(Debug)]
pub enum InputMsg {
    Key(KeyEvent),
    MouseButton(MouseButton),
    MouseWheel(MouseWheel),
    ContinuousScroll(ContinuousScroll),
    TextInput(TextInput),
}

/// 处理一条控制流输入消息(同步调用,内部为阻塞 Win32 注入;置前失败重试
/// 最长数百毫秒,点击/按键为人手频率,不构成调度问题)。
pub fn handle(ctx: &SessionCtx, msg: InputMsg) -> anyhow::Result<()> {
    // 暂停远程输入:全部丢弃
    if ctx.state.paused.load(Ordering::SeqCst) {
        return Ok(());
    }
    // 自控自(loopback)「穿透模式」（2026-10-28 定稿）：点击/滚轮/键盘/文字
    // 放行——agent 端 ensure_fg 把远端窗口拉到前台后定点注入（用户直接看到
    // 真实窗口被操作，点控制端画面即可切回）。全部停用会退化成"只能看"
    // （用户反馈）；高频鼠标移动的拉扯问题见 on_datagram（仍停用）。
    // busy 豁免：用户点工具栏的物理输入就是"本机活动"，不豁免则常用语/
    // 虚拟按键 2s 窗口期内全被吞（实测）。debug 构建再豁免 busy：e2e 与
    // 开发者共用一台机器，人在用电脑时物理输入会让注入用例随机挂掉
    // （实测 t05 套件内间歇失败）——无头 e2e 不代表双机语义，release 不受影响。
    if crate::host_activity::busy() && !ctx.loopback && !cfg!(debug_assertions) {
        return Ok(());
    }
    match msg {
        InputMsg::MouseButton(m) => mouse_button(ctx, &m),
        InputMsg::MouseWheel(m) => mouse_wheel(ctx, &m),
        InputMsg::ContinuousScroll(m) => continuous_scroll(ctx, &m),
        InputMsg::Key(m) => key_event(ctx, &m),
        InputMsg::TextInput(m) => text_input(ctx, &m),
    }
}

/// 协议错误码 40:UIPI 拦截(05_api §4 追加;管理员窗口静默丢注入的可感知化)。
#[cfg(windows)]
const ERR_UIPI_BLOCKED: u32 = 40;

/// UIPI 反馈去抖:连打/连点场景 30 s 只提示一次。
#[cfg(windows)]
static LAST_UIPI_NOTIFY: LazyLock<Mutex<Option<std::time::Instant>>> =
    LazyLock::new(|| Mutex::new(None));

/// [`handle`] + UIPI 可感知化(D-013 包2):SendInput 被 UIPI 拦截时返回值
/// 多半仍"成功"(MSDN 明示不反映在返回值),控制端只会看到键鼠无声无效——
/// 前台窗口完整性高于自身时按 30 s 去抖回一次性 `Error{40}`,由调用方发给
/// 控制端。内部错误仍按 `handle_soft` 语义只记日志,不打断会话。
pub fn handle_with_notify(ctx: &SessionCtx, msg: InputMsg) -> Option<(u32, String)> {
    match handle(ctx, msg) {
        Ok(()) => uipi_check_notify(ctx),
        Err(err) => {
            tracing::warn!(name = %ctx.peer_name, "消息处理失败: {err:#}");
            None
        }
    }
}

/// 前台窗口完整性级别高于自身进程 → 本次注入(以及此前的)已被系统丢弃。
#[cfg(windows)]
fn uipi_check_notify(ctx: &SessionCtx) -> Option<(u32, String)> {
    if !uipi_blocked_now() {
        return None;
    }
    let due = match LAST_UIPI_NOTIFY.lock() {
        Ok(mut last) => {
            let now = std::time::Instant::now();
            let due = last.is_none_or(|t| now.duration_since(t).as_secs() >= 30);
            if due {
                *last = Some(now);
            }
            due
        }
        Err(_) => false,
    };
    if !due {
        return None;
    }
    tracing::warn!(
        name = %ctx.peer_name,
        "前台窗口受 UIPI 保护(管理员权限),注入被系统丢弃"
    );
    Some((
        ERR_UIPI_BLOCKED,
        "对方窗口以管理员权限运行,系统禁止注入。请对方右键「以管理员身份运行」rd-agent 后重试,或在该窗口手动操作".to_string(),
    ))
}

#[cfg(not(windows))]
fn uipi_check_notify(_ctx: &SessionCtx) -> Option<(u32, String)> {
    None
}

/// 前台窗口进程完整性 > 本进程 → UIPI 拦截成立。
#[cfg(windows)]
fn uipi_blocked_now() -> bool {
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::Security::{TOKEN_ACCESS_MASK, TOKEN_QUERY};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    /// 本进程完整性 RID(进程生命周期内不变,一次缓存)。
    static OWN_RID: LazyLock<Option<u32>> =
        LazyLock::new(|| unsafe { integrity_rid(GetCurrentProcess()) });

    let Some(own) = *OWN_RID else {
        return false; // 自身完整性拿不到:宁可放行也不误报
    };
    let Some(fg) = foreground_hwnd() else {
        return false;
    };
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(fg as isize as *mut core::ffi::c_void), Some(&mut pid));
    }
    if pid == 0 {
        return false;
    }
    unsafe {
        let Ok(proc_h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false; // 打不开(权限/退出):无法判定,不拦
        };
        let mut token = HANDLE::default();
        if OpenProcessToken(proc_h, TOKEN_ACCESS_MASK(TOKEN_QUERY.0), &mut token).is_err() {
            return false;
        }
        let fg_rid = integrity_rid(token);
        let _ = windows::Win32::Foundation::CloseHandle(token);
        matches!(fg_rid, Some(rid) if rid > own)
    }
}

/// 读进程 token 的完整性级别 RID(MandatoryLabel SID 最后一个 SubAuthority)。
#[cfg(windows)]
unsafe fn integrity_rid(token: windows::Win32::Foundation::HANDLE) -> Option<u32> {
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL,
    };
    unsafe {
        let mut ret_len = 0u32;
        // 第一次调用探测所需缓冲区长度(预期失败,仅取 ret_len)
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut ret_len);
        if ret_len == 0 {
            return None;
        }
        let mut buf = vec![0u8; ret_len as usize];
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            ret_len,
            &mut ret_len,
        )
        .ok()?;
        let label = &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let count = GetSidSubAuthorityCount(sid);
        if count.is_null() || *count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, u32::from(*count) - 1))
    }
}

/// 鼠标点击:仅限已打开窗口;按下前置前(若系统前台锁拒绝仍直接注入点击,
/// 由 Windows 原生点击激活目标窗口,避免静默吞点击导致彻底失控);只看不控窗口丢弃。
fn mouse_button(ctx: &SessionCtx, m: &MouseButton) -> anyhow::Result<()> {
    if !ctx.windows.is_open(m.window_id) {
        tracing::warn!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃鼠标点击:窗口未打开"
        );
        return Ok(());
    }
    if ctx.windows.is_view_only(m.window_id) {
        tracing::debug!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃鼠标点击:窗口为只看不控"
        );
        return Ok(());
    }
    if let Some(origin) = ctx.windows.desktop_origin(m.window_id) {
        return ctx
            .state
            .injector
            .lock()
            .expect("injector 锁中毒")
            .mouse_button_screen(origin, m.x, m.y, m.button, m.down)
            .map_err(|err| anyhow::anyhow!("完整桌面注入鼠标按键失败: {err:#}"));
    }
    if m.down {
        record_active_window(ctx, m.window_id);
        if !ensure_fg(m.window_id) {
            tracing::warn!(
                name = %ctx.peer_name,
                window_id = m.window_id,
                "置前未确认成功，仍直接注入鼠标按下以激活目标窗口"
            );
        }
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .mouse_button(m.window_id as isize, m.x, m.y, m.button, m.down)
        .map_err(|err| anyhow::anyhow!("注入鼠标按键失败: {err:#}"))
}

/// 滚轮:仅限已打开窗口;无按下语义,不置前(滚动落在光标下的目标窗口);
/// 只看不控窗口丢弃(严格只读)。
fn mouse_wheel(ctx: &SessionCtx, m: &MouseWheel) -> anyhow::Result<()> {
    if ctx.windows.is_view_only(m.window_id) {
        tracing::debug!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃滚轮:窗口为只看不控"
        );
        return Ok(());
    }
    if !ctx.windows.is_open(m.window_id) {
        tracing::warn!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃滚轮:窗口未打开"
        );
        return Ok(());
    }
    if let Some(origin) = ctx.windows.desktop_origin(m.window_id) {
        return ctx
            .state
            .injector
            .lock()
            .expect("injector 锁中毒")
            .wheel_screen(origin, m.x, m.y, m.dx, m.dy)
            .map_err(|err| anyhow::anyhow!("完整桌面注入滚轮失败: {err:#}"));
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .wheel(m.window_id as isize, m.x, m.y, m.dx, m.dy)
        .map_err(|err| anyhow::anyhow!("注入滚轮失败: {err:#}"))
}

/// 连续滚动（手势翻译产出）：与滚轮同守卫（只看不控/未打开丢弃），注入
/// 像素级滚动。高频小步（60Hz 级），单帧失败软处理不打断会话。
fn continuous_scroll(ctx: &SessionCtx, m: &ContinuousScroll) -> anyhow::Result<()> {
    if ctx.windows.is_view_only(m.window_id) {
        tracing::debug!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃连续滚动:窗口为只看不控"
        );
        return Ok(());
    }
    if !ctx.windows.is_open(m.window_id) {
        tracing::warn!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃连续滚动:窗口未打开"
        );
        return Ok(());
    }
    if let Some(origin) = ctx.windows.desktop_origin(m.window_id) {
        return ctx
            .state
            .injector
            .lock()
            .expect("injector 锁中毒")
            .scroll_pixel_screen(
                origin,
                m.x.round() as i32,
                m.y.round() as i32,
                m.pixels_x,
                m.pixels_y,
            )
            .map_err(|err| anyhow::anyhow!("完整桌面注入连续滚动失败: {err:#}"));
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .scroll_pixel(
            m.window_id as isize,
            m.x.round() as i32,
            m.y.round() as i32,
            m.pixels_x,
            m.pixels_y,
        )
        .map_err(|err| anyhow::anyhow!("注入连续滚动失败: {err:#}"))
}

/// 键盘:无 window_id,以前台窗口为准,且前台必须属于本会话已打开窗口
/// (只看不控窗口丢弃)。若当前前台暂未切到目标窗口,尝试将最近点击的已打开窗口
/// 置前一次,减少跨平台操控时“点了但打不了字”的挫败感。
fn key_event(ctx: &SessionCtx, m: &KeyEvent) -> anyhow::Result<()> {
    if !ctx.windows.has_open_desktop() {
        if foreground_view_only(ctx) {
            tracing::debug!(name = %ctx.peer_name, "丢弃按键:前台窗口为只看不控");
            return Ok(());
        }
        if !foreground_open(ctx) {
            try_focus_last_active(ctx);
        }
        if !foreground_open(ctx) {
            tracing::debug!(name = %ctx.peer_name, "丢弃按键:前台窗口不属于本会话");
            return Ok(());
        }
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .key(m.scancode, m.extended, m.down)
        .map_err(|err| anyhow::anyhow!("注入按键失败: {err:#}"))
}

/// 文本(IME 结果/软键盘):准入条件同 [`key_event`]。
fn text_input(ctx: &SessionCtx, m: &TextInput) -> anyhow::Result<()> {
    if !ctx.windows.has_open_desktop() {
        if foreground_view_only(ctx) {
            tracing::debug!(name = %ctx.peer_name, "丢弃文本:前台窗口为只看不控");
            return Ok(());
        }
        if !foreground_open(ctx) {
            try_focus_last_active(ctx);
        }
        if !foreground_open(ctx) {
            tracing::debug!(name = %ctx.peer_name, "丢弃文本:前台窗口不属于本会话");
            return Ok(());
        }
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .text(&m.text)
        .map_err(|err| anyhow::anyhow!("注入文本失败: {err:#}"))
}

// ---------------------------------------------------------------------------
// datagram(鼠标移动,05_api §3)
// ---------------------------------------------------------------------------

/// datagram 帧长:`u8 type + u64 window_id + i32 x + i32 y + u32 seq`(小端)。
const MOVE_DATAGRAM_LEN: usize = 1 + 8 + 4 + 4 + 4;

/// 解析后的鼠标移动 datagram(type=1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMove {
    pub window_id: u64,
    pub x: i32,
    pub y: i32,
    pub seq: u32,
}

/// 解析 `type=1` 鼠标移动 datagram。定长帧:长度不符或 type 未知均报错。
pub fn parse_mouse_move(bytes: &[u8]) -> anyhow::Result<MouseMove> {
    let raw: &[u8; MOVE_DATAGRAM_LEN] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!(
            "datagram 长度不符: {} 字节(需 {MOVE_DATAGRAM_LEN})",
            bytes.len()
        )
    })?;
    if raw[0] != 1 {
        anyhow::bail!("未知 datagram type: {}", raw[0]);
    }
    Ok(MouseMove {
        window_id: u64::from_le_bytes(raw[1..9].try_into().expect("8 字节")),
        x: i32::from_le_bytes(raw[9..13].try_into().expect("4 字节")),
        y: i32::from_le_bytes(raw[13..17].try_into().expect("4 字节")),
        seq: u32::from_le_bytes(raw[17..21].try_into().expect("4 字节")),
    })
}

/// 每连接、每窗口最近接受的 datagram seq(乱序旧包丢弃,任务卡步骤 2)。
/// 顶层键为连接(`Arc<Conn>` 指针地址):控制端按连接重新计数 seq,断开即
/// 失效;`on_disconnect` 整键清除。ctx 存活期间连接必然存活,指针不会复用。
static LAST_SEQ: LazyLock<Mutex<HashMap<usize, HashMap<u64, u32>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `seq` 是否比 `last` 更新:回绕语义(与 TCP 序号一致);恰差半程的歧义
/// 保守视为旧包。
fn seq_is_newer(last: u32, seq: u32) -> bool {
    seq != last && (seq.wrapping_sub(last) as i32) > 0
}

/// 记录并判断该 (连接, 窗口) 的 datagram 是否接受(新包)。重复/乱序旧包
/// 返回 false。
fn accept_seq(ctx: &SessionCtx, window_id: u64, seq: u32) -> bool {
    let conn_key = Arc::as_ptr(&ctx.conn) as usize;
    let mut all = LAST_SEQ.lock().expect("LAST_SEQ 锁中毒");
    let per_window = all.entry(conn_key).or_default();
    let accept = match per_window.get(&window_id) {
        Some(&last) => seq_is_newer(last, seq),
        None => true,
    };
    if accept {
        per_window.insert(window_id, seq);
    }
    accept
}

/// 处理一条 QUIC datagram(鼠标移动:`u8 type=1, u64 window_id, i32 x, i32 y,
/// u32 seq`,05_api §3)。同样受暂停与「仅已打开窗口」门约束;移动高频,
/// 未打开/乱序只记 debug,避免刷日志。
pub fn on_datagram(ctx: &SessionCtx, bytes: &[u8]) -> anyhow::Result<()> {
    let mv = parse_mouse_move(bytes)?;
    if ctx.state.paused.load(Ordering::SeqCst) {
        return Ok(());
    }
    // 自控自:高频移动注入与控制端光标互相拉扯(移动 → 真光标动 → 控制端
    // 又收到 pointermove → 再注入),停用;点击/键盘的"穿透"见 handle。
    // 真实场景里远端光标跟随靠画面内绘制的软光标(控制端),不依赖注入。
    if ctx.loopback {
        return Ok(());
    }
    // 本机优先(D-013 包5):高频移动路径同样挂起(原子读,便宜)
    if crate::host_activity::busy() {
        return Ok(());
    }
    if !ctx.windows.is_open(mv.window_id) {
        tracing::debug!(
            name = %ctx.peer_name,
            window_id = mv.window_id,
            "丢弃鼠标移动:窗口未打开"
        );
        return Ok(());
    }
    if !accept_seq(ctx, mv.window_id, mv.seq) {
        return Ok(());
    }
    // 只看不控窗口丢弃移动(严格只读;datagram 高频只记 debug 防刷屏)
    if ctx.windows.is_view_only(mv.window_id) {
        return Ok(());
    }
    if let Some(origin) = ctx.windows.desktop_origin(mv.window_id) {
        return ctx
            .state
            .injector
            .lock()
            .expect("injector 锁中毒")
            .mouse_move_screen(origin, mv.x, mv.y)
            .map_err(|err| anyhow::anyhow!("完整桌面注入鼠标移动失败: {err:#}"));
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .mouse_move(mv.window_id as isize, mv.x, mv.y)
        .map_err(|err| anyhow::anyhow!("注入鼠标移动失败: {err:#}"))
}

/// 每连接最近交互（点击）的窗口 id：键盘/文本到达时若前台尚未切回，
/// 可尝试置前该窗口，避免 Windows 前台锁导致打字无响应。
static LAST_ACTIVE_WIN: LazyLock<Mutex<HashMap<usize, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn record_active_window(ctx: &SessionCtx, window_id: u64) {
    let conn_key = Arc::as_ptr(&ctx.conn) as usize;
    if let Ok(mut map) = LAST_ACTIVE_WIN.lock() {
        map.insert(conn_key, window_id);
    }
}

fn try_focus_last_active(ctx: &SessionCtx) {
    let conn_key = Arc::as_ptr(&ctx.conn) as usize;
    let last = LAST_ACTIVE_WIN
        .lock()
        .ok()
        .and_then(|map| map.get(&conn_key).copied());
    if let Some(wid) = last {
        if ctx.windows.is_open(wid) && !ctx.windows.is_view_only(wid) {
            let _ = ensure_fg(wid);
        }
    }
}

/// 会话断开:抬起全部已按下键/鼠标键,防目标窗口卡键(DoD),并清除本连接
/// 的 datagram seq 记录(重连后控制端 seq 重新计数,旧记录会导致全丢弃)。
pub fn on_disconnect(ctx: &SessionCtx) {
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .release_all();
    let conn_key = Arc::as_ptr(&ctx.conn) as usize;
    LAST_SEQ.lock().expect("LAST_SEQ 锁中毒").remove(&conn_key);
    if let Ok(mut map) = LAST_ACTIVE_WIN.lock() {
        map.remove(&conn_key);
    }
    tracing::debug!(name = %ctx.peer_name, "输入断开清理:已抬起全部按下状态");
}

// ---------------------------------------------------------------------------
// 前台辅助
// ---------------------------------------------------------------------------

/// 当前前台窗口 hwnd(无前台为 None)。
#[cfg(windows)]
fn foreground_hwnd() -> Option<u64> {
    let hwnd = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd.0 as u64)
}

#[cfg(not(windows))]
fn foreground_hwnd() -> Option<u64> {
    // macOS：取当前可见窗口列表首位作为前台窗口（CGWindowList 按 Z 序从前到后）
    rd_mac::window::list_windows()
        .ok()
        .and_then(|list| list.first().map(|w| w.hwnd as u64))
}

/// 前台窗口是否属于本会话已打开的窗口(键盘/文本注入的准入条件)。
fn foreground_open(ctx: &SessionCtx) -> bool {
    matches!(foreground_hwnd(), Some(fg) if ctx.windows.is_open(fg))
}

/// 前台窗口是否为只看不控(v0.1/D-013 包4;键盘/文本注入的前置否决)。
fn foreground_view_only(ctx: &SessionCtx) -> bool {
    matches!(foreground_hwnd(), Some(fg) if ctx.windows.is_view_only(fg))
}

/// 确保 `hwnd` 置前(鼠标按下前调用);失败(前台锁拒绝等)返回 false。
#[cfg(windows)]
fn ensure_fg(window_id: u64) -> bool {
    rd_input_win::ensure_foreground(window_id as isize)
}

#[cfg(not(windows))]
fn ensure_fg(window_id: u64) -> bool {
    rd_mac::input::ensure_foreground(window_id as isize).is_ok()
}

// ---------------------------------------------------------------------------
// 单测:datagram 解析与 seq 乱序判定(任务卡测试要求)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 05_api §3 编码一条 datagram(小端)。
    fn encode(ty: u8, window_id: u64, x: i32, y: i32, seq: u32) -> Vec<u8> {
        let mut out = vec![ty];
        out.extend_from_slice(&window_id.to_le_bytes());
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out
    }

    #[test]
    fn parses_mouse_move_datagram() {
        let mv = parse_mouse_move(&encode(1, 0x1234, -5, 700, 42)).expect("合法包");
        assert_eq!(
            mv,
            MouseMove {
                window_id: 0x1234,
                x: -5,
                y: 700,
                seq: 42,
            }
        );
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(parse_mouse_move(&[]).is_err());
        let full = encode(1, 1, 0, 0, 0);
        assert!(parse_mouse_move(&full[..MOVE_DATAGRAM_LEN - 1]).is_err());
        // 定长帧:多余字节同样拒绝
        let mut extra = full.clone();
        extra.push(0);
        assert!(parse_mouse_move(&extra).is_err());
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(parse_mouse_move(&encode(0, 1, 0, 0, 0)).is_err());
        assert!(parse_mouse_move(&encode(2, 1, 0, 0, 0)).is_err());
    }

    #[test]
    fn negative_coordinates_survive_roundtrip() {
        let mv =
            parse_mouse_move(&encode(1, u64::MAX, i32::MIN, i32::MAX, u32::MAX)).expect("边界值包");
        assert_eq!(mv.x, i32::MIN);
        assert_eq!(mv.y, i32::MAX);
        assert_eq!(mv.window_id, u64::MAX);
        assert_eq!(mv.seq, u32::MAX);
    }

    #[test]
    fn seq_ordering_follows_wrapping_semantics() {
        assert!(seq_is_newer(1, 2)); // 新包
        assert!(!seq_is_newer(5, 5)); // 重复
        assert!(!seq_is_newer(5, 4)); // 乱序旧包
        assert!(seq_is_newer(u32::MAX, 0)); // 回绕:MAX 之后 0 是新包
        assert!(!seq_is_newer(0, u32::MAX)); // 回绕方向的旧包
        assert!(!seq_is_newer(0, 0x8000_0000)); // 半程歧义,保守视为旧包
    }
}
