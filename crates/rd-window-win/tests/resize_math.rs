//! T-009：`target_client_size` 纯函数单测；`#[ignore]` 标记的 GUI 手测。
//!
//! GUI 手测运行方式：`cargo test -p rd-window-win -- --ignored`
//! （启动真实记事本/计算器，结束后按 PID 或窗口消息清理，不做全局强杀）。

use std::ffi::c_void;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use rd_window_win::resize::{
    fit_window, restore_placement, save_placement, target_client_size, FitResult,
};
use rd_window_win::{list_windows, WindowAttrs};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowLongPtrW, PostMessageW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE,
    SW_SHOWMAXIMIZED, WM_CLOSE, WS_THICKFRAME,
};

// ---------- target_client_size 纯函数 ----------

#[test]
fn dpr_1_0_一比一映射() {
    assert_eq!(target_client_size(1200, 800, 1.0), (1200, 800));
}

#[test]
fn dpr_1_5_半像素远离零取整() {
    assert_eq!(target_client_size(800, 500, 1.5), (1200, 750));
    // 333×1.5 = 499.5 → 500；201×1.5 = 301.5 → 302（round half away from zero）。
    assert_eq!(target_client_size(333, 201, 1.5), (500, 302));
}

#[test]
fn dpr_2_0_整数倍() {
    assert_eq!(target_client_size(600, 400, 2.0), (1200, 800));
}

#[test]
fn dpr_2_25_四舍五入() {
    assert_eq!(target_client_size(400, 300, 2.25), (900, 675));
    // 333×2.25 = 749.25 → 749；223×2.25 = 501.75 → 502。
    assert_eq!(target_client_size(333, 223, 2.25), (749, 502));
}

#[test]
fn 低于下限取_200x150() {
    assert_eq!(target_client_size(1, 1, 1.0), (200, 150));
    // 300×0.5 = 150、200×0.5 = 100，均低于下限。
    assert_eq!(target_client_size(300, 200, 0.5), (200, 150));
    // 恰在下限。
    assert_eq!(target_client_size(200, 150, 1.0), (200, 150));
}

#[test]
fn 异常_dpr_兜底到下限() {
    assert_eq!(target_client_size(1000, 800, 0.0), (200, 150));
    assert_eq!(target_client_size(1000, 800, -2.0), (200, 150));
    assert_eq!(target_client_size(1000, 800, f32::NAN), (200, 150));
}

#[test]
fn 超大结果饱和为_i32_上限() {
    // f32 → u32 饱和转换 + clamp(i32::MAX)，防止后续 as i32 反号。
    let max = i32::MAX as u32;
    assert_eq!(target_client_size(u32::MAX, u32::MAX, 1.0), (max, max));
}

// ---------- GUI 手测（#[ignore]） ----------

/// 轮询查找满足条件的可见顶层窗口，超时返回 `None`。
fn wait_window(pred: impl Fn(&WindowAttrs) -> bool, timeout: Duration) -> Option<isize> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let wins = list_windows().unwrap_or_default();
        if let Some(w) = wins.iter().find(|w| pred(w)) {
            return Some(w.hwnd);
        }
        thread::sleep(Duration::from_millis(200));
    }
    None
}

/// 客户区尺寸（物理像素）。
fn client_size(hwnd: isize) -> (u32, u32) {
    let h = HWND(hwnd as *mut c_void);
    let mut rc = RECT::default();
    unsafe { GetClientRect(h, &mut rc).expect("GetClientRect") };
    (
        (rc.right - rc.left).max(0) as u32,
        (rc.bottom - rc.top).max(0) as u32,
    )
}

/// 按 PID 清理自启进程（不全局强杀）。
fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "GUI 手测：启动记事本，fit 1200×800 应 Exact，恢复后与原状态一致"]
fn 手测_记事本_fit_精确与恢复() {
    let mut child = Command::new("notepad.exe").spawn().expect("启动记事本失败");
    let hwnd = wait_window(
        |w| w.pid == child.id() && w.class == "Notepad",
        Duration::from_secs(10),
    )
    .expect("10 秒内未找到记事本窗口");

    let saved = save_placement(hwnd).expect("save_placement");
    let size0 = client_size(hwnd);

    let r = fit_window(hwnd, 1200, 800).expect("fit_window");
    assert_eq!(r, FitResult::Exact, "记事本应可精确 1:1 适配");
    assert_eq!(
        client_size(hwnd),
        (1200, 800),
        "GetClientRect 应精确等于目标"
    );
    println!(
        "记事本: 初始客户区 {size0:?} → fit 1200×800 → {r:?}，实测客户区 {:?}",
        client_size(hwnd)
    );

    // 幂等：重复 fit 不漂移。
    assert_eq!(
        fit_window(hwnd, 1200, 800).expect("再次 fit"),
        FitResult::Exact
    );
    assert_eq!(client_size(hwnd), (1200, 800));

    restore_placement(hwnd, &saved).expect("restore_placement");
    let after = save_placement(hwnd).expect("恢复后回读 placement");
    assert_eq!(
        after.normal_position(),
        saved.normal_position(),
        "恢复后普通态矩形应一致"
    );
    assert_eq!(after.show_cmd(), saved.show_cmd(), "恢复后显示状态应一致");
    assert_eq!(client_size(hwnd), size0, "恢复后客户区应与初始一致");

    kill_child(&mut child);
}

#[test]
#[ignore = "GUI 手测：最大化状态 save → 打乱 → restore 后仍最大化"]
fn 手测_最大化状态保存与恢复() {
    let mut child = Command::new("notepad.exe").spawn().expect("启动记事本失败");
    let hwnd = wait_window(
        |w| w.pid == child.id() && w.class == "Notepad",
        Duration::from_secs(10),
    )
    .expect("10 秒内未找到记事本窗口");
    let h = HWND(hwnd as *mut c_void);

    unsafe {
        let _ = ShowWindow(h, SW_MAXIMIZE);
    };
    thread::sleep(Duration::from_millis(400));

    let saved = save_placement(hwnd).expect("save_placement");
    assert_eq!(
        saved.show_cmd(),
        SW_SHOWMAXIMIZED.0 as u32,
        "保存时应为最大化"
    );

    // 打乱状态：fit 到普通尺寸（结果 Exact/Partial 皆可，只要脱离最大化）。
    let _ = fit_window(hwnd, 900, 500);

    restore_placement(hwnd, &saved).expect("restore_placement");
    thread::sleep(Duration::from_millis(400));
    let after = save_placement(hwnd).expect("恢复后回读 placement");
    assert_eq!(
        after.show_cmd(),
        SW_SHOWMAXIMIZED.0 as u32,
        "restore 后应仍最大化"
    );
    assert_eq!(after, saved, "restore 后整个 placement 应与保存值一致");
    println!(
        "最大化: save showCmd={} → fit 900×500 打乱 → restore showCmd={}",
        saved.show_cmd(),
        after.show_cmd()
    );

    kill_child(&mut child);
}

#[test]
#[ignore = "GUI 手测：Win10 计算器有最小尺寸，fit 小目标应返回 Partial 并可恢复"]
fn 手测_计算器受限返回_partial() {
    // UWP 计算器无独立 exe（calc.exe 只是转发 stub，拿不到进程），经 URI 启动，
    // 结束时对窗口发 WM_CLOSE 收尾。
    let status = Command::new("cmd")
        .args(["/C", "start", "", "calculator://"])
        .status()
        .expect("启动计算器失败");
    assert!(status.success());

    let hwnd = wait_window(
        |w| {
            w.class == "ApplicationFrameWindow"
                && (w.title.contains("计算") || w.title.contains("Calc"))
        },
        Duration::from_secs(20),
    )
    .expect("20 秒内未找到计算器窗口");

    let saved = save_placement(hwnd).expect("save_placement");
    let r = fit_window(hwnd, 400, 300).expect("fit_window");
    let FitResult::Partial { actual_w, actual_h } = r else {
        panic!("期望 Partial（计算器有最小尺寸限制），实际 {r:?}");
    };
    assert!(
        actual_w > 400 || actual_h > 300,
        "受限后的实际尺寸应大于目标（actual {actual_w}×{actual_h}）"
    );
    println!(
        "计算器: fit 400×300 → Partial(actual {actual_w}×{actual_h})，恢复后 normal_position 一致"
    );

    restore_placement(hwnd, &saved).expect("restore_placement");
    thread::sleep(Duration::from_millis(300));
    let after = save_placement(hwnd).expect("恢复后回读 placement");
    assert_eq!(
        after.normal_position(),
        saved.normal_position(),
        "恢复后普通态矩形应一致"
    );

    // 按窗口关闭，不全局杀进程。
    let h = HWND(hwnd as *mut c_void);
    unsafe {
        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

#[test]
#[ignore = "GUI 手测：移除 WS_THICKFRAME 后 fit 应返回 NotResizable"]
fn 手测_无_thickframe_返回_not_resizable() {
    let mut child = Command::new("notepad.exe").spawn().expect("启动记事本失败");
    let hwnd = wait_window(
        |w| w.pid == child.id() && w.class == "Notepad",
        Duration::from_secs(10),
    )
    .expect("10 秒内未找到记事本窗口");
    let h = HWND(hwnd as *mut c_void);
    let frame_flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED;

    unsafe {
        // 移除 WS_THICKFRAME 使窗口不可调，fit 应直接返回 NotResizable。
        let style0 = GetWindowLongPtrW(h, GWL_STYLE);
        SetWindowLongPtrW(h, GWL_STYLE, style0 & !(WS_THICKFRAME.0 as isize));
        let _ = SetWindowPos(h, None, 0, 0, 0, 0, frame_flags);
        let r = fit_window(hwnd, 1200, 800).expect("fit_window");
        assert_eq!(
            r,
            FitResult::NotResizable,
            "无 WS_THICKFRAME 应返回 NotResizable"
        );
        println!("移除 WS_THICKFRAME 后 fit 1200×800 → {r:?}");

        // 恢复样式。
        SetWindowLongPtrW(
            h,
            GWL_STYLE,
            GetWindowLongPtrW(h, GWL_STYLE) | WS_THICKFRAME.0 as isize,
        );
        let _ = SetWindowPos(h, None, 0, 0, 0, 0, frame_flags);
        assert!(
            rd_window_win::list_windows()
                .expect("list_windows")
                .iter()
                .find(|w| w.hwnd == hwnd)
                .expect("记事本仍在枚举中")
                .resizable
        );
    }

    kill_child(&mut child);
}
