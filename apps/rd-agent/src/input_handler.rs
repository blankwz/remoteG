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

use rd_proto::v0::{KeyEvent, MouseButton, MouseWheel, TextInput};

use crate::session::SessionCtx;

/// 键鼠注入器:Windows 下为 rd-input-win 实现(记录按下状态,断开时全抬);
/// 其他平台为空桩,保持整个 crate 可跨平台 check。
#[cfg(windows)]
pub use rd_input_win::Injector;

/// 非 Windows 平台注入桩:仅满足类型签名,全部为空操作。
#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct Injector;

#[cfg(not(windows))]
impl Injector {
    /// 与 Windows 版签名对齐的空构造。
    pub fn new() -> Self {
        Self
    }

    pub fn mouse_move(&mut self, _hwnd: isize, _x: i32, _y: i32) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn mouse_button(
        &mut self,
        _hwnd: isize,
        _x: i32,
        _y: i32,
        _button: u32,
        _down: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn wheel(
        &mut self,
        _hwnd: isize,
        _x: i32,
        _y: i32,
        _dx: i32,
        _dy: i32,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn key(&mut self, _scancode: u32, _extended: bool, _down: bool) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn text(&mut self, _s: &str) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn release_all(&mut self) {}
}

/// 控制流承载的输入消息(点击/按键需可靠送达,05_api 摘要)。
#[derive(Debug)]
pub enum InputMsg {
    Key(KeyEvent),
    MouseButton(MouseButton),
    MouseWheel(MouseWheel),
    TextInput(TextInput),
}

/// 处理一条控制流输入消息(同步调用,内部为阻塞 Win32 注入;置前失败重试
/// 最长数百毫秒,点击/按键为人手频率,不构成调度问题)。
pub fn handle(ctx: &SessionCtx, msg: InputMsg) -> anyhow::Result<()> {
    // 暂停远程输入:全部丢弃
    if ctx.state.paused.load(Ordering::SeqCst) {
        return Ok(());
    }
    match msg {
        InputMsg::MouseButton(m) => mouse_button(ctx, &m),
        InputMsg::MouseWheel(m) => mouse_wheel(ctx, &m),
        InputMsg::Key(m) => key_event(ctx, &m),
        InputMsg::TextInput(m) => text_input(ctx, &m),
    }
}

/// 鼠标点击:仅限已打开窗口;按下前置前(见模块注释)。
fn mouse_button(ctx: &SessionCtx, m: &MouseButton) -> anyhow::Result<()> {
    if !ctx.windows.is_open(m.window_id) {
        tracing::warn!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃鼠标点击:窗口未打开"
        );
        return Ok(());
    }
    // 只在按下时置前;抬起不要求(否则窗口中途失焦会吞掉抬起,造成卡键)
    if m.down && !ensure_fg(m.window_id) {
        tracing::warn!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃鼠标按下:置前失败"
        );
        return Ok(());
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .mouse_button(m.window_id as isize, m.x, m.y, m.button, m.down)
        .map_err(|err| anyhow::anyhow!("注入鼠标按键失败: {err:#}"))
}

/// 滚轮:仅限已打开窗口;无按下语义,不置前(滚动落在光标下的目标窗口)。
fn mouse_wheel(ctx: &SessionCtx, m: &MouseWheel) -> anyhow::Result<()> {
    if !ctx.windows.is_open(m.window_id) {
        tracing::warn!(
            name = %ctx.peer_name,
            window_id = m.window_id,
            "丢弃滚轮:窗口未打开"
        );
        return Ok(());
    }
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .wheel(m.window_id as isize, m.x, m.y, m.dx, m.dy)
        .map_err(|err| anyhow::anyhow!("注入滚轮失败: {err:#}"))
}

/// 键盘:无 window_id,以前台窗口为准,且前台必须属于本会话已打开窗口。
/// 修饰键由控制端作为独立 KeyEvent 扫描码事件下发,`modifiers` 位图仅作
/// 信息参考,不在此换算。
fn key_event(ctx: &SessionCtx, m: &KeyEvent) -> anyhow::Result<()> {
    if !foreground_open(ctx) {
        tracing::debug!(name = %ctx.peer_name, "丢弃按键:前台窗口不属于本会话");
        return Ok(());
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
    if !foreground_open(ctx) {
        tracing::debug!(name = %ctx.peer_name, "丢弃文本:前台窗口不属于本会话");
        return Ok(());
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
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .mouse_move(mv.window_id as isize, mv.x, mv.y)
        .map_err(|err| anyhow::anyhow!("注入鼠标移动失败: {err:#}"))
}

/// 会话断开:抬起全部已按下键/鼠标键,防目标窗口卡键(DoD),并清除本连接
/// 的 datagram seq 记录(重连后控制端 seq 重新计数,旧记录会导致全丢弃)。
pub fn on_disconnect(ctx: &SessionCtx) {
    ctx.state
        .injector
        .lock()
        .expect("injector 锁中毒")
        .release_all();
    LAST_SEQ
        .lock()
        .expect("LAST_SEQ 锁中毒")
        .remove(&(Arc::as_ptr(&ctx.conn) as usize));
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
    None
}

/// 前台窗口是否属于本会话已打开的窗口(键盘/文本注入的准入条件)。
fn foreground_open(ctx: &SessionCtx) -> bool {
    matches!(foreground_hwnd(), Some(fg) if ctx.windows.is_open(fg))
}

/// 确保 `hwnd` 置前(鼠标按下前调用);失败(前台锁拒绝等)返回 false。
#[cfg(windows)]
fn ensure_fg(window_id: u64) -> bool {
    rd_input_win::ensure_foreground(window_id as isize)
}

#[cfg(not(windows))]
fn ensure_fg(_window_id: u64) -> bool {
    false
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
