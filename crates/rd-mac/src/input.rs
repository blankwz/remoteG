//! macOS 键鼠输入注入（CoreGraphics `CGEventPost`，对齐 `rd-input-win` 契约）。

use std::collections::HashSet;

use anyhow::Result;

pub const MOUSE_BUTTON_LEFT: u32 = 0;
pub const MOUSE_BUTTON_RIGHT: u32 = 1;
pub const MOUSE_BUTTON_MIDDLE: u32 = 2;
pub const MOUSE_BUTTON_X1: u32 = 3;
pub const MOUSE_BUTTON_X2: u32 = 4;

/// 置前目标窗口所属应用（macOS 通过 PID 激活；非 macOS 为 no-op）。
pub fn ensure_foreground(_window_id: isize) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(list) = crate::window::list_windows() {
            if let Some(w) = list.into_iter().find(|w| w.hwnd == _window_id) {
                let script = format!(
                    "tell application \"System Events\" to set frontmost of (first process whose unix id is {}) to true",
                    w.pid
                );
                let _ = std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .output();
            }
        }
    }
    Ok(())
}

/// 将 Windows/协议 PS/2 Set 1 扫描码映射为 macOS Virtual Keycode (`CGKeyCode`)。
pub fn ps2_scancode_to_mac_keycode(scancode: u32, extended: bool) -> Option<u16> {
    if extended {
        return match scancode {
            0x48 => Some(0x7E), // Up Arrow
            0x50 => Some(0x7D), // Down Arrow
            0x4B => Some(0x7B), // Left Arrow
            0x4D => Some(0x7C), // Right Arrow
            0x47 => Some(0x73), // Home
            0x4F => Some(0x77), // End
            0x49 => Some(0x74), // Page Up
            0x51 => Some(0x79), // Page Down
            0x53 => Some(0x75), // Forward Delete
            0x1D => Some(0x3E), // Right Control
            0x38 => Some(0x3D), // Right Option/Alt
            0x5B => Some(0x37), // Left Command (Win)
            0x5C => Some(0x36), // Right Command
            0x1C => Some(0x4C), // Keypad Enter
            _ => None,
        };
    }
    match scancode {
        0x01 => Some(0x35), // Escape
        0x02 => Some(0x12), // 1
        0x03 => Some(0x13), // 2
        0x04 => Some(0x14), // 3
        0x05 => Some(0x15), // 4
        0x06 => Some(0x17), // 5
        0x07 => Some(0x16), // 6
        0x08 => Some(0x1A), // 7
        0x09 => Some(0x1C), // 8
        0x0A => Some(0x19), // 9
        0x0B => Some(0x1D), // 0
        0x0C => Some(0x1B), // -
        0x0D => Some(0x18), // =
        0x0E => Some(0x33), // Backspace (Delete)
        0x0F => Some(0x30), // Tab
        0x10 => Some(0x0C), // Q
        0x11 => Some(0x0D), // W
        0x12 => Some(0x0E), // E
        0x13 => Some(0x0F), // R
        0x14 => Some(0x11), // T
        0x15 => Some(0x10), // Y
        0x16 => Some(0x20), // U
        0x17 => Some(0x22), // I
        0x18 => Some(0x1F), // O
        0x19 => Some(0x23), // P
        0x1A => Some(0x21), // [
        0x1B => Some(0x1E), // ]
        0x1C => Some(0x24), // Return
        0x1D => Some(0x3B), // Left Control
        0x1E => Some(0x00), // A
        0x1F => Some(0x01), // S
        0x20 => Some(0x02), // D
        0x21 => Some(0x03), // F
        0x22 => Some(0x05), // G
        0x23 => Some(0x04), // H
        0x24 => Some(0x26), // J
        0x25 => Some(0x28), // K
        0x26 => Some(0x25), // L
        0x27 => Some(0x29), // ;
        0x28 => Some(0x27), // '
        0x29 => Some(0x32), // `
        0x2A => Some(0x38), // Left Shift
        0x2B => Some(0x2A), // \
        0x2C => Some(0x06), // Z
        0x2D => Some(0x07), // X
        0x2E => Some(0x08), // C
        0x2F => Some(0x09), // V
        0x30 => Some(0x0B), // B
        0x31 => Some(0x2D), // N
        0x32 => Some(0x2E), // M
        0x33 => Some(0x2B), // ,
        0x34 => Some(0x2F), // .
        0x35 => Some(0x2C), // /
        0x36 => Some(0x3C), // Right Shift
        0x38 => Some(0x3A), // Left Option/Alt
        0x39 => Some(0x31), // Space
        0x3A => Some(0x39), // Caps Lock
        0x3B => Some(0x7A), // F1
        0x3C => Some(0x78), // F2
        0x3D => Some(0x63), // F3
        0x3E => Some(0x76), // F4
        0x3F => Some(0x60), // F5
        0x40 => Some(0x61), // F6
        0x41 => Some(0x62), // F7
        0x42 => Some(0x64), // F8
        0x43 => Some(0x65), // F9
        0x44 => Some(0x6D), // F10
        0x57 => Some(0x67), // F11
        0x58 => Some(0x6F), // F12
        _ => None,
    }
}

/// macOS 键鼠注入器（与 `rd-input-win::Injector` 同签名）。
#[derive(Debug, Default)]
pub struct Injector {
    keys_down: HashSet<(u32, bool)>,
    buttons_down: HashSet<u32>,
}

impl Injector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mouse_move(&mut self, hwnd: isize, x: i32, y: i32) -> Result<()> {
        let (sx, sy) = self.to_screen(hwnd, x, y);
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let ev_type = if self.buttons_down.contains(&MOUSE_BUTTON_LEFT) {
                kCGEventLeftMouseDragged
            } else if self.buttons_down.contains(&MOUSE_BUTTON_RIGHT) {
                kCGEventRightMouseDragged
            } else {
                kCGEventMouseMoved
            };
            let ev = CGEventCreateMouseEvent(
                std::ptr::null(),
                ev_type,
                CGPoint { x: sx, y: sy },
                kCGMouseButtonLeft,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        let _ = (sx, sy);
        Ok(())
    }

    pub fn mouse_button(
        &mut self,
        hwnd: isize,
        x: i32,
        y: i32,
        button: u32,
        down: bool,
    ) -> Result<()> {
        let (sx, sy) = self.to_screen(hwnd, x, y);
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let (ev_type, cg_btn) = match (button, down) {
                (MOUSE_BUTTON_LEFT, true) => (kCGEventLeftMouseDown, kCGMouseButtonLeft),
                (MOUSE_BUTTON_LEFT, false) => (kCGEventLeftMouseUp, kCGMouseButtonLeft),
                (MOUSE_BUTTON_RIGHT, true) => (kCGEventRightMouseDown, kCGMouseButtonRight),
                (MOUSE_BUTTON_RIGHT, false) => (kCGEventRightMouseUp, kCGMouseButtonRight),
                (_, true) => (kCGEventOtherMouseDown, kCGMouseButtonCenter),
                (_, false) => (kCGEventOtherMouseUp, kCGMouseButtonCenter),
            };
            let ev = CGEventCreateMouseEvent(
                std::ptr::null(),
                ev_type,
                CGPoint { x: sx, y: sy },
                cg_btn,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        let _ = (sx, sy);
        if down {
            self.buttons_down.insert(button);
        } else {
            self.buttons_down.remove(&button);
        }
        Ok(())
    }

    pub fn wheel(&mut self, hwnd: isize, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        self.mouse_move(hwnd, x, y)?;
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            // 协议以 120 为一格；转 macOS 行滚动（每格 3 行）
            let lines_y = (dy / 40).clamp(-30, 30);
            let lines_x = (dx / 40).clamp(-30, 30);
            let ev = CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                kCGScrollEventUnitLine,
                2,
                lines_y,
                lines_x,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        Ok(())
    }

    pub fn mouse_move_screen(&mut self, origin: (i32, i32), x: i32, y: i32) -> Result<()> {
        let sx = (origin.0 + x) as f64;
        let sy = (origin.1 + y) as f64;
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let ev_type = if self.buttons_down.contains(&MOUSE_BUTTON_LEFT) {
                kCGEventLeftMouseDragged
            } else if self.buttons_down.contains(&MOUSE_BUTTON_RIGHT) {
                kCGEventRightMouseDragged
            } else {
                kCGEventMouseMoved
            };
            let ev = CGEventCreateMouseEvent(
                std::ptr::null(),
                ev_type,
                CGPoint { x: sx, y: sy },
                kCGMouseButtonLeft,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        let _ = (sx, sy);
        Ok(())
    }

    pub fn mouse_button_screen(
        &mut self,
        origin: (i32, i32),
        x: i32,
        y: i32,
        button: u32,
        down: bool,
    ) -> Result<()> {
        let sx = (origin.0 + x) as f64;
        let sy = (origin.1 + y) as f64;
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let (ev_type, cg_btn) = match (button, down) {
                (MOUSE_BUTTON_LEFT, true) => (kCGEventLeftMouseDown, kCGMouseButtonLeft),
                (MOUSE_BUTTON_LEFT, false) => (kCGEventLeftMouseUp, kCGMouseButtonLeft),
                (MOUSE_BUTTON_RIGHT, true) => (kCGEventRightMouseDown, kCGMouseButtonRight),
                (MOUSE_BUTTON_RIGHT, false) => (kCGEventRightMouseUp, kCGMouseButtonRight),
                (_, true) => (kCGEventOtherMouseDown, kCGMouseButtonCenter),
                (_, false) => (kCGEventOtherMouseUp, kCGMouseButtonCenter),
            };
            let ev = CGEventCreateMouseEvent(
                std::ptr::null(),
                ev_type,
                CGPoint { x: sx, y: sy },
                cg_btn,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        let _ = (sx, sy);
        if down {
            self.buttons_down.insert(button);
        } else {
            self.buttons_down.remove(&button);
        }
        Ok(())
    }

    pub fn wheel_screen(
        &mut self,
        origin: (i32, i32),
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        self.mouse_move_screen(origin, x, y)?;
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let lines_y = (dy / 40).clamp(-30, 30);
            let lines_x = (dx / 40).clamp(-30, 30);
            let ev = CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                kCGScrollEventUnitLine,
                2,
                lines_y,
                lines_x,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        Ok(())
    }

    pub fn scroll_pixel_screen(
        &mut self,
        origin: (i32, i32),
        x: i32,
        y: i32,
        pixels_x: i32,
        pixels_y: i32,
    ) -> Result<()> {
        if pixels_x == 0 && pixels_y == 0 {
            return Ok(());
        }
        self.mouse_move_screen(origin, x, y)?;
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let ev = CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                kCGScrollEventUnitPixel,
                2,
                pixels_y,
                pixels_x,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        Ok(())
    }

    pub fn scroll_pixel(
        &mut self,
        hwnd: isize,
        x: i32,
        y: i32,
        pixels_x: i32,
        pixels_y: i32,
    ) -> Result<()> {
        if pixels_x == 0 && pixels_y == 0 {
            return Ok(());
        }
        self.mouse_move(hwnd, x, y)?;
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let ev = CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                kCGScrollEventUnitPixel,
                2,
                pixels_y,
                pixels_x,
            );
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        Ok(())
    }

    pub fn key(&mut self, scancode: u32, extended: bool, down: bool) -> Result<()> {
        let Some(vk) = ps2_scancode_to_mac_keycode(scancode, extended) else {
            return Ok(());
        };
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            let ev = CGEventCreateKeyboardEvent(std::ptr::null(), vk, down);
            if !ev.is_null() {
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
        let _ = vk;
        if down {
            self.keys_down.insert((scancode, extended));
        } else {
            self.keys_down.remove(&(scancode, extended));
        }
        Ok(())
    }

    pub fn text(&mut self, s: &str) -> Result<()> {
        let units: Vec<u16> = s.encode_utf16().collect();
        if units.is_empty() {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        unsafe {
            use crate::ffi::*;
            for &u in &units {
                let down = CGEventCreateKeyboardEvent(std::ptr::null(), 0, true);
                if !down.is_null() {
                    CGEventKeyboardSetUnicodeString(down, 1, &u);
                    CGEventPost(kCGHIDEventTap, down);
                    CFRelease(down);
                }
                let up = CGEventCreateKeyboardEvent(std::ptr::null(), 0, false);
                if !up.is_null() {
                    CGEventKeyboardSetUnicodeString(up, 1, &u);
                    CGEventPost(kCGHIDEventTap, up);
                    CFRelease(up);
                }
            }
        }
        Ok(())
    }

    pub fn release_all(&mut self) {
        let keys: Vec<(u32, bool)> = self.keys_down.drain().collect();
        for (sc, ext) in keys {
            let _ = self.key(sc, ext, false);
        }
        self.buttons_down.clear();
    }

    fn to_screen(&self, hwnd: isize, x: i32, y: i32) -> (f64, f64) {
        if let Some(rect) = crate::window::window_rect(hwnd) {
            ((rect.x + x) as f64, (rect.y + y) as f64)
        } else {
            (x as f64, y as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_ps2_scancodes_to_mac_keycodes() {
        assert_eq!(ps2_scancode_to_mac_keycode(0x1E, false), Some(0x00)); // A
        assert_eq!(ps2_scancode_to_mac_keycode(0x1C, false), Some(0x24)); // Return
        assert_eq!(ps2_scancode_to_mac_keycode(0x01, false), Some(0x35)); // Escape
        assert_eq!(ps2_scancode_to_mac_keycode(0x48, true), Some(0x7E)); // Up Arrow
        assert_eq!(ps2_scancode_to_mac_keycode(0x5B, true), Some(0x37)); // Left Command
    }
}
