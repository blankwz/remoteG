//! rd-input-win — 键鼠注入（SendInput）、坐标映射、置前。
//!
//! 架构（03_architecture/架构方案.md §3.4）：
//! - 鼠标：控制端坐标 → 根窗口客户区坐标 → 屏幕坐标 → `SendInput`
//!   （`MOUSEEVENTF_ABSOLUTE|VIRTUALDESK`，归一化 0..=65535）。
//! - 键盘：扫描码透传（`KEYEVENTF_SCANCODE`）；文本（IME 结果/软键盘）用
//!   `KEYEVENTF_UNICODE` 逐 UTF-16 码元注入。
//! - 焦点：首次输入或焦点不在目标时 `ensure_foreground`（Alt 抖动绕过前台锁）。
//! - 断开：`Injector::release_all` 抬起全部记录为按下的键/鼠标键，防卡键。

#[cfg(windows)]
pub mod focus;
#[cfg(windows)]
pub mod keyboard;
#[cfg(windows)]
pub mod mouse;

#[cfg(windows)]
pub use focus::ensure_foreground;
#[cfg(windows)]
pub use mouse::client_to_absolute;

/// 协议 `MouseButton.button`：左键。
#[cfg(windows)]
pub const MOUSE_BUTTON_LEFT: u32 = 0;
/// 协议 `MouseButton.button`：右键。
#[cfg(windows)]
pub const MOUSE_BUTTON_RIGHT: u32 = 1;
/// 协议 `MouseButton.button`：中键。
#[cfg(windows)]
pub const MOUSE_BUTTON_MIDDLE: u32 = 2;
/// 协议 `MouseButton.button`：X1（侧键后退）。
#[cfg(windows)]
pub const MOUSE_BUTTON_X1: u32 = 3;
/// 协议 `MouseButton.button`：X2（侧键前进）。
#[cfg(windows)]
pub const MOUSE_BUTTON_X2: u32 = 4;

/// 键鼠注入器。
///
/// 记录当前被注入为“按下”的键与鼠标按钮，断开时调用 [`Injector::release_all`]
/// 全部抬起，避免目标窗口卡键。
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct Injector {
    /// 已按下的键盘键：(扫描码, extended)。
    keys_down: std::collections::HashSet<(u32, bool)>,
    /// 已按下的鼠标按钮：协议按钮编号（0左/1右/2中/3X1/4X2）。
    buttons_down: std::collections::HashSet<u32>,
}

#[cfg(windows)]
impl Injector {
    /// 创建注入器，并尽力开启进程级 Per-Monitor-V2 DPI 感知。
    ///
    /// DPI 感知保证 `ClientToScreen`/`GetClientRect` 返回物理像素（150% 缩放下
    /// 点击才准确）。若 T-015 的 manifest 已声明同名感知级别，此处失败可忽略。
    pub fn new() -> Self {
        mouse::set_process_dpi_awareness();
        Self::default()
    }

    /// 把光标移动到 `hwnd` 客户区 `(x, y)`（物理像素）。
    pub fn mouse_move(&mut self, hwnd: isize, x: i32, y: i32) -> anyhow::Result<()> {
        let abs = self.absolute(hwnd, x, y)?;
        mouse::send(&[mouse::build_move(abs)])
    }

    /// 在 `hwnd` 客户区 `(x, y)` 处按下/抬起鼠标按钮（先移动到位再按键）。
    pub fn mouse_button(
        &mut self,
        hwnd: isize,
        x: i32,
        y: i32,
        button: u32,
        down: bool,
    ) -> anyhow::Result<()> {
        let abs = self.absolute(hwnd, x, y)?;
        let mut inputs = vec![mouse::build_move(abs)];
        inputs.push(mouse::build_button(button, down)?);
        mouse::send(&inputs)?;
        if down {
            self.buttons_down.insert(button);
        } else {
            self.buttons_down.remove(&button);
        }
        Ok(())
    }

    /// 在 `hwnd` 客户区 `(x, y)` 处滚动滚轮。
    ///
    /// `dx`/`dy` 单位为 wheel delta（`WHEEL_DELTA` = 120 的倍数，通常 ±120）。
    pub fn wheel(&mut self, hwnd: isize, x: i32, y: i32, dx: i32, dy: i32) -> anyhow::Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        let abs = self.absolute(hwnd, x, y)?;
        let mut inputs = vec![mouse::build_move(abs)];
        inputs.extend(mouse::build_wheel(dx, dy));
        mouse::send(&inputs)
    }

    /// 像素级连续滚动（触控板/手势翻译路径，协议 `ContinuousScroll` 消费）。
    ///
    /// 与 [`wheel`](Self::wheel) 的差异：`wheel` 的 `dx`/`dy` 是离散滚轮档位
    /// （±120），这里输入是像素增量——手势翻译（拖动/惯性）以 60Hz 级频率
    /// 送来小步增量，直接把像素当 wheel delta 注入（**1px = 1 delta**：
    /// Chrome/Edge/资源管理器按 delta 比例平滑滚动，观感近似 1:1；应用行高
    /// 各异，手感调参改这个系数）。phase/momentum 已在控制端消费成连续帧，
    /// 被控端无状态——惯性物理永远在发送侧。
    pub fn scroll_pixel(
        &mut self,
        hwnd: isize,
        x: i32,
        y: i32,
        pixels_x: i32,
        pixels_y: i32,
    ) -> anyhow::Result<()> {
        if pixels_x == 0 && pixels_y == 0 {
            return Ok(());
        }
        let abs = self.absolute(hwnd, x, y)?;
        let mut inputs = vec![mouse::build_move(abs)];
        inputs.extend(mouse::build_wheel(pixels_x, pixels_y));
        mouse::send(&inputs)
    }

    /// 注入一次键盘事件（扫描码透传，`extended` 加 `KEYEVENTF_EXTENDEDKEY`）。
    ///
    /// Ctrl+Alt+Del 属系统安全注意序列（SAS），`SendInput` 无法注入，返回错误。
    pub fn key(&mut self, scancode: u32, extended: bool, down: bool) -> anyhow::Result<()> {
        if down && keyboard::is_ctrl_alt_delete(scancode) {
            anyhow::bail!("Ctrl+Alt+Del 属系统安全注意序列（SAS），无法注入");
        }
        let input = keyboard::build_scancode(scancode, extended, down);
        mouse::send(&[input])?;
        if down {
            self.keys_down.insert((scancode, extended));
        } else {
            self.keys_down.remove(&(scancode, extended));
        }
        Ok(())
    }

    /// 注入文本：UTF-16 逐码元 `KEYEVENTF_UNICODE`（代理对自然拆成两次注入）。
    pub fn text(&mut self, s: &str) -> anyhow::Result<()> {
        for unit in s.encode_utf16() {
            mouse::send(&[
                keyboard::build_unicode(unit, true),
                keyboard::build_unicode(unit, false),
            ])?;
        }
        Ok(())
    }

    /// 抬起所有记录为按下的键与鼠标键（会话断开时防卡键）。
    pub fn release_all(&mut self) {
        let mut inputs = Vec::new();
        for &button in &self.buttons_down {
            if let Ok(input) = mouse::build_button(button, false) {
                inputs.push(input);
            }
        }
        for &(sc, ext) in &self.keys_down {
            inputs.push(keyboard::build_scancode(sc, ext, false));
        }
        if !inputs.is_empty() {
            let _ = mouse::send(&inputs);
        }
        self.buttons_down.clear();
        self.keys_down.clear();
    }

    /// `hwnd` 客户区 `(x, y)` → 虚拟桌面归一化绝对坐标。
    fn absolute(&self, hwnd: isize, x: i32, y: i32) -> anyhow::Result<(i32, i32)> {
        let origin = mouse::client_origin(hwnd)?;
        Ok(mouse::client_to_absolute(
            origin,
            x,
            y,
            mouse::virtual_desktop(),
        ))
    }
}
