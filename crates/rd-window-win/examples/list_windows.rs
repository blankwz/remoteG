//! 打印当前可捕获窗口列表，随后监听 5 秒窗口事件（T-008 手测入口）。
//!
//! 运行：`cargo run -p rd-window-win --example list_windows`
//! 运行期间开关窗口 / 移动窗口 / 弹出右键菜单，观察事件输出。

use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;

use rd_window_win::{list_windows, spawn_event_watcher};

const EVENT_WINDOW: Duration = Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    let windows = list_windows()?;
    println!("可捕获窗口 {} 个（Z 序）：", windows.len());
    for w in &windows {
        println!(
            "hwnd={:>8}  pid={:>6}  {:<22} {:>3}dpi  {:>5}x{:<5} @({:>5},{:>5})  {}{}",
            w.hwnd,
            w.pid,
            truncate(&w.process, 22),
            w.dpi,
            w.rect.w,
            w.rect.h,
            w.rect.x,
            w.rect.y,
            truncate(&w.title, 48),
            if w.minimized { "  [最小化]" } else { "" },
        );
    }

    // 图标抽查：给前 3 个窗口取图标。
    for w in windows.iter().take(3) {
        match rd_window_win::window_icon_png(w.hwnd) {
            Some(png) => {
                println!(
                    "icon: \"{}\" → PNG {} 字节",
                    truncate(&w.title, 24),
                    png.len()
                )
            }
            None => println!("icon: \"{}\" → 无", truncate(&w.title, 24)),
        }
    }

    let (handle, rx) = spawn_event_watcher()?;
    println!("\n监听窗口事件 {EVENT_WINDOW:?}（试着开关/移动窗口、弹出右键菜单）…");
    let deadline = Instant::now() + EVENT_WINDOW;
    let mut count = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(ev) => {
                count += 1;
                println!("{count:>2}. {ev:?}");
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                println!("事件通道已关闭");
                break;
            }
        }
    }
    println!("共收到 {count} 个事件");
    drop(handle); // 停止监听线程
    Ok(())
}

/// 截断过长的显示字符串。
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}
