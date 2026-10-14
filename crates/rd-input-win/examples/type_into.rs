//! 在指定标题的窗口中置前、点击客户区中心、输入 "Hello 你好 😀"（T-012 手测）。
//!
//! 用法：
//! ```text
//! cargo run -p rd-input-win --example type_into -- <窗口标题子串>
//! ```
//! 例：先启动记事本（`start notepad`），再运行 `cargo run -p rd-input-win
//! --example type_into -- 无标题`。

#![cfg(windows)]

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowTextW, IsWindowVisible,
};

struct Finder {
    needle: String,
    found: Option<isize>,
}

static FINDER: Mutex<Option<Finder>> = Mutex::new(None);

fn main() -> Result<()> {
    let needle = std::env::args()
        .nth(1)
        .context("用法：type_into <窗口标题子串>")?;

    // example 自身也要 Per-Monitor-V2 感知（客户区尺寸为物理像素）。
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let hwnd = find_window(&needle).context(format!("找不到标题含「{needle}」的可见窗口"))?;
    println!("目标 HWND: {hwnd:#x}");

    // 后台窗口先置前（分层：Alt 抖动 → AttachThreadInput → SwitchToThisWindow）。
    // 失败不终止：随后的真实点击本身也会把窗口激活。
    if !rd_input_win::ensure_foreground(hwnd) {
        std::thread::sleep(Duration::from_millis(120));
        if !rd_input_win::ensure_foreground(hwnd) {
            eprintln!("警告：置前 API 未生效，继续执行（点击将激活窗口）");
        }
    }
    std::thread::sleep(Duration::from_millis(200));

    let (w, h) = client_size(hwnd)?;
    let (cx, cy) = (w / 2, h / 2);
    println!("客户区 {w}x{h}，点击 ({cx}, {cy})");

    let mut injector = rd_input_win::Injector::new();
    injector.mouse_move(hwnd, cx, cy)?;
    injector.mouse_button(hwnd, cx, cy, rd_input_win::MOUSE_BUTTON_LEFT, true)?;
    std::thread::sleep(Duration::from_millis(30));
    injector.mouse_button(hwnd, cx, cy, rd_input_win::MOUSE_BUTTON_LEFT, false)?;
    println!("已左键点击");

    std::thread::sleep(Duration::from_millis(150));
    injector.text("Hello 你好 😀")?;
    println!("已注入文本，请目视确认窗口内容");
    Ok(())
}

/// 按标题子串查找第一个可见顶层窗口。
fn find_window(needle: &str) -> Result<isize> {
    *FINDER.lock().expect("FINDER 锁") = Some(Finder {
        needle: needle.to_owned(),
        found: None,
    });
    // 回调匹配到目标会提前返回 FALSE 停止枚举，此时 EnumWindows 返回值不可信，
    // 以 FINDER.found 为准。
    let _ = unsafe { EnumWindows(Some(enum_callback), LPARAM(0)) };
    FINDER
        .lock()
        .expect("FINDER 锁")
        .as_ref()
        .and_then(|f| f.found)
        .context("枚举完成但未匹配到窗口")
}

unsafe extern "system" fn enum_callback(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let Ok(mut guard) = FINDER.lock() else {
        return BOOL(1);
    };
    let Some(finder) = guard.as_mut() else {
        return BOOL(1);
    };
    if finder.found.is_some() {
        return BOOL(0);
    }
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len > 0 {
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        if title.contains(&finder.needle) {
            finder.found = Some(hwnd.0 as isize);
            return BOOL(0);
        }
    }
    BOOL(1)
}

/// 窗口客户区尺寸（物理像素）。
fn client_size(hwnd: isize) -> Result<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(HWND(hwnd as *mut core::ffi::c_void), &mut rect) }
        .map_err(|e| anyhow::anyhow!("GetClientRect 失败：hwnd = {hwnd:#x}，{e}"))?;
    Ok((rect.right - rect.left, rect.bottom - rect.top))
}
