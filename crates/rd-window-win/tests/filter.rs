//! `is_capturable` 过滤规则单测（纯函数，不依赖窗口系统）。

use rd_window_win::{is_capturable, WindowAttrs};

/// 正常可捕获窗口基准（普通应用主窗口）。
fn base() -> WindowAttrs {
    WindowAttrs {
        hwnd: 0x10010,
        title: "无标题 - 记事本".into(),
        process: "notepad.exe".into(),
        pid: 4242,
        rect: rd_window_win::Rect {
            x: 100,
            y: 80,
            w: 1280,
            h: 720,
        },
        dpi: 144,
        resizable: true,
        minimized: false,
        style: 0x16CF_0000, // WS_VISIBLE | WS_CAPTION | WS_THICKFRAME | …
        ex_style: 0x0000_0100,
        cloaked: false,
        class: "Notepad".into(),
    }
}

#[test]
fn 正常窗口可捕获() {
    let a = base();
    assert!(is_capturable(&a));
}

#[test]
fn 不可见窗口排除() {
    let mut a = base();
    a.style &= !0x1000_0000; // 清 WS_VISIBLE
    assert!(!is_capturable(&a));
}

#[test]
fn 工具窗口排除() {
    let mut a = base();
    a.ex_style |= 0x0000_0080; // WS_EX_TOOLWINDOW
    a.title = "Settings".into();
    assert!(!is_capturable(&a), "工具窗口且无 APPWINDOW 应排除");
}

#[test]
fn 工具窗口带应用窗口样式保留() {
    let mut a = base();
    a.ex_style |= 0x0000_0080 | 0x0004_0000; // WS_EX_TOOLWINDOW | WS_EX_APPWINDOW
    assert!(is_capturable(&a));
}

#[test]
fn cloak的_uwp_窗口排除() {
    let mut a = base();
    a.cloaked = true;
    assert!(!is_capturable(&a));
}

#[test]
fn 空标题排除() {
    let mut a = base();
    a.title = String::new();
    assert!(!is_capturable(&a));
}

#[test]
fn 零尺寸客户区排除() {
    let mut a = base();
    a.rect = rd_window_win::Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    assert!(!is_capturable(&a));

    let mut b = base();
    b.rect = rd_window_win::Rect {
        x: 0,
        y: 0,
        w: 800,
        h: 0,
    };
    assert!(!is_capturable(&b));
}

#[test]
fn 自身进程排除() {
    let mut a = base();
    a.pid = std::process::id();
    assert!(!is_capturable(&a));
}

#[test]
fn 桌面与任务栏类排除() {
    for class in ["Progman", "Shell_TrayWnd"] {
        let mut a = base();
        a.class = class.into();
        assert!(!is_capturable(&a), "class={class} 应排除");
    }
}

#[test]
fn 最小化窗口不排除() {
    let mut a = base();
    a.minimized = true;
    a.rect = rd_window_win::Rect {
        x: -32000,
        y: -32000,
        w: 160,
        h: 28,
    };
    assert!(
        is_capturable(&a),
        "最小化窗口保留，由调用方用 minimized 区分"
    );
}

#[test]
fn 最小化塌缩窗口保留() {
    // 最小化后客户区塌缩 0×0：设计意图是保留在列表（minimized 标记，
    // 打开时自动还原），否则列表随最小化/还原来回闪烁（用户跨机实测）。
    let mut a = base();
    a.rect.w = 0;
    a.rect.h = 0;
    a.minimized = true;
    assert!(is_capturable(&a));
    // 非最小化的 0×0 仍然排除（不可见/即将销毁的瞬时态）。
    let mut b = base();
    b.rect.w = 0;
    b.rect.h = 0;
    b.minimized = false;
    assert!(!is_capturable(&b));
}
