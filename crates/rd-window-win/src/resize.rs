//! 1:1 适配：按视口换算并调整窗口客户区，记录/恢复 WINDOWPLACEMENT（US-03）。
//!
//! 控制端按物理像素 1:1 显示：[`target_client_size`] 只做 `css × dpr` 取整换算，
//! 远端窗口 DPI 不参与（远端窗口在其 DPI 下以更多/更少像素渲染相同内容；若
//! 要求远端字号观感一致，由控制端调整 dpr 参数）。[`fit_window`] 用
//! `AdjustWindowRectExForDpi` 由目标客户区反推外框尺寸并 `SetWindowPos`，回读
//! `GetClientRect` 校验：受程序最小/最大尺寸限制时返回 [`FitResult::Partial`]，
//! 无 `WS_THICKFRAME` 时返回 [`FitResult::NotResizable`]，调用方据此退化本地
//! 缩放（架构 §3.1）。[`save_placement`]/[`restore_placement`] 保存与恢复会话
//! 前的原状态（含最大化）。

use std::ffi::c_void;
use std::mem;

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetMenu, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, IsIconic,
    IsWindow, IsZoomed, SetWindowPlacement, SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE, WINDOWPLACEMENT,
    WINDOW_EX_STYLE, WINDOW_STYLE, WS_THICKFRAME,
};

use crate::enumerate::DpiGuard;
use crate::Rect;

/// 目标客户区最小宽（视口过小或 dpr 异常时的下限）。
pub const MIN_CLIENT_W: u32 = 200;
/// 目标客户区最小高。
pub const MIN_CLIENT_H: u32 = 150;

/// 按控制端视口换算目标客户区物理尺寸：`round(css × dpr)`，下限 200×150。
pub fn target_client_size(css_w: u32, css_h: u32, dpr: f32) -> (u32, u32) {
    // f32 → u32 为饱和转换（NaN → 0、超界 → u32::MAX），max/clamp 统一兜底。
    let scale = |css: u32, min: u32| {
        let px = (css as f32 * dpr).round();
        (px.max(0.0) as u32).clamp(min, i32::MAX as u32)
    };
    (scale(css_w, MIN_CLIENT_W), scale(css_h, MIN_CLIENT_H))
}

/// [`fit_window`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FitResult {
    /// 客户区与目标完全一致。
    Exact,
    /// 窗口受程序最小/最大尺寸限制，实际客户区与目标不一致。
    Partial {
        /// 回读的实际客户区宽。
        actual_w: u32,
        /// 回读的实际客户区高。
        actual_h: u32,
    },
    /// 窗口无 `WS_THICKFRAME`，不可调尺寸。
    NotResizable,
}

/// 把窗口客户区调整为 `client_w × client_h`（物理像素）。
///
/// 最小化/最大化窗口先还原为普通态再调；外框超出所在显示器工作区时移到
/// 工作区左上（仍超出则允许，WGC 仍可捕获屏外部分）。调整后回读客户区，
/// 与目标不一致（程序自我限制）时返回 [`FitResult::Partial`]。
pub fn fit_window(hwnd: isize, client_w: u32, client_h: u32) -> Result<FitResult> {
    let _dpi = DpiGuard::new();
    let h = HWND(hwnd as *mut c_void);

    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            bail!("窗口句柄无效: {hwnd:#x}");
        }
        let style = GetWindowLongPtrW(h, GWL_STYLE) as u32;
        if style & WS_THICKFRAME.0 == 0 {
            return Ok(FitResult::NotResizable);
        }
        restore_to_normal(h);

        // 还原后的样式才是普通态样式；由目标客户区经窗口装饰反推外框尺寸。
        let ex_style = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        let has_menu = !GetMenu(h).is_invalid();
        let dpi = GetDpiForWindow(h);
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: client_w as i32,
            bottom: client_h as i32,
        };
        AdjustWindowRectExForDpi(
            &mut rc,
            WINDOW_STYLE(style),
            has_menu,
            WINDOW_EX_STYLE(ex_style),
            dpi,
        )
        .context("AdjustWindowRectExForDpi 失败")?;

        SetWindowPos(
            h,
            None,
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
        )
        .context("SetWindowPos 失败")?;
        ensure_in_work_area(h);

        // 回读校验：程序可经 WM_GETMINMAXINFO 拒绝目标尺寸。
        let mut rc = RECT::default();
        GetClientRect(h, &mut rc).context("GetClientRect 失败")?;
        let actual_w = (rc.right - rc.left).max(0) as u32;
        let actual_h = (rc.bottom - rc.top).max(0) as u32;
        if actual_w == client_w && actual_h == client_h {
            Ok(FitResult::Exact)
        } else {
            Ok(FitResult::Partial { actual_w, actual_h })
        }
    }
}

/// 最小化/最大化还原为普通态（`SW_RESTORE` 两者皆可；最小化窗口还原后可能
/// 落在最大化，故二次判断）。
fn restore_to_normal(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        if IsZoomed(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
    }
}

/// 外框超出所在显示器工作区时移到工作区左上（仍超出则允许超出）。
fn ensure_in_work_area(h: HWND) {
    unsafe {
        let mut wr = RECT::default();
        if GetWindowRect(h, &mut wr).is_err() {
            return;
        }
        let monitor = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return;
        }
        let mut info = MONITORINFO {
            cbSize: mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }
        let work = info.rcWork;
        let outside = wr.left < work.left
            || wr.top < work.top
            || wr.right > work.right
            || wr.bottom > work.bottom;
        if outside {
            let _ = SetWindowPos(
                h,
                None,
                work.left,
                work.top,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// `WINDOWPLACEMENT` 快照（会话开始保存、结束恢复；含最大化状态）。
#[derive(Debug, Clone, PartialEq)]
pub struct SavedPlacement(WINDOWPLACEMENT);

impl SavedPlacement {
    /// `showCmd`（1 普通态 / 2 最小化 / 3 最大化）。
    pub fn show_cmd(&self) -> u32 {
        self.0.showCmd
    }

    /// 还原到普通态时的外框矩形（屏幕物理像素）。
    pub fn normal_position(&self) -> Rect {
        let rc = self.0.rcNormalPosition;
        Rect {
            x: rc.left,
            y: rc.top,
            w: (rc.right - rc.left).max(0) as u32,
            h: (rc.bottom - rc.top).max(0) as u32,
        }
    }
}

/// 保存窗口当前布局（`GetWindowPlacement`）。
pub fn save_placement(hwnd: isize) -> Result<SavedPlacement> {
    let _dpi = DpiGuard::new();
    let h = HWND(hwnd as *mut c_void);
    let mut wp = WINDOWPLACEMENT {
        length: mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    unsafe { GetWindowPlacement(h, &mut wp).context("GetWindowPlacement 失败")? };
    Ok(SavedPlacement(wp))
}

/// 恢复 [`save_placement`] 保存的布局（`SetWindowPlacement`，含最大化状态）。
pub fn restore_placement(hwnd: isize, p: &SavedPlacement) -> Result<()> {
    let _dpi = DpiGuard::new();
    let h = HWND(hwnd as *mut c_void);
    unsafe { SetWindowPlacement(h, &p.0).context("SetWindowPlacement 失败")? };
    Ok(())
}
