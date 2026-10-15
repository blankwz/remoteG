//! 真机 GUI 验证（默认忽略）：窗口 resize 时 `CapturedFrame.resized` 置位。
//!
//! 依赖 `clarity-bench` 可执行文件与本机桌面会话。运行：
//! ```text
//! cargo build -p clarity-bench
//! cargo test -p rd-capture-win --test resize_signal -- --ignored --nocapture
//! ```
//!
//! 流程：启动 testwin（clock 模式持续刷新）→ WGC 捕获 → PowerShell
//! `SetWindowPos` 放大到 900x600 → 断言收到 `resized=true` 且随后出现新尺寸帧
//! → 缩回 640x480 → 断言再次置位。全程按子进程 PID 清理。
//!
//! 注：`SetWindowPos` 的 900x600 为窗口外部尺寸，捕获帧为客户区尺寸（约
//! 884x561）；且放大后第一帧（resized=true）可能仍是被旧帧池纹理裁剪的过渡
//! 帧，故断言「信号置位」与「新尺寸帧出现」分别成立，不要求同一帧。

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rd_capture_win::{CaptureOptions, WindowCapture};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible};

const BENCH_EXE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/debug/clarity-bench.exe"
);
const BASELINE_W: u32 = 640;

#[test]
#[ignore = "需要 GUI 与 clarity-bench 可执行文件（先 cargo build -p clarity-bench）"]
fn resize_signal_fires_on_window_resize() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let guard = BenchGuard::spawn()?;

    let hwnd = guard.wait_window(Duration::from_secs(10))?;
    println!("目标窗口 HWND: {hwnd:#x}");

    let (_capture, rx) = WindowCapture::start(hwnd, CaptureOptions { max_fps: 60 })?;

    // 等待基线帧（clock 模式持续刷新，必出帧）。
    let first = recv_frame(&rx, Duration::from_secs(10))?;
    println!(
        "基线帧: {}x{} resized={}",
        first.width, first.height, first.resized
    );
    assert!(
        !first.resized,
        "首帧不应置位 resized（got {}x{})",
        first.width, first.height
    );

    // 放大到 900x600（外部尺寸）。
    guard.resize(900, 600)?;
    let (resized_seen, enlarged) = watch_until(
        &rx,
        Duration::from_secs(8),
        |f| f.resized,
        |f| f.width > BASELINE_W,
    )?;
    assert!(resized_seen, "放大后未收到 resized=true");
    assert!(enlarged, "放大后未收到宽度 > {BASELINE_W} 的新尺寸帧");
    println!("放大 900x600: resized=true ✓，新尺寸帧 ✓");

    // 缩回 640x480：再次置位。
    guard.resize(640, 480)?;
    let (resized_seen, shrunk) = watch_until(
        &rx,
        Duration::from_secs(8),
        |f| f.resized,
        |f| f.width <= BASELINE_W,
    )?;
    assert!(resized_seen, "缩回后未收到 resized=true");
    assert!(shrunk, "缩回后未收到宽度 <= {BASELINE_W} 的新尺寸帧");
    println!("缩回 640x480: resized=true ✓，新尺寸帧 ✓");

    drop(_capture);
    Ok(())
}

/// 消费帧直到两个谓词各自满足（或超时）。返回 (resized 谓词结果, 尺寸谓词结果)。
fn watch_until(
    rx: &crossbeam_channel::Receiver<rd_capture_win::CapturedFrame>,
    timeout: Duration,
    mut pred_a: impl FnMut(&rd_capture_win::CapturedFrame) -> bool,
    mut pred_b: impl FnMut(&rd_capture_win::CapturedFrame) -> bool,
) -> Result<(bool, bool)> {
    let (mut got_a, mut got_b) = (false, false);
    let deadline = Instant::now() + timeout;
    while !(got_a && got_b) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let frame = match rx.recv_timeout(left.min(Duration::from_millis(500))) {
            Ok(f) => f,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                bail!("捕获会话已关闭")
            }
        };
        println!(
            "  帧: {}x{} resized={}",
            frame.width, frame.height, frame.resized
        );
        got_a |= pred_a(&frame);
        got_b |= pred_b(&frame);
    }
    Ok((got_a, got_b))
}

fn recv_frame(
    rx: &crossbeam_channel::Receiver<rd_capture_win::CapturedFrame>,
    timeout: Duration,
) -> Result<rd_capture_win::CapturedFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("等待基线帧超时（窗口可能静止或启动失败）");
        }
        match rx.recv_timeout(left.min(Duration::from_millis(500))) {
            Ok(f) => return Ok(f),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                bail!("捕获会话已关闭")
            }
        }
    }
}

/// clarity-bench 子进程守护：Drop 时按 PID 终止（不波及其他进程）。
struct BenchGuard(Child);

impl BenchGuard {
    fn spawn() -> Result<Self> {
        if !std::path::Path::new(BENCH_EXE).exists() {
            bail!("未找到 {BENCH_EXE}，请先 cargo build -p clarity-bench");
        }
        let child = Command::new(BENCH_EXE)
            .args(["testwin", "--mode", "clock", "--size", "640x480"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("启动 clarity-bench 失败")?;
        println!("clarity-bench PID: {}", child.id());
        Ok(Self(child))
    }

    fn wait_window(&self, timeout: Duration) -> Result<isize> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(hwnd) = find_window_by_title("clarity-bench") {
                return Ok(hwnd);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        bail!("10s 内未找到 testwin 窗口")
    }

    /// SetWindowPos 调整 testwin 尺寸（外部尺寸，物理像素）。
    fn resize(&self, width: i32, height: i32) -> Result<()> {
        let pid = self.0.id();
        let script = format!(
            "Add-Type -TypeDefinition 'using System;using System.Runtime.InteropServices;\
             public class W{{[DllImport(\"user32.dll\")]\
             public static extern bool SetWindowPos(IntPtr h,IntPtr a,int x,int y,int cx,int cy,uint f);}}';\
             $p=Get-Process -Id {pid};\
             if(-not $p.MainWindowHandle){{throw 'MainWindowHandle 为空'}};\
             [W]::SetWindowPos($p.MainWindowHandle,[IntPtr]::Zero,100,100,{width},{height},0x14)"
        );
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .output()
            .context("调用 PowerShell 失败")?;
        if !out.status.success() {
            bail!("resize 失败: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    }
}

impl Drop for BenchGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
