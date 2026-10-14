//! `client_rect_in_frame` 纯计算单测：DPI 100%/150%、有/无标题栏、越界裁剪。

use rd_capture_win::crop::{client_rect_in_frame, Rect};

/// Win10/11 标准带标题栏窗口（DPI 100%）：
/// 视觉边界 800x600，客户区偏移 = 8px 边框 + 31px 标题栏。
#[test]
fn dpi_100_titled_window() {
    let r = client_rect_in_frame((800, 600), (0, 0), (8, 31), (784, 569)).unwrap();
    assert_eq!(
        r,
        Rect {
            x: 8,
            y: 31,
            w: 784,
            h: 569
        }
    );
}

/// DPI 150%：8 逻辑 px → 12 物理 px，31 逻辑 px → 约 47 物理 px；一切以物理像素计算。
#[test]
fn dpi_150_titled_window() {
    let r = client_rect_in_frame((1200, 900), (0, 0), (12, 47), (1176, 853)).unwrap();
    assert_eq!(
        r,
        Rect {
            x: 12,
            y: 47,
            w: 1176,
            h: 853
        }
    );
}

/// 无边框窗口（Chrome 全屏/Electron 无边框、最大化的现代应用）：偏移为 0。
#[test]
fn borderless_window_no_offset() {
    let r = client_rect_in_frame((1280, 720), (0, 0), (0, 0), (1280, 720)).unwrap();
    assert_eq!(
        r,
        Rect {
            x: 0,
            y: 0,
            w: 1280,
            h: 720
        }
    );
}

/// 窗口在负坐标副屏：偏移为相对量，不受绝对坐标影响。
#[test]
fn secondary_monitor_negative_origin() {
    let r = client_rect_in_frame((800, 600), (-1920, 0), (-1912, 31), (784, 569)).unwrap();
    assert_eq!(
        r,
        Rect {
            x: 8,
            y: 31,
            w: 784,
            h: 569
        }
    );
}

/// 帧比视觉边界小（DWM 裁剪/圆角等）：客户区超出部分被裁到帧内。
#[test]
fn client_overhang_clamped_to_frame() {
    let r = client_rect_in_frame((790, 580), (0, 0), (8, 31), (784, 569)).unwrap();
    assert_eq!(
        r,
        Rect {
            x: 8,
            y: 31,
            w: 782, // 790 - 8
            h: 549, // 580 - 31
        }
    );
}

/// 客户区原点在帧外（右侧之外）：None。
#[test]
fn client_outside_frame_returns_none() {
    assert!(
        client_rect_in_frame((800, 600), (0, 0), (900, 31), (100, 50)).is_none(),
        "客户区原点在帧右之外应返回 None"
    );
    assert!(
        client_rect_in_frame((800, 600), (0, 0), (8, 700), (100, 50)).is_none(),
        "客户区原点在帧下之外应返回 None"
    );
}

/// 最小化 / 已销毁窗口：客户区为空 → None。
#[test]
fn minimized_empty_client_returns_none() {
    assert!(client_rect_in_frame((800, 600), (0, 0), (0, 0), (0, 0)).is_none());
    assert!(client_rect_in_frame((0, 0), (0, 0), (0, 0), (100, 50)).is_none());
    assert!(client_rect_in_frame((800, 600), (0, 0), (0, 0), (800, 0)).is_none());
}
