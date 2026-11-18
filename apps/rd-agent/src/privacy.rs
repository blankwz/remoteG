//! 隐私屏（T-028 F9，Pro 功能）：被控机屏幕被一层置顶纯黑窗盖住，本机看不到
//! 操作过程，控制端画面照常——WGC 窗口捕获的是目标窗口自身的合成内容，与
//! 屏幕遮挡无关，因此黑罩不影响远控画面。
//!
//! 为什么选黑罩而不是 DWM Cloak / 移窗出屏：
//! - Cloak 后 WGC 是否持续出帧无契约保证（挂起 UWP 同为 cloak，赌不得）；
//! - 移窗会与 Fit/还原/布局快照逻辑打架；
//! - 黑罩不动目标窗口任何状态，断开即撤，风险最小。
//!
//! 知情合规：开启瞬间由 session 层弹系统 MessageBox（家人点确定后才进入
//! 黑屏），黑屏期间托盘图标仍是「正在被控制」绿色态。
//!
//! 线程模型：HWND 有线程亲和性——黑罩窗口在常驻线程上创建并常驻（隐藏 ≠
//! 销毁），切换只 ShowWindow；[`set`] 经通道投递请求后立即返回。

#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::mpsc::Sender;
#[cfg(windows)]
use std::sync::OnceLock;

/// 黑罩线程的请求通道；进程内一次。
#[cfg(windows)]
static SHIELD_TX: OnceLock<Sender<Req>> = OnceLock::new();
/// 状态文件目录（init 时登记；E3 状态落盘用）。
#[cfg(windows)]
static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 设置隐私屏（异步生效；与当前状态相同则为无操作）。返回是否已受理
/// （`false` = 线程未起或已退出，调用方据此回 Error）。
#[cfg(windows)]
pub fn set(on: bool) -> bool {
    let Some(tx) = SHIELD_TX.get() else {
        return false;
    };
    // E2 自扰静默：黑窗创建/销毁会触发窗口事件风暴，登记静默窗（取 max
    // 延后），静默期内桥接线程不向控制端推窗口事件
    crate::window_session::note_self_disturbance();
    let accepted = tx.send(if on { Req::On } else { Req::Off }).is_ok();
    // E3 状态落盘：重启回看恢复用（受理即记，与实际生效异步一致由线程保证）
    if accepted {
        if let Some(dir) = STATE_DIR.get() {
            persist_state(dir, on);
        }
    }
    accepted
}

/// 非 Windows：不可用（保持可 check 可运行）。
#[cfg(not(windows))]
pub fn set(_on: bool) -> bool {
    false
}

/// 隐私屏状态文件（E3 重启回看恢复）：`<config_dir>/rd-privacy-state.json`。
/// 崩溃重启后回看恢复（开着 → 重新遮蔽；保护跨重启，fail-safe 方向）。
const STATE_ON: &str = "{\"on\":true}";
const STATE_OFF: &str = "{\"on\":false}";

/// 状态落盘（原子写：临时文件 + rename）。
fn persist_state(config_dir: &std::path::Path, on: bool) {
    let path = config_dir.join("rd-privacy-state.json");
    let tmp = path.with_extension("json.tmp");
    let body = if on { STATE_ON } else { STATE_OFF };
    if let Err(err) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &path)) {
        tracing::warn!("隐私屏状态落盘失败（重启后不恢复）: {err}");
    }
}

/// 回看恢复：读状态文件（无文件 = 从未开启过，不恢复）。
fn read_state(config_dir: &std::path::Path) -> Option<bool> {
    let bytes = std::fs::read(config_dir.join("rd-privacy-state.json")).ok()?;
    if bytes == STATE_ON.as_bytes() {
        Some(true)
    } else if bytes == STATE_OFF.as_bytes() {
        Some(false)
    } else {
        None
    }
}

/// 启动回看恢复：重启前隐私屏开着 → 重新遮蔽（E3；恢复失败只记日志）。
#[cfg(windows)]
pub fn recover_state(config_dir: &std::path::Path) {
    if read_state(config_dir) == Some(true) {
        tracing::info!("重启回看：隐私屏在上次运行时开启，恢复遮蔽");
        if !set(true) {
            tracing::warn!("隐私屏恢复投递失败（黑罩线程未就绪）");
        }
    }
}

/// 启动回看恢复（非 Windows no-op）。
#[cfg(not(windows))]
pub fn recover_state(_config_dir: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// E3 状态文件：写读回看 roundtrip；未知内容返回 None（不猜）。
    #[test]
    fn state_file_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(read_state(dir.path()), None, "无文件 = 从未开启，不恢复");
        persist_state(dir.path(), true);
        assert_eq!(read_state(dir.path()), Some(true));
        persist_state(dir.path(), false);
        assert_eq!(read_state(dir.path()), Some(false));
        // 损坏内容：None（不猜，宁可不复原也不误判）
        std::fs::write(dir.path().join("rd-privacy-state.json"), b"garbage").unwrap();
        assert_eq!(read_state(dir.path()), None);
    }
}

/// 启动黑罩线程（进程内一次；lib `start` 时调用）。
#[cfg(windows)]
pub fn init(config_dir: &Path) {
    let _ = STATE_DIR.set(config_dir.to_path_buf());
    let (tx, rx) = std::sync::mpsc::channel::<Req>();
    if SHIELD_TX.set(tx).is_err() {
        return; // 已初始化
    }
    std::thread::Builder::new()
        .name("rd-privacy-shield".into())
        .spawn(move || shield_thread(rx))
        .expect("启动隐私屏线程失败");
}

/// 非 Windows：no-op。
#[cfg(not(windows))]
pub fn init() {}

/// 隐私屏请求（投递给黑罩线程）。
#[cfg(windows)]
#[derive(Debug)]
enum Req {
    /// 显示黑罩（幂等：已显示则无操作）。
    On,
    /// 隐藏黑罩（幂等：未显示则无操作）。
    Off,
}

/// 常驻黑罩线程：注册类 → 创建全屏纯黑窗（初始隐藏）→ 请求 + 消息双泵。
#[cfg(windows)]
fn shield_thread(rx: std::sync::mpsc::Receiver<Req>) {
    use windows::core::w;
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetSystemMetrics, PeekMessageW, RegisterClassW,
        SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, HMENU, HWND_TOPMOST,
        MSG, PM_REMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN, SW_HIDE, SW_SHOW, SYSTEM_METRICS_INDEX, WINDOW_EX_STYLE, WNDCLASSW,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    let hinstance = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let class_name = w!("rd-privacy-shield");
    // 背景黑刷交给 DefWindowProc：WM_PAINT/ERASEBKGND 零自绘
    let background = unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x000000)) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(def_wndproc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        hbrBackground: background,
        style: CS_HREDRAW | CS_VREDRAW,
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };

    // 覆盖整个虚拟桌面（多显示器一起盖住；显示器热插拔不做——远控时长内可接受）
    let m = |i: SYSTEM_METRICS_INDEX| unsafe { GetSystemMetrics(i) };
    let (vx, vy, vw, vh) = (
        m(SM_XVIRTUALSCREEN),
        m(SM_YVIRTUALSCREEN),
        m(SM_CXVIRTUALSCREEN),
        m(SM_CYVIRTUALSCREEN),
    );
    // windows 0.62 的 CreateWindowExW 返回 Result<HWND>。
    // parent 必须是桌面顶层（None）：HWND_MESSAGE 是 message-only 窗口，
    // 带 WS_POPUP 创建失败、即便成功也永远不可见——黑罩无从谈起；
    // 「不进任务栏/Alt-Tab」由 WS_EX_TOOLWINDOW 承担。
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(
                WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0 | WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0,
            ),
            class_name,
            w!("rd 隐私屏"),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            None,
            Some(HMENU::default()),
            Some(hinstance.into()),
            None,
        )
    } {
        Ok(h) if !h.is_invalid() => h,
        Err(err) => {
            tracing::error!("隐私屏黑窗创建失败({err})，privacy::set 将无效果");
            while rx.recv().is_ok() {} // 排空：send 方的受理返回只是乐观值
            return;
        }
        Ok(_) => unreachable!("Ok 但句柄无效"),
    };

    let mut shown = false;
    loop {
        // 带超时收请求；空转期持续泵消息（遮挡恢复触发 WM_PAINT 必须处理，
        // 否则黑罩被全屏应用顶掉后不重绘）
        let req = match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(r) => Some(r),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        match req {
            Some(Req::On) if !shown => {
                unsafe {
                    // 重新置顶并覆盖（期间可能有人动过窗口）
                    let _ =
                        SetWindowPos(hwnd, Some(HWND_TOPMOST), vx, vy, vw, vh, Default::default());
                    let _ = ShowWindow(hwnd, SW_SHOW);
                }
                shown = true;
                tracing::info!("隐私屏已开启(被控机屏幕已遮蔽)");
            }
            Some(Req::Off) if shown => {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                shown = false;
                tracing::info!("隐私屏已关闭");
            }
            Some(_) => {} // 幂等：重复 On/Off 无操作
            None => {}
        }
        // 非阻塞排空本线程消息
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// 黑罩窗口过程：全部交给 DefWindowProc（黑背景由类刷完成）。
#[cfg(windows)]
unsafe extern "system" fn def_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 开启提示（session 层调用）：独立线程弹一次知情提示（家人点确定后黑屏）。
pub fn notify_user(peer_name: &str) {
    #[cfg(windows)]
    {
        let text = format!(
            "设备「{peer_name}」已开启隐私保护：\n本机屏幕即将黑屏，远程操作继续。\n\n(点「确定」继续；结束远控后屏幕自动恢复)"
        );
        std::thread::spawn(move || {
            use windows::core::HSTRING;
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxW, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
            };
            let text = HSTRING::from(text);
            let title = HSTRING::from("rd 远程控制 · 隐私保护");
            unsafe {
                MessageBoxW(None, &text, &title, MB_OK | MB_SETFOREGROUND | MB_TOPMOST);
            }
        });
    }
    #[cfg(not(windows))]
    let _ = peer_name;
}
