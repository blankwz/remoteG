//! 鼠标注入与坐标映射：客户区坐标 → 虚拟桌面归一化绝对坐标（T-012）。
//!
//! `SendInput` 的绝对鼠标坐标是 0..=65535 的归一化值，配合
//! `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` 覆盖整个虚拟桌面
//! （多显示器含负坐标区域），因此任意窗口客户区坐标都能直接映射，无需先
//! 把光标约束到单块显示器。

use anyhow::{bail, Result};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use crate::{
    MOUSE_BUTTON_LEFT, MOUSE_BUTTON_MIDDLE, MOUSE_BUTTON_RIGHT, MOUSE_BUTTON_X1, MOUSE_BUTTON_X2,
};

/// 客户区坐标 → 虚拟桌面归一化绝对坐标（0..=65535）。
///
/// - `origin`：窗口客户区原点的屏幕坐标，即 `ClientToScreen((0,0))`。
/// - `vdesk`：`(SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SM_CXVIRTUALSCREEN,
///   SM_CYVIRTUALSCREEN)`。
///
/// 归一化用 `65535 / (size - 1)` 而非 MSDN 的 `65536 / size`：前者保证像素
/// 0 → 0、最右/最下像素 → 65535 双向无损，反向取整误差 ≤ 1 px。
/// 多显示器负坐标同样成立；越界输入钳制到 0..=65535，退化尺寸不除零。
pub fn client_to_absolute(
    origin: (i32, i32),
    x: i32,
    y: i32,
    vdesk: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (vx, vy, vw, vh) = vdesk;
    // (size - 1) 在 size ∈ {0, 1} 时为 0 或负，钳到 1 防除零。
    let denom_x = (vw - 1).max(1);
    let denom_y = (vh - 1).max(1);
    let nx = ((i64::from(origin.0) + i64::from(x) - i64::from(vx)) * 65535 / i64::from(denom_x))
        .clamp(0, 65535);
    let ny = ((i64::from(origin.1) + i64::from(y) - i64::from(vy)) * 65535 / i64::from(denom_y))
        .clamp(0, 65535);
    (nx as i32, ny as i32)
}

/// 线程级 Per-Monitor-V2 DPI 感知守卫：融合进程（Tauri + WebView2）中某些工作线程
/// 可能继承系统缩放上下文，导致 `GetSystemMetrics(SM_*VIRTUALSCREEN)` / `ClientToScreen`
/// 返回逻辑像素（125%/150% 缩放下与 WGC 物理像素产生 25%~50% 鼠标坐标偏移）。
/// 每次查询几何与调用 `SendInput` 时临时切到 Per-Monitor-V2 并在退出时恢复。
struct ThreadDpiV2Guard(DPI_AWARENESS_CONTEXT);

impl ThreadDpiV2Guard {
    fn enter() -> Self {
        let prev =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self(prev)
    }
}

impl Drop for ThreadDpiV2Guard {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            unsafe {
                let _ = SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}

/// 当前虚拟桌面矩形（多显示器并集，物理像素，含负坐标）。
pub(crate) fn virtual_desktop() -> (i32, i32, i32, i32) {
    let _dpi = ThreadDpiV2Guard::enter();
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// 窗口客户区原点的屏幕坐标（物理像素；强制线程级 Per-Monitor-V2 DPI 感知）。
pub(crate) fn client_origin(hwnd: isize) -> Result<(i32, i32)> {
    let _dpi = ThreadDpiV2Guard::enter();
    let mut pt = POINT { x: 0, y: 0 };
    let ok = unsafe { ClientToScreen(HWND(hwnd as *mut core::ffi::c_void), &mut pt) };
    if !ok.as_bool() {
        bail!("ClientToScreen 失败：hwnd = {hwnd:#x} 可能已失效");
    }
    Ok((pt.x, pt.y))
}

/// 尽力开启进程级 Per-Monitor-V2 DPI 感知（已由 manifest/其他组件设置时失败可忽略）。
pub(crate) fn set_process_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// 一次 `SendInput` 注入全部事件；注入数不足视为失败（如 UIPI 拦截）。
pub(crate) fn send(inputs: &[INPUT]) -> Result<()> {
    let _dpi = ThreadDpiV2Guard::enter();
    let want = inputs.len() as u32;
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != want {
        let err = windows::core::Error::from_thread();
        bail!(
            "SendInput 仅注入 {sent}/{want}：{err}（常见原因：目标窗口以管理员运行，受 UIPI 保护）"
        );
    }
    Ok(())
}

/// 归一化绝对坐标移动。
pub(crate) fn build_move(abs: (i32, i32)) -> INPUT {
    mouse_input(
        abs.0,
        abs.1,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

/// 鼠标按钮按下/抬起（协议按钮编号 0左/1右/2中/3X1/4X2）。
pub(crate) fn build_button(button: u32, down: bool) -> Result<INPUT> {
    let (down_flag, up_flag) = match button {
        MOUSE_BUTTON_LEFT => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MOUSE_BUTTON_RIGHT => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MOUSE_BUTTON_MIDDLE => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        MOUSE_BUTTON_X1 | MOUSE_BUTTON_X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
        other => bail!("未知鼠标按钮编号 {other}（协议为 0左/1右/2中/3X1/4X2）"),
    };
    let data = match button {
        MOUSE_BUTTON_X1 => u32::from(XBUTTON1),
        MOUSE_BUTTON_X2 => u32::from(XBUTTON2),
        _ => 0,
    };
    Ok(mouse_input(
        0,
        0,
        data,
        if down { down_flag } else { up_flag },
    ))
}

/// 滚轮：`dy` 垂直（`MOUSEEVENTF_WHEEL`）、`dx` 水平（`MOUSEEVENTF_HWHEEL`），
/// 值即 wheel delta（±120 的倍数）。
pub(crate) fn build_wheel(dx: i32, dy: i32) -> Vec<INPUT> {
    let mut inputs = Vec::new();
    if dy != 0 {
        inputs.push(mouse_input(0, 0, dy as u32, MOUSEEVENTF_WHEEL));
    }
    if dx != 0 {
        inputs.push(mouse_input(0, 0, dx as u32, MOUSEEVENTF_HWHEEL));
    }
    inputs
}

fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
