//! 窗口枚举：顶层可捕获窗口列表（标题/进程/矩形/DPI/样式）与窗口图标。
//!
//! 过滤规则集中在纯函数 [`is_capturable`]：可见、非 `WS_EX_TOOLWINDOW`
//! （除非 `WS_EX_APPWINDOW`）、非 cloak、标题非空、客户区非零、排除自身进程
//! 与 `Progman`/`Shell_TrayWnd`。纯函数便于单测；枚举侧只做廉价预过滤
//! （可见 + cloak），完整规则仍由 `is_capturable` 复核，单一事实来源。

use std::ffi::c_void;
use std::io::Cursor;
use std::mem;

use anyhow::{Context, Result};
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassLongPtrW, GetClassNameW, GetClientRect, GetIconInfo, GetWindowLongPtrW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
    SendMessageTimeoutW, GCLP_HICON, GCLP_HICONSM, GWL_EXSTYLE, GWL_STYLE, HICON, ICONINFO,
    ICON_BIG, ICON_SMALL, ICON_SMALL2, SMTO_ABORTIFHUNG, WM_GETICON, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW, WS_THICKFRAME, WS_VISIBLE,
};

use crate::Rect;

/// `WM_GETICON` 查询挂起窗口的超时（hung 窗口不能拖死枚举线程）。
const ICON_QUERY_TIMEOUT_MS: u32 = 200;
/// 图标统一输出尺寸（PNG 边长）。
const ICON_SIZE: u32 = 32;

/// 一个顶层窗口的静态属性（协议 `WindowInfo` 的本机来源）。
#[derive(Debug, Clone)]
pub struct WindowAttrs {
    /// 顶层窗口句柄。
    pub hwnd: isize,
    /// 窗口标题（空串表示无标题）。
    pub title: String,
    /// 进程可执行文件名（如 `notepad.exe`；查询失败为空串）。
    pub process: String,
    /// 进程 ID。
    pub pid: u32,
    /// 客户区屏幕矩形（物理像素）。
    pub rect: Rect,
    /// 窗口 DPI（96 = 100%）。
    pub dpi: u32,
    /// 是否可调尺寸（`WS_THICKFRAME`）。
    pub resizable: bool,
    /// 是否最小化。
    pub minimized: bool,
    /// `GWL_STYLE`。
    pub style: u32,
    /// `GWL_EXSTYLE`。
    pub ex_style: u32,
    /// DWM cloak（UWP 挂起/预启动窗口为 true，应排除）。
    pub cloaked: bool,
    /// 窗口类名。
    pub class: String,
}

/// 窗口是否值得展示/捕获（过滤规则，纯函数）。
///
/// 规则（与 T-008 步骤 1/2 一致）：
/// 1. 可见（顶层窗口的 `IsWindowVisible` ≡ `WS_VISIBLE`）；
/// 2. 非 `WS_EX_TOOLWINDOW`，除非同时有 `WS_EX_APPWINDOW`；
/// 3. 未被 DWM cloak（UWP 挂起窗口）；
/// 4. 标题非空；
/// 5. 客户区宽高 > 0；
/// 6. 排除自身进程与 `Progman`/`Shell_TrayWnd`。
///
/// 最小化窗口不排除（还原后可捕获，调用方用 `minimized` 字段区分）。
pub fn is_capturable(a: &WindowAttrs) -> bool {
    if a.style & WS_VISIBLE.0 == 0 {
        return false;
    }
    if a.ex_style & WS_EX_TOOLWINDOW.0 != 0 && a.ex_style & WS_EX_APPWINDOW.0 == 0 {
        return false;
    }
    if a.cloaked {
        return false;
    }
    if a.title.is_empty() {
        return false;
    }
    if a.rect.w == 0 || a.rect.h == 0 {
        return false;
    }
    if a.pid == std::process::id() {
        return false;
    }
    if matches!(a.class.as_str(), "Progman" | "Shell_TrayWnd") {
        return false;
    }
    true
}

/// 列出当前桌面上的可捕获顶层窗口（Z 序）。
///
/// 图标不在此获取（逐窗口取图标代价高），需要时对单个 hwnd 调
/// [`window_icon_png`]。
pub fn list_windows() -> Result<Vec<WindowAttrs>> {
    let _dpi = DpiGuard::new();

    let hwnds = roster_hwnds()?;
    let mut out = Vec::with_capacity(hwnds.len());
    for hwnd in hwnds {
        let h = HWND(hwnd as *mut c_void);
        // 预过滤：不可见与 cloak 窗口直接跳过，避免无意义的进程名/客户区查询。
        unsafe {
            if !IsWindowVisible(h).as_bool() || is_cloaked(h) {
                continue;
            }
        }
        if let Some(attrs) = attrs_of(h) {
            if is_capturable(&attrs) {
                out.push(attrs);
            }
        }
    }
    Ok(out)
}

/// 取窗口图标并编码为 32×32 RGBA 透明底 PNG。
///
/// 依次尝试 `WM_GETICON`（BIG/SMALL2/SMALL，带 hung 保护超时）与窗口类
/// `GCLP_HICON`/`GCLP_HICONSM`；无图标或解码失败返回 `None`。图标归窗口/
/// 窗口类所有，此处不调用 `DestroyIcon`。
pub fn window_icon_png(hwnd: isize) -> Option<Vec<u8>> {
    let h = HWND(hwnd as *mut c_void);
    let hicon = icon_handle_of(h)?;
    hicon_to_png32(hicon)
}

/// `EnumWindows` 收集全部顶层 HWND（Z 序）。
fn roster_hwnds() -> Result<Vec<isize>> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        out.push(hwnd.0 as isize);
        true.into()
    }
    let mut out: Vec<isize> = Vec::new();
    unsafe {
        EnumWindows(Some(collect), LPARAM(&mut out as *mut _ as isize))
            .context("EnumWindows 失败")?;
    }
    Ok(out)
}

/// 汇总单个窗口属性；窗口已销毁返回 `None`。
fn attrs_of(h: HWND) -> Option<WindowAttrs> {
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let style = GetWindowLongPtrW(h, GWL_STYLE) as u32;
        Some(WindowAttrs {
            hwnd: h.0 as isize,
            title: window_text(h),
            process: process_image_name(pid),
            pid,
            rect: client_rect_screen(h).ok()?,
            dpi: GetDpiForWindow(h),
            resizable: style & WS_THICKFRAME.0 != 0,
            minimized: IsIconic(h).as_bool(),
            style,
            ex_style: GetWindowLongPtrW(h, GWL_EXSTYLE) as u32,
            cloaked: is_cloaked(h),
            class: class_name(h),
        })
    }
}

/// `DWMWA_CLOAKED` != 0（UWP 挂起/预启动、其他虚拟桌面窗口）。
pub(crate) fn is_cloaked(h: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            mem::size_of::<u32>() as u32,
        )
    };
    hr.is_ok() && cloaked != 0
}

/// 客户区屏幕矩形（物理像素；DPI 上下文由调用方保证）。
pub(crate) fn client_rect_screen(h: HWND) -> Result<Rect> {
    let mut rc = RECT::default();
    unsafe { GetClientRect(h, &mut rc).context("GetClientRect 失败")? };
    let mut origin = POINT::default();
    unsafe {
        windows::Win32::Graphics::Gdi::ClientToScreen(h, &mut origin)
            .ok()
            .context("ClientToScreen 失败")?;
    }
    Ok(Rect {
        x: origin.x,
        y: origin.y,
        w: (rc.right - rc.left).max(0) as u32,
        h: (rc.bottom - rc.top).max(0) as u32,
    })
}

/// 窗口标题（UTF-16 → String）。
fn window_text(h: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(h, &mut buf) };
    decode_utf16(&buf[..len.max(0) as usize])
}

/// 窗口类名。
fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(h, &mut buf) };
    decode_utf16(&buf[..len.max(0) as usize])
}

/// 进程可执行文件名（basename）；打不开进程（权限/已退出）返回空串。
fn process_image_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let name = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    match name {
        Ok(()) => decode_utf16(&buf[..len as usize])
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or_default()
            .to_owned(),
        Err(err) => {
            tracing::debug!(pid, "QueryFullProcessImageNameW 失败: {err}");
            String::new()
        }
    }
}

fn decode_utf16(chars: &[u16]) -> String {
    String::from_utf16_lossy(chars)
}

/// 线程级 per-monitor-v2 DPI 上下文；Drop 时恢复原上下文。
///
/// 枚举/分组需在物理像素坐标系下调用 `GetClientRect`/`ClientToScreen`，
/// 否则 DPI 虚拟化返回缩放后的假坐标。
pub(crate) struct DpiGuard(DPI_AWARENESS_CONTEXT);

impl DpiGuard {
    pub(crate) fn new() -> Self {
        let prev =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if prev.is_invalid() {
            tracing::debug!("SetThreadDpiAwarenessContext 未生效（进程可能未声明 DPI aware）");
        }
        DpiGuard(prev)
    }
}

impl Drop for DpiGuard {
    fn drop(&mut self) {
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

/// 依次尝试 `WM_GETICON`（BIG → SMALL2 → SMALL）与窗口类图标，返回首个非空句柄。
///
/// `SendMessageTimeoutW` 对跨线程消息成功时返回非零成功标志，消息本身的
/// 返回值（HICON）写入 `lpdwResult` 出参，必须取出参而非函数返回值。
fn icon_handle_of(h: HWND) -> Option<HICON> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    for icon_type in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
        let mut result = 0usize;
        let sent = unsafe {
            SendMessageTimeoutW(
                h,
                WM_GETICON,
                WPARAM(icon_type as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                ICON_QUERY_TIMEOUT_MS,
                Some(&mut result),
            )
        }
        .0 != 0;
        if sent {
            let handle = HICON(result as *mut c_void);
            if !handle.is_invalid() {
                return Some(handle);
            }
        }
    }
    // 窗口未通过 WM_GETICON 提供图标 → 窗口类默认图标。
    for index in [GCLP_HICON, GCLP_HICONSM] {
        let handle = HICON(unsafe { GetClassLongPtrW(h, index) } as *mut c_void);
        if !handle.is_invalid() {
            return Some(handle);
        }
    }
    None
}

/// `HICON` → 32×32 RGBA PNG。
///
/// `GetIconInfo` 取 color/mask 位图，`GetDIBits` 读 32bpp；带 alpha 通道的
/// 图标直接使用 alpha，否则用 AND 掩码合成透明度（GDI 经典无 alpha 图标）。
/// 仅含单色位图的图标（无 `hbmColor`）不支持，返回 `None`。
fn hicon_to_png32(hicon: HICON) -> Option<Vec<u8>> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &mut info) }.ok()?;
    let (mask, color) = (info.hbmMask, info.hbmColor);
    let result = (|| {
        let (w, h, mut px) = read_32bpp(color)?;
        if !px.iter().skip(3).any(|&a| a != 0) {
            apply_mask(&mut px, w, h, mask);
        }
        bgra_to_rgba(&mut px);
        encode_png32(w, h, &px)
    })();
    // GetIconInfo 返回的位图由我们释放；HICON 归窗口/窗口类所有。
    unsafe {
        let _ = DeleteObject(mask.into());
        if !color.is_invalid() {
            let _ = DeleteObject(color.into());
        }
    }
    result
}

/// `GetDIBits` 读 32bpp 位图 → (宽, 高, BGRA，顶向下行序)。
fn read_32bpp(hbm: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    if hbm.is_invalid() {
        return None;
    }
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    let hdc: HDC = unsafe { CreateCompatibleDC(None) };
    if hdc.is_invalid() {
        return None;
    }
    let result = (|| {
        // 首次调用（缓冲为空）以真实尺寸填充 bmiHeader。
        let filled = unsafe { GetDIBits(hdc, hbm, 0, 0, None, &mut bi, DIB_RGB_COLORS) } != 0;
        if !filled {
            return None;
        }
        let (w, h) = (
            bi.bmiHeader.biWidth.unsigned_abs(),
            bi.bmiHeader.biHeight.unsigned_abs(),
        );
        if w == 0 || h == 0 {
            return None;
        }
        bi.bmiHeader.biHeight = -bi.bmiHeader.biHeight; // 顶向下
        bi.bmiHeader.biCompression = 0; // BI_RGB
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let read = unsafe {
            GetDIBits(
                hdc,
                hbm,
                0,
                h,
                Some(buf.as_mut_ptr() as *mut c_void),
                &mut bi,
                DIB_RGB_COLORS,
            )
        };
        (read == h as i32).then_some((w, h, buf))
    })();
    unsafe {
        let _ = DeleteDC(hdc);
    }
    result
}

/// 无 alpha 通道时用 AND 掩码合成透明度：掩码位 0 = 不透明。
///
/// 掩码经 `GetDIBits` 转 32bpp 后：黑（B=0）= 位 0 = 不透明，白 = 位 1 = 透明。
fn apply_mask(px: &mut [u8], w: u32, h: u32, mask: HBITMAP) {
    let set_opaque = |px: &mut [u8]| {
        for a in px.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
    };
    let Some((_mw, _mh, mp)) = read_32bpp(mask) else {
        set_opaque(px); // 拿不到掩码：视为全不透明，避免图标全透明
        return;
    };
    let (cw, mw) = (w as usize, _mw as usize);
    let row_bytes = cw * 4;
    let mask_h = _mh as usize;
    // 有 color 位图时掩码与图标同高；高度异常时按底对齐处理。
    let off_y = mask_h.saturating_sub(h as usize);
    for (row, chunk) in px.chunks_exact_mut(row_bytes).enumerate() {
        for (col, alpha) in chunk.iter_mut().skip(3).step_by(4).enumerate() {
            let mx = col.min(mw.saturating_sub(1));
            let my = (row + off_y).min(mask_h.saturating_sub(1));
            *alpha = if mp[(my * mw + mx) * 4] == 0 { 255 } else { 0 };
        }
    }
}

/// BGRA → RGBA（就地交换 R/B）。
fn bgra_to_rgba(px: &mut [u8]) {
    for chunk in px.as_chunks_mut::<4>().0 {
        chunk.swap(0, 2);
    }
}

/// RGBA → PNG（非 32×32 时最近邻缩放，保持图标边缘锐利）。
fn encode_png32(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(w, h, rgba.to_vec())?;
    let img = if (w, h) == (ICON_SIZE, ICON_SIZE) {
        img
    } else {
        image::imageops::resize(
            &img,
            ICON_SIZE,
            ICON_SIZE,
            image::imageops::FilterType::Nearest,
        )
    };
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}
