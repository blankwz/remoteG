//! 置前：`SetForegroundWindow` 与前台锁绕过（T-012）。
//!
//! Windows 前台锁：调用进程自身不是前台时，`SetForegroundWindow` 只会让任务栏
//! 图标闪烁。绕过手段（03_architecture §3.4）按可靠性分层：
//! 1. `AllowSetForegroundWindow(ASFW_ANY)` + Alt 抖动（`WM_SYSKEYDOWN` 使系统
//!    视本进程为“正在响应用户输入”）+ `SetForegroundWindow`；
//! 2. 失败 → 再抖动一次 + `AttachThreadInput` 与前台窗口线程共享输入状态；
//! 3. 仍失败 → `SwitchToThisWindow` 兜底（Alt-Tab 同款路径；部分宿主进程树 /
//!    前台锁状态下 1、2 会被系统整体拒绝，实测该路径仍有效）。
//!
//! 500 ms 内对同一窗口不重复尝试，避免高频输入事件下反复抖动 Alt。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::keyboard;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    SwitchToThisWindow, ASFW_ANY,
};

/// 同一窗口两次置前尝试的最小间隔。
const RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// Alt 抖动注入后等待系统消化输入队列的时间（前台锁状态异步更新）。
const FOREGROUND_LOCK_SETTLE: Duration = Duration::from_millis(120);

/// 上一次置前尝试：(hwnd, 时刻)。
static LAST_ATTEMPT: Mutex<Option<(isize, Instant)>> = Mutex::new(None);

/// 确保 `hwnd` 在前台。
///
/// 已前台直接返回 `true`；否则按模块注释的分层流程置前并返回是否成功。
/// 500 ms 内已对同一窗口尝试过时不重复注入，直接返回当前前台判定。
pub fn ensure_foreground(hwnd: isize) -> bool {
    if foreground_hwnd() == Some(hwnd) {
        return true;
    }

    // 节流：锁失败（poisoned）视为不可重试，宁可不动前台也不 panic。
    let Ok(mut last) = LAST_ATTEMPT.lock() else {
        return false;
    };
    if let Some((target, at)) = *last {
        if target == hwnd && at.elapsed() < RETRY_INTERVAL {
            return foreground_hwnd() == Some(hwnd);
        }
    }
    *last = Some((hwnd, Instant::now()));
    drop(last);

    let target = HWND(hwnd as *mut core::ffi::c_void);
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        // 第一层：抖动 Alt 后等待注入输入被处理，再申请前台。
        jitter_alt();
        std::thread::sleep(FOREGROUND_LOCK_SETTLE);
        if SetForegroundWindow(target).as_bool() {
            return true;
        }
        // 第二层：再抖动 + 与前台窗口线程共享输入状态（AttachThreadInput），
        // 兜住无窗口控制台进程等前台锁规则覆盖不到的场景。
        jitter_alt();
        std::thread::sleep(FOREGROUND_LOCK_SETTLE);
        if force_foreground_via_input_attachment(target) {
            return true;
        }
        // 第三层：Alt-Tab 同款切换路径。实测部分宿主进程树 / 前台锁状态下
        // SetForegroundWindow 两层都被拒，该路径仍能完成切换。
        // 注：前置抖动的 Alt 需要时间消化，因此首切后复验、必要时再切一次。
        for _ in 0..2 {
            SwitchToThisWindow(target, true);
            std::thread::sleep(FOREGROUND_LOCK_SETTLE * 2);
            if foreground_hwnd() == Some(hwnd) {
                return true;
            }
        }
        false
    }
}

/// AttachThreadInput 兜底：与当前前台窗口的线程共享输入状态后，
/// `SetForegroundWindow` 不再受前台锁限制。
fn force_foreground_via_input_attachment(target: HWND) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return SetForegroundWindow(target).as_bool();
        }
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let cur_thread = GetCurrentThreadId();
        if fg_thread == 0 || fg_thread == cur_thread {
            return SetForegroundWindow(target).as_bool();
        }
        if !AttachThreadInput(cur_thread, fg_thread, true).as_bool() {
            return SetForegroundWindow(target).as_bool();
        }
        let ok = SetForegroundWindow(target).as_bool();
        let _ = AttachThreadInput(cur_thread, fg_thread, false);
        ok
    }
}

/// 注入一次 Alt 按下/抬起，让系统把本进程视为“正在响应用户输入”，
/// 从而解除 `SetForegroundWindow` 的前台锁。
fn jitter_alt() {
    let jitter = [
        keyboard::build_vk(VK_MENU, true),
        keyboard::build_vk(VK_MENU, false),
    ];
    unsafe {
        let _ = SendInput(jitter.as_slice(), std::mem::size_of::<INPUT>() as i32);
    }
}

/// 当前前台窗口（无前台时为 None）。
fn foreground_hwnd() -> Option<isize> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd.0 as isize)
    }
}
