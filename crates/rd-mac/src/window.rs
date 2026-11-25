//! macOS 窗口枚举、事件监听、窗口组与缩放适配（对齐 `rd-window-win` 契约）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver};

/// 矩形（物理/逻辑像素，字段与 `rd-window-win::Rect` 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 顶层窗口静态属性（字段与 `rd-window-win::WindowAttrs` 一致）。
#[derive(Debug, Clone)]
pub struct WindowAttrs {
    pub hwnd: isize,
    pub title: String,
    pub process: String,
    pub pid: u32,
    pub rect: Rect,
    pub frame: Rect,
    pub close_button: Option<Rect>,
    pub dpi: u32,
    pub resizable: bool,
    pub minimized: bool,
    pub style: u32,
    pub ex_style: u32,
    pub cloaked: bool,
    pub class: String,
}

/// 窗口组成员（与 `rd-window-win::GroupMember` 一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    pub hwnd: isize,
    pub offset_x: i32,
    pub offset_y: i32,
    pub w: u32,
    pub h: u32,
}

/// 枚举 macOS 当前可见顶层窗口（`CGWindowListCopyWindowInfo`）。
pub fn list_windows() -> Result<Vec<WindowAttrs>> {
    #[cfg(target_os = "macos")]
    {
        Ok(enumerate_mac_windows())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Vec::new())
    }
}

#[cfg(target_os = "macos")]
fn enumerate_mac_windows() -> Vec<WindowAttrs> {
    use crate::ffi::*;

    let mut out = Vec::new();
    let self_pid = std::process::id();
    unsafe {
        let arr = CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        );
        if arr.is_null() {
            return out;
        }
        let count = CFArrayGetCount(arr);
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(arr, i) as CFDictionaryRef;
            if dict.is_null() {
                continue;
            }
            let layer = dict_get_i64(dict, "kCGWindowLayer").unwrap_or(0);
            if layer != 0 {
                continue;
            }
            let pid = dict_get_i64(dict, "kCGWindowOwnerPID").unwrap_or(0) as u32;
            if pid == 0 || pid == self_pid {
                continue;
            }
            let wid = dict_get_i64(dict, "kCGWindowNumber").unwrap_or(0);
            if wid <= 0 {
                continue;
            }
            let Some(bounds) = dict_get_rect(dict, "kCGWindowBounds") else {
                continue;
            };
            let w = bounds.size.width.max(0.0) as u32;
            let h = bounds.size.height.max(0.0) as u32;
            if w < 64 || h < 64 {
                continue;
            }
            let process = dict_get_string(dict, "kCGWindowOwnerName").unwrap_or_default();
            if matches!(
                process.as_str(),
                "Window Server" | "Dock" | "Control Center" | "Notification Center"
            ) {
                continue;
            }
            let raw_title = dict_get_string(dict, "kCGWindowName").unwrap_or_default();
            let title = if raw_title.trim().is_empty() {
                process.clone()
            } else {
                raw_title
            };
            let rect = Rect {
                x: bounds.origin.x as i32,
                y: bounds.origin.y as i32,
                w,
                h,
            };
            // macOS 交通灯关闭按钮位于窗口左上角 (+14, +14) 附近 12×12
            let close_button = Some(Rect {
                x: rect.x + 8,
                y: rect.y + 8,
                w: 14,
                h: 14,
            });
            out.push(WindowAttrs {
                hwnd: wid as isize,
                title,
                process,
                pid,
                rect,
                frame: rect,
                close_button,
                dpi: 96,
                resizable: true,
                minimized: false,
                style: 0,
                ex_style: 0,
                cloaked: false,
                class: String::new(),
            });
        }
        CFRelease(arr);
    }
    out
}

/// 查询指定 `CGWindowID` 的屏幕矩形（供 `rd-mac::input` 客户区坐标换算使用）。
pub fn window_rect(window_id: isize) -> Option<Rect> {
    list_windows()
        .ok()?
        .into_iter()
        .find(|w| w.hwnd == window_id)
        .map(|w| w.rect)
}

/// 求窗口组：macOS 端返回根窗口自身（偏移 0,0）。
pub fn window_group(root: isize) -> Vec<GroupMember> {
    let Some(rect) = window_rect(root) else {
        return Vec::new();
    };
    vec![GroupMember {
        hwnd: root,
        offset_x: 0,
        offset_y: 0,
        w: rect.w,
        h: rect.h,
    }]
}

/// 窗口图标（macOS 暂返回 None，前端自动展示进程首字母徽标）。
pub fn window_icon_png(_hwnd: isize) -> Option<Vec<u8>> {
    None
}

pub mod events {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum WindowEventRaw {
        Created(isize),
        Destroyed(isize),
        Moved(isize),
        Title(isize),
        MenuStart(isize),
        MenuEnd(isize),
    }

    pub struct WatcherHandle {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for WatcherHandle {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// 轮询式窗口事件监听器（每 250ms 对比 `list_windows()` 快照发射增/删/移动/标题变更）。
    pub fn spawn_event_watcher() -> Result<(WatcherHandle, Receiver<WindowEventRaw>)> {
        let (tx, rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("rd-mac-win-events".into())
            .spawn(move || {
                let mut prev: HashMap<isize, (Rect, String)> = HashMap::new();
                if let Ok(list) = list_windows() {
                    for w in list {
                        prev.insert(w.hwnd, (w.rect, w.title));
                    }
                }
                while !stop_clone.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    let Ok(list) = list_windows() else { continue };
                    let mut cur: HashMap<isize, (Rect, String)> = HashMap::new();
                    for w in list {
                        cur.insert(w.hwnd, (w.rect, w.title));
                    }
                    for (&id, (rect, title)) in &cur {
                        match prev.get(&id) {
                            None => {
                                let _ = tx.send(WindowEventRaw::Created(id));
                            }
                            Some((old_rect, old_title)) => {
                                if old_rect != rect {
                                    let _ = tx.send(WindowEventRaw::Moved(id));
                                }
                                if old_title != title {
                                    let _ = tx.send(WindowEventRaw::Title(id));
                                }
                            }
                        }
                    }
                    for &id in prev.keys() {
                        if !cur.contains_key(&id) {
                            let _ = tx.send(WindowEventRaw::Destroyed(id));
                        }
                    }
                    prev = cur;
                }
            })?;
        Ok((
            WatcherHandle {
                stop,
                thread: Some(thread),
            },
            rx,
        ))
    }
}

pub mod resize {
    use super::*;

    pub const MIN_CLIENT_W: u32 = 200;
    pub const MIN_CLIENT_H: u32 = 150;

    pub fn target_client_size(css_w: u32, css_h: u32, dpr: f32) -> (u32, u32) {
        let scale = |css: u32, min: u32| {
            let px = (css as f32 * dpr).round();
            (px.max(0.0) as u32).clamp(min, i32::MAX as u32)
        };
        (scale(css_w, MIN_CLIENT_W), scale(css_h, MIN_CLIENT_H))
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum FitResult {
        Exact,
        Partial { actual_w: u32, actual_h: u32 },
        NotResizable,
    }

    #[derive(Debug, Clone)]
    pub struct SavedPlacement {
        pub rect: Rect,
    }

    impl SavedPlacement {
        pub fn show_cmd(&self) -> u32 {
            1
        }
        pub fn normal_rect_ltrb(&self) -> [i32; 4] {
            [
                self.rect.x,
                self.rect.y,
                self.rect.x + self.rect.w as i32,
                self.rect.y + self.rect.h as i32,
            ]
        }
    }

    pub fn save_placement(hwnd: isize) -> Result<SavedPlacement> {
        let rect = window_rect(hwnd).unwrap_or(Rect {
            x: 0,
            y: 0,
            w: 1280,
            h: 800,
        });
        Ok(SavedPlacement { rect })
    }

    pub fn fit_window(hwnd: isize, client_w: u32, client_h: u32) -> Result<FitResult> {
        let Some(cur) = window_rect(hwnd) else {
            return Ok(FitResult::NotResizable);
        };
        if cur.w == client_w && cur.h == client_h {
            return Ok(FitResult::Exact);
        }
        Ok(FitResult::Partial {
            actual_w: cur.w,
            actual_h: cur.h,
        })
    }

    pub fn restore_placement(_hwnd: isize, _p: &SavedPlacement) -> Result<()> {
        Ok(())
    }
}

pub mod ledger {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct OperationLedger {
        _path: PathBuf,
    }

    impl OperationLedger {
        pub fn open(dir: &Path) -> Self {
            Self {
                _path: dir.join("rd-window-ledger.json"),
            }
        }
        pub fn record(
            &self,
            _hwnd: isize,
            _reason: &str,
            _p: &super::resize::SavedPlacement,
        ) -> Result<()> {
            Ok(())
        }
        pub fn clear(&self, _hwnd: isize) -> Result<()> {
            Ok(())
        }
        pub fn recover(&self) -> Result<usize> {
            Ok(0)
        }
    }
}

pub mod quiet {
    pub const QUIET_WINDOW: f64 = 5.0;
    pub const NOTE_DEBOUNCE: f64 = 1.5;

    #[derive(Debug, Default, Clone)]
    pub struct QuietWindow {
        until: f64,
        last_note: f64,
    }

    impl QuietWindow {
        pub fn note(&mut self, now: f64) {
            if now - self.last_note < NOTE_DEBOUNCE {
                return;
            }
            self.last_note = now;
            self.until = self.until.max(now + QUIET_WINDOW);
        }

        pub fn is_quiet(&self, now: f64) -> bool {
            now < self.until
        }
    }
}
