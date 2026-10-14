//! 捕获一个窗口若干帧：保存第一帧为 PNG，并统计 GPU→CPU 读回耗时。
//!
//! 用法：
//! ```text
//! cargo run -p rd-capture-win --example capture_dump -- <窗口标题子串> [--frames N] [--out 路径]
//! ```
//!
//! 注意：WGC 只在窗口有更新时推帧，静止窗口（无输入、无动画）不会有帧到来；
//! 超时会提示先动一动目标窗口。

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rd_capture_win::{CaptureOptions, CapturedFrame, WindowCapture};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible};

fn main() -> Result<()> {
    let (title, frames, out) = parse_args()?;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()?;
    }

    if !rd_capture_win::is_supported() {
        bail!("Windows.Graphics.Capture 不受支持");
    }

    let hwnd =
        find_window_by_title(&title).context(format!("找不到标题含「{title}」的可见窗口"))?;
    println!("目标窗口 HWND: {:#x}", hwnd);

    let (_capture, rx) = WindowCapture::start(hwnd, CaptureOptions { max_fps: 60 })?;

    let mut readbacks: Vec<Duration> = Vec::new();
    let mut saved_path = String::new();
    let mut last_size = (0u32, 0u32);
    let mut resize_events = 0usize;
    let mut first_frame_at: Option<Instant> = None;

    while readbacks.len() < frames {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(frame) => {
                if first_frame_at.is_none() {
                    first_frame_at = Some(Instant::now());
                    saved_path = out.clone();
                    save_png(&frame, &saved_path)?;
                    println!(
                        "已保存第一帧: {saved_path} ({}x{}, 客户区)",
                        frame.width, frame.height
                    );
                }
                if (frame.width, frame.height) != last_size {
                    if last_size != (0, 0) {
                        resize_events += 1;
                        println!(
                            "尺寸变化: {}x{} -> {}x{}",
                            last_size.0, last_size.1, frame.width, frame.height
                        );
                    }
                    last_size = (frame.width, frame.height);
                }
                readbacks.push(frame.readback);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if readbacks.is_empty() {
                    bail!("10s 内未收到任何帧：窗口可能静止（WGC 仅在窗口更新时推帧），请移动/点击/输入目标窗口后重试");
                }
                println!("超过 10s 无新帧，提前结束（共 {} 帧）", readbacks.len());
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                bail!("捕获会话已关闭");
            }
        }
    }
    let _ = first_frame_at;
    drop(_capture);

    print_stats(&readbacks, last_size, resize_events, &saved_path);
    Ok(())
}

fn parse_args() -> Result<(String, usize, String)> {
    let mut args = std::env::args().skip(1);
    let mut title = String::new();
    let mut frames: usize = 60;
    let mut out = String::from("capture_dump.png");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => {
                frames = args
                    .next()
                    .context("--frames 缺少参数")?
                    .parse()
                    .context("--frames 需要正整数")?
            }
            "--out" => out = args.next().context("--out 缺少参数")?,
            other if title.is_empty() => title = other.to_string(),
            other => bail!("未知参数: {other}"),
        }
    }
    if title.is_empty() {
        bail!("用法: capture_dump <窗口标题子串> [--frames N] [--out 路径]");
    }
    Ok((title, frames, out))
}

fn print_stats(readbacks: &[Duration], size: (u32, u32), resize_events: usize, saved_path: &str) {
    if readbacks.is_empty() {
        println!("未捕获到任何帧");
        return;
    }
    let mut sorted: Vec<Duration> = readbacks.to_vec();
    sorted.sort();
    let n = sorted.len();
    let sum: Duration = sorted.iter().sum();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "读回耗时统计 n={n} min={:.2}ms avg={:.2}ms p95={:.2}ms max={:.2}ms (DoD 阈值 <8ms)",
        ms(sorted[0]),
        ms(sum / n as u32),
        ms(sorted[(n * 95 / 100).min(n - 1)]),
        ms(sorted[n - 1]),
    );
    println!(
        "最终尺寸: {}x{} 尺寸变化事件: {resize_events} PNG: {saved_path}",
        size.0, size.1
    );
}

fn save_png(frame: &CapturedFrame, path: &str) -> Result<()> {
    let mut rgba = frame.data.clone();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2); // BGRA -> RGBA
    }
    let img = image::RgbaImage::from_raw(frame.width, frame.height, rgba)
        .context("帧数据尺寸与 (width, height) 不匹配")?;
    img.save(path).with_context(|| format!("写入 {path} 失败"))
}

/// 枚举可见顶层窗口，返回标题包含子串的第一个 HWND。
fn find_window_by_title(sub: &str) -> Option<isize> {
    struct Ctx<'a> {
        sub: &'a str,
        found: Option<isize>,
    }
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        if ctx.found.is_some() {
            return BOOL(0);
        }
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }
        let mut buf = [0u16; 512];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
        let title = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
        if title.contains(ctx.sub) {
            ctx.found = Some(hwnd.0 as isize);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut ctx = Ctx { sub, found: None };
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.found
}
