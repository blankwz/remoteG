//! macOS 按窗口捕获（CoreGraphics `CGWindowListCreateImage`，BGRA8888，最高 144fps）。
//!
//! 公共类型与 `rd-capture-win` 完全一致，方便 `rd-agent` 无缝跨平台复用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{bounded, Receiver};

/// 脏区矩形（与 `rd-capture-win::Rect` 逐字段一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 捕获配置。
pub struct CaptureOptions {
    pub max_fps: u32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self { max_fps: 144 }
    }
}

/// 一帧窗口 BGRA8 像素（与 `rd-capture-win::CapturedFrame` 逐字段一致）。
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    pub dirty: Option<Vec<Rect>>,
    pub resized: bool,
    pub captured_at: Instant,
    pub readback: Duration,
}

/// 已连接的显示器信息（与 `rd-capture-win::MonitorInfo` 逐字段一致）。
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: u32,
    pub hmonitor: isize,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// 枚举当前活动显示器（macOS 主显示器；坐标与 `CGEvent` 逻辑点 1:1 对齐）。
pub fn list_monitors() -> Vec<MonitorInfo> {
    #[cfg(target_os = "macos")]
    {
        use crate::ffi::{CGDisplayBounds, CGMainDisplayID};
        unsafe {
            let id = CGMainDisplayID();
            let b = CGDisplayBounds(id);
            let w = b.size.width.max(1.0) as u32;
            let h = b.size.height.max(1.0) as u32;
            vec![MonitorInfo {
                index: 0,
                hmonitor: id as isize,
                name: format!("完整桌面（主显示器 {w}×{h}）"),
                x: b.origin.x as i32,
                y: b.origin.y as i32,
                width: w,
                height: h,
                primary: true,
            }]
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// 捕获是否受支持（macOS 上检测屏幕录制权限）。
pub fn is_supported() -> bool {
    crate::permissions::has_screen_recording()
}

/// 窗口捕获会话；`stop()` 或 Drop 时优雅停止捕获线程。
pub struct WindowCapture {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WindowCapture {
    /// 启动指定显示器的完整桌面捕获线程。
    pub fn start_monitor(
        display_id: isize,
        opts: CaptureOptions,
    ) -> Result<(Self, Receiver<CapturedFrame>)> {
        let (tx, rx) = bounded::<CapturedFrame>(2);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_flag);
        let fps = opts.max_fps.clamp(1, 144);
        let interval = Duration::from_secs_f64(1.0 / fps as f64);

        let thread = std::thread::Builder::new()
            .name(format!("rd-mac-mon-{display_id}"))
            .spawn(move || {
                #[cfg(target_os = "macos")]
                capture_loop(None, Some(display_id as u32), interval, &stop_clone, &tx);
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (display_id, interval, stop_clone, tx);
                }
            })?;

        Ok((
            Self {
                stop_flag,
                thread: Some(thread),
            },
            rx,
        ))
    }

    /// 启动指定 `CGWindowID`（以 `isize` 透传）的窗口捕获线程。
    pub fn start(
        window_id: isize,
        opts: CaptureOptions,
    ) -> Result<(Self, Receiver<CapturedFrame>)> {
        let (tx, rx) = bounded::<CapturedFrame>(2);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_flag);
        let fps = opts.max_fps.clamp(1, 144);
        let interval = Duration::from_secs_f64(1.0 / fps as f64);

        let thread = std::thread::Builder::new()
            .name(format!("rd-mac-cap-{window_id}"))
            .spawn(move || {
                #[cfg(target_os = "macos")]
                capture_loop(Some(window_id as u32), None, interval, &stop_clone, &tx);
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (window_id, interval, stop_clone, tx);
                }
            })?;

        Ok((
            Self {
                stop_flag,
                thread: Some(thread),
            },
            rx,
        ))
    }

    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn capture_loop(
    window_id: Option<u32>,
    display_id: Option<u32>,
    interval: Duration,
    stop: &AtomicBool,
    tx: &crossbeam_channel::Sender<CapturedFrame>,
) {
    use crate::ffi::*;

    let mut last_size: Option<(u32, u32)> = None;
    let mut pending_resized = false;

    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let frame_opt = unsafe {
            let img = if let Some(wid) = window_id {
                CGWindowListCreateImage(
                    CGRectNull,
                    kCGWindowListOptionIncludingWindow,
                    wid,
                    kCGWindowImageBoundsIgnoreFraming | kCGWindowImageNominalResolution,
                )
            } else {
                let did = display_id.unwrap_or_else(|| CGMainDisplayID());
                let bounds = CGDisplayBounds(did);
                CGWindowListCreateImage(
                    bounds,
                    kCGWindowListOptionOnScreenOnly,
                    kCGNullWindowID,
                    kCGWindowImageNominalResolution,
                )
            };
            if img.is_null() {
                None
            } else {
                let w = CGImageGetWidth(img) as u32;
                let h = CGImageGetHeight(img) as u32;
                let bpr = CGImageGetBytesPerRow(img);
                let bpp = CGImageGetBitsPerPixel(img);
                let provider = CGImageGetDataProvider(img);
                let data_ref = if !provider.is_null() {
                    CGDataProviderCopyData(provider)
                } else {
                    std::ptr::null()
                };

                let out = if !data_ref.is_null() && w > 0 && h > 0 && bpp == 32 {
                    let len = CFDataGetLength(data_ref) as usize;
                    let ptr = CFDataGetBytePtr(data_ref);
                    let row_bytes = (w as usize) * 4;
                    let mut buf = vec![0u8; row_bytes * (h as usize)];
                    if !ptr.is_null() && bpr >= row_bytes {
                        let slice = std::slice::from_raw_parts(ptr, len);
                        for y in 0..(h as usize) {
                            let src_off = y * bpr;
                            if src_off + row_bytes <= slice.len() {
                                let dst_off = y * row_bytes;
                                buf[dst_off..dst_off + row_bytes]
                                    .copy_from_slice(&slice[src_off..src_off + row_bytes]);
                            }
                        }
                        let resized = last_size.is_some_and(|s| s != (w, h));
                        last_size = Some((w, h));
                        let deliver_resized = resized || pending_resized;
                        Some(CapturedFrame {
                            width: w,
                            height: h,
                            stride: row_bytes as u32,
                            data: buf,
                            dirty: None,
                            resized: deliver_resized,
                            captured_at: Instant::now(),
                            readback: t0.elapsed(),
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };

                if !data_ref.is_null() {
                    CFRelease(data_ref);
                }
                CGImageRelease(img);
                out
            }
        };

        if let Some(frame) = frame_opt {
            let was_resized = frame.resized;
            match tx.try_send(frame) {
                Ok(()) => pending_resized = false,
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    pending_resized |= was_resized;
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
            }
        }

        let elapsed = t0.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}
