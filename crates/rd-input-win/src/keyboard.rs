//! 键盘注入：扫描码透传 + `KEYEVENTF_UNICODE` 文本输入（T-012）。
//!
//! 扫描码注入（`KEYEVENTF_SCANCODE`）让目标窗口经系统键盘布局自然解释按键
//! （应用感知的修饰键状态、快捷键均正常）；文本注入绕过布局直投 Unicode
//! 码元，代理对拆成两次注入即可送达完整字符（如 😀）。

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MAPVK_VSC_TO_VK, VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_MENU,
};

/// 扫描码按键 INPUT（`extended` 位加 `KEYEVENTF_EXTENDEDKEY`）。
pub(crate) fn build_scancode(scancode: u32, extended: bool, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode as u16,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Unicode 码元按键 INPUT（UTF-16 单码元，代理对由调用方逐码元注入）。
pub(crate) fn build_unicode(unit: u16, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 虚拟键按键 INPUT（不带扫描码；置前时抖动 Alt 用）。
pub(crate) fn build_vk(vk: VIRTUAL_KEY, down: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 该扫描码 + 当前修饰键状态是否构成 Ctrl+Alt+Del（SAS 序列，无法注入）。
///
/// Ctrl/Alt 的判断用 `GetAsyncKeyState`，同时覆盖物理按住与已注入按下两种来源。
pub(crate) fn is_ctrl_alt_delete(scancode: u32) -> bool {
    let vk = unsafe { MapVirtualKeyW(scancode, MAPVK_VSC_TO_VK) };
    if VIRTUAL_KEY(vk as u16) != VK_DELETE {
        return false;
    }
    let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
    let alt = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0;
    ctrl && alt
}
