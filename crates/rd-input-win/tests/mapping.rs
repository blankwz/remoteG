//! 坐标映射纯函数测试（T-012）：单屏/多显示器负坐标、不同 vdesk 尺寸、
//! 越界钳制、退化尺寸不除零。
#![cfg(windows)]

use rd_input_win::client_to_absolute;

#[test]
fn single_monitor_maps_corners_exactly() {
    let vdesk = (0, 0, 1920, 1080);
    assert_eq!(client_to_absolute((0, 0), 0, 0, vdesk), (0, 0));
    assert_eq!(
        client_to_absolute((0, 0), 1919, 1079, vdesk),
        (65535, 65535)
    );
}

#[test]
fn negative_secondary_monitor_maps_into_range() {
    // 左屏位于 -1920..0，虚拟桌面为两块 1920x1080 横向拼接。
    let vdesk = (-1920, 0, 3840, 1080);
    // 左屏最左像素（客户区原点本身在负坐标屏上）。
    assert_eq!(client_to_absolute((-1920, 0), 0, 0, vdesk), (0, 0));
    // 左屏最右像素：屏幕坐标 -1，归一化值应为 (1919 * 65535) / 3839。
    let (nx, _) = client_to_absolute((-1920, 0), 1919, 0, vdesk);
    assert_eq!(nx, (1919 * 65535) / 3839);
    // 右屏（原点 (0,0)）最右像素 → 65535。
    assert_eq!(client_to_absolute((0, 0), 1919, 0, vdesk), (65535, 0));
}

#[test]
fn negative_y_desktop_maps_into_range() {
    // 主屏在虚拟桌面下方（上方有屏）：vdesk y 起点 -1080。
    let vdesk = (0, -1080, 1920, 2160);
    assert_eq!(client_to_absolute((0, -1080), 0, 0, vdesk), (0, 0));
    let (_, ny) = client_to_absolute((0, -1080), 0, 1079, vdesk);
    assert_eq!(ny, (1079 * 65535) / 2159);
    assert_eq!(client_to_absolute((0, 0), 0, 1079, vdesk).1, 65535);
}

#[test]
fn out_of_range_clamps() {
    let vdesk = (0, 0, 1920, 1080);
    // 客户区原点在另一块屏上、点远超本桌面 → 钳到上界。
    assert_eq!(client_to_absolute((1920, 0), 5000, 0, vdesk), (65535, 0));
    // 负坐标 → 钳到 0。
    assert_eq!(client_to_absolute((0, 0), -10, -10, vdesk), (0, 0));
}

#[test]
fn degenerate_virtual_desktop_does_not_panic() {
    // 退化尺寸（宽/高为 0 或 1）不除零、不 panic，结果恒在合法区间。
    let degenerate = [(0, 0, 0, 0), (0, 0, 1, 1), (5, 5, 1, 0)];
    for vdesk in degenerate {
        for (x, y) in [(0, 0), (100, 100), (-50, 10_000)] {
            let (nx, ny) = client_to_absolute((0, 0), x, y, vdesk);
            assert!(
                (0..=65535).contains(&nx) && (0..=65535).contains(&ny),
                "退化 vdesk {vdesk:?} 下 ({x},{y}) 越界: ({nx},{ny})"
            );
        }
    }
}

#[test]
fn tall_narrow_desktop_maps_corners() {
    // 竖屏 1080x1920。
    let vdesk = (0, 0, 1080, 1920);
    assert_eq!(client_to_absolute((0, 0), 0, 0, vdesk), (0, 0));
    assert_eq!(
        client_to_absolute((0, 0), 1079, 1919, vdesk),
        (65535, 65535)
    );
}

#[test]
fn is_monotonic_in_client_coordinates() {
    let vdesk = (-1920, 0, 3840, 1080);
    let origin = (-500, 42);
    let mut prev = (0, 0);
    for step in 0..=100 {
        let cur = client_to_absolute(origin, step * 30, step * 9, vdesk);
        assert!(
            cur.0 >= prev.0 && cur.1 >= prev.1,
            "映射必须单调: {prev:?} -> {cur:?}"
        );
        prev = cur;
    }
}

/// 真实注入冒烟：按住 Shift/Ctrl/Alt 后 `release_all`，用 `GetAsyncKeyState`
/// 验证系统键状态全部抬起（防卡键 DoD）。
#[test]
fn release_all_lifts_all_held_keys() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT,
    };

    let pressed = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } < 0;

    // 测试环境恰有这些修饰键被物理按住时跳过，避免误报。
    if pressed(VK_SHIFT) || pressed(VK_CONTROL) || pressed(VK_MENU) {
        eprintln!("跳过：Shift/Ctrl/Alt 正被物理按住");
        return;
    }

    let mut inj = rd_input_win::Injector::new();
    // LShift=0x2A, LCtrl=0x1D, LAlt=0x38（均非 extended）。
    inj.key(0x2A, false, true).expect("注入 Shift down");
    inj.key(0x1D, false, true).expect("注入 Ctrl down");
    inj.key(0x38, false, true).expect("注入 Alt down");

    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(pressed(VK_SHIFT), "注入后 Shift 应处于按下状态");
    assert!(pressed(VK_CONTROL), "注入后 Ctrl 应处于按下状态");
    assert!(pressed(VK_MENU), "注入后 Alt 应处于按下状态");

    inj.release_all();
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(!pressed(VK_SHIFT), "release_all 后 Shift 未抬起");
    assert!(!pressed(VK_CONTROL), "release_all 后 Ctrl 未抬起");
    assert!(!pressed(VK_MENU), "release_all 后 Alt 未抬起");
}
