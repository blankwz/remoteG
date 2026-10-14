//! testwin 子命令：Win32 测试窗口（PerMonitorV2 DPI + GDI 绘制，T-024）。
//!
//! 布局约定（供 compare 使用）：
//! - 四角 16×16 `#FF00FF` 定位标记 + 相邻 16×16 黑白棋盘（客户区最边缘）；
//! - 文字块：9/10/11/12/14/16 pt，ClearType 与灰度各一行，中英文混排；
//! - 彩色字配彩色背景（红/蓝/绿组合）；
//! - 1 px 细线（间距 1/2/3）与 1 px 网格；
//! - `clock` 模式右上角毫秒时间码 + 帧计数，约 60 fps；
//! - `scroll` 模式文字区每帧滚动 1 px；
//! - 左键点击计数与坐标同时写入窗口标题 `clarity-bench clicks=<n> last=<x>,<y>`；
//! - 右键弹出 3 项上下文菜单（TrackPopupMenu，供 T-025 用例）。

use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetDeviceCaps,
    GetTextExtentPoint32W, IntersectClipRect, InvalidateRect, LineTo, MoveToEx, RestoreDC, SaveDC,
    SelectObject, SetBkMode, SetTextColor, TextOutW, UpdateWindow, ANTIALIASED_QUALITY,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, FW_NORMAL, HDC, HFONT,
    LOGPIXELSY, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DispatchMessageW, GetMessageW, LoadCursorW, PostQuitMessage, RegisterClassW, SetTimer,
    SetWindowTextW, ShowWindow, TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, IDC_ARROW, MF_STRING, MSG, SW_SHOW, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

/// 测试模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 静止文字画面（清晰度基准）。
    Static,
    /// 文字区每帧滚动 1 px。
    Scroll,
    /// 静止画面 + 右上角毫秒时间码/帧计数（延迟基准）。
    Clock,
}

/// testwin 选项。
#[derive(Debug, Clone, Copy)]
pub struct TestwinOptions {
    pub mode: Mode,
    /// 客户区宽（物理像素）。
    pub width: i32,
    /// 客户区高（物理像素）。
    pub height: i32,
}

impl TestwinOptions {
    /// 解析 `testwin [--mode static|scroll|clock] [--size WxH]`。
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut opts = Self {
            mode: Mode::Static,
            width: 1280,
            height: 800,
        };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--mode" => {
                    i += 1;
                    opts.mode = match args.get(i).map(String::as_str) {
                        Some("static") => Mode::Static,
                        Some("scroll") => Mode::Scroll,
                        Some("clock") => Mode::Clock,
                        _ => bail!("--mode 需要 static|scroll|clock"),
                    };
                }
                "--size" => {
                    i += 1;
                    let raw = args.get(i).context("--size 需要 WxH（如 1280x800）")?;
                    let (w, h) = parse_size(raw)?;
                    opts.width = w;
                    opts.height = h;
                }
                other => bail!("testwin 未知参数: {other}（支持 --mode <…> --size <WxH>）"),
            }
            i += 1;
        }
        if !(64..=16384).contains(&opts.width) || !(64..=16384).contains(&opts.height) {
            bail!(
                "--size 超出范围（64..=16384）: {}x{}",
                opts.width,
                opts.height
            );
        }
        Ok(opts)
    }
}

fn parse_size(raw: &str) -> Result<(i32, i32)> {
    let (w, h) = raw
        .split_once(['x', 'X'])
        .context("--size 格式应为 WxH（如 1280x800）")?;
    let w: i32 = w
        .trim()
        .parse()
        .with_context(|| format!("--size 宽非法: {raw}"))?;
    let h: i32 = h
        .trim()
        .parse()
        .with_context(|| format!("--size 高非法: {raw}"))?;
    Ok((w, h))
}

/// 窗口会话状态（消息循环单线程访问）。
struct AppState {
    opts: TestwinOptions,
    frame: u64,
    clicks: u32,
    last_click: Option<(i32, i32)>,
    client_w: i32,
    client_h: i32,
}

thread_local! {
    static STATE: RefCell<AppState> = const {
        RefCell::new(AppState {
            opts: TestwinOptions { mode: Mode::Static, width: 1280, height: 800 },
            frame: 0,
            clicks: 0,
            last_click: None,
            client_w: 1280,
            client_h: 800,
        })
    };
}

/// 创建窗口并进入消息循环，窗口关闭后返回。
pub fn run(opts: TestwinOptions) -> Result<()> {
    unsafe {
        // PerMonitorV2：每个显示器独立 DPI；不支持时退回 system aware。
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        }
        let hinstance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: w!("clarity-bench-testwin"),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let mut rect = RECT {
            left: 0,
            top: 0,
            right: opts.width,
            bottom: opts.height,
        };
        let _ = AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("clarity-bench-testwin"),
            w!("clarity-bench clicks=0 last=-,-"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;

        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.opts = opts;
            st.client_w = opts.width;
            st.client_h = opts.height;
        });

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        if opts.mode != Mode::Static {
            SetTimer(Some(hwnd), 1, 16, None); // ≈60 fps
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_CONTEXTMENU, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_PAINT, WM_SIZE, WM_TIMER,
    };
    match msg {
        WM_PAINT => paint(hwnd),
        WM_ERASEBKGND => return LRESULT(1),
        WM_TIMER => advance_frame(hwnd),
        WM_LBUTTONDOWN => on_click(hwnd, lparam),
        WM_CONTEXTMENU => on_context_menu(hwnd, lparam),
        WM_SIZE => update_size(lparam),
        WM_DESTROY => PostQuitMessage(0),
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let (w, h) = STATE.with(|s| {
        let st = s.borrow();
        (st.client_w, st.client_h)
    });
    if w <= 0 || h <= 0 {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    // 双缓冲，避免时钟/滚动重绘闪烁。
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp.into());
    draw_scene(mem, w, h);
    let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
    let _ = SelectObject(mem, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem);
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn advance_frame(hwnd: HWND) {
    STATE.with(|s| s.borrow_mut().frame += 1);
    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = UpdateWindow(hwnd);
}

unsafe fn update_size(lparam: LPARAM) {
    let w = (lparam.0 & 0xFFFF) as u16 as i32;
    let h = ((lparam.0 >> 16) & 0xFFFF) as u16 as i32;
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.client_w = w;
        st.client_h = h;
    });
}

unsafe fn on_click(hwnd: HWND, lparam: LPARAM) {
    let x = lparam.0 as i16 as i32;
    let y = (lparam.0 >> 16) as i16 as i32;
    let title = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.clicks += 1;
        st.last_click = Some((x, y));
        format!("clarity-bench clicks={} last={},{}", st.clicks, x, y)
    });
    let _ = SetWindowTextW(hwnd, &HSTRING::from(title));
    let _ = InvalidateRect(Some(hwnd), None, false);
}

unsafe fn on_context_menu(hwnd: HWND, lparam: LPARAM) {
    let x = lparam.0 as i16 as i32;
    let y = (lparam.0 >> 16) as i16 as i32;
    if x == -1 && y == -1 {
        return; // 键盘触发的上下文菜单，忽略
    }
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(menu, MF_STRING, 101, w!("菜单项 A"));
    let _ = AppendMenuW(menu, MF_STRING, 102, w!("菜单项 B"));
    let _ = AppendMenuW(menu, MF_STRING, 103, w!("菜单项 C"));
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        x,
        y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

unsafe fn fill_rect(hdc: HDC, l: i32, t: i32, r: i32, b: i32, color: COLORREF) {
    let rect = RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    };
    let brush = CreateSolidBrush(color);
    let _ = FillRect(hdc, &rect, brush);
    let _ = DeleteObject(brush.into());
}

/// 16×16 黑白棋盘（8 px 格）。
unsafe fn draw_checker(hdc: HDC, x: i32, y: i32) {
    for i in 0..2i32 {
        for j in 0..2i32 {
            let c = if (i + j) % 2 == 0 {
                rgb(0, 0, 0)
            } else {
                rgb(255, 255, 255)
            };
            fill_rect(hdc, x + i * 8, y + j * 8, x + i * 8 + 8, y + j * 8 + 8, c);
        }
    }
}

/// 四角定位标记：16×16 品红 + 相邻内侧 16×16 棋盘。
unsafe fn draw_markers(hdc: HDC, w: i32, h: i32) {
    let magenta = rgb(0xFF, 0x00, 0xFF);
    fill_rect(hdc, 0, 0, 16, 16, magenta);
    draw_checker(hdc, 16, 0);
    fill_rect(hdc, w - 16, 0, w, 16, magenta);
    draw_checker(hdc, w - 32, 0);
    fill_rect(hdc, 0, h - 16, 16, h, magenta);
    draw_checker(hdc, 16, h - 16);
    fill_rect(hdc, w - 16, h - 16, w, h, magenta);
    draw_checker(hdc, w - 32, h - 16);
}

/// 1 px 细线（间距 1/2/3）与 1 px 网格。
unsafe fn draw_lines_panel(hdc: HDC, x0: i32, y0: i32) {
    let pen = CreatePen(PS_SOLID, 1, rgb(0, 0, 0));
    let old = SelectObject(hdc, pen.into());
    for (gi, gap) in [1i32, 2, 3].iter().enumerate() {
        let yy = y0 + gi as i32 * 40;
        for i in 0..8i32 {
            let _ = MoveToEx(hdc, x0, yy + i * gap, None);
            let _ = LineTo(hdc, x0 + 120, yy + i * gap);
        }
    }
    let gx = x0 + 160;
    for i in 0..=8i32 {
        let _ = MoveToEx(hdc, gx + i * 10, y0, None);
        let _ = LineTo(hdc, gx + i * 10, y0 + 80);
        let _ = MoveToEx(hdc, gx, y0 + i * 10, None);
        let _ = LineTo(hdc, gx + 80, y0 + i * 10);
    }
    let _ = SelectObject(hdc, old);
    let _ = DeleteObject(pen.into());
}

/// 创建指定像素高度的文字字体。
unsafe fn create_font(px: i32, clear_type: bool, face: PCWSTR) -> HFONT {
    let quality = if clear_type {
        CLEARTYPE_QUALITY
    } else {
        ANTIALIASED_QUALITY
    };
    CreateFontW(
        -px,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        quality,
        DEFAULT_PITCH.0 as u32,
        face,
    )
}

/// 绘制一行文字，返回下一行的 y。
#[allow(clippy::too_many_arguments)]
unsafe fn draw_text(
    hdc: HDC,
    x: i32,
    y: i32,
    text: &str,
    pt: i32,
    fg: [u8; 3],
    clear_type: bool,
    dpi: i32,
    bg: Option<[u8; 3]>,
) -> i32 {
    let px = pt * dpi / 72;
    if let Some(b) = bg {
        fill_rect(
            hdc,
            x - 4,
            y - 2,
            x + 560,
            y + px + 2,
            rgb(b[0], b[1], b[2]),
        );
    }
    let font = create_font(px, clear_type, w!("Segoe UI"));
    let old = SelectObject(hdc, font.into());
    SetTextColor(hdc, rgb(fg[0], fg[1], fg[2]));
    let _ = SetBkMode(hdc, TRANSPARENT);
    let wide: Vec<u16> = text.encode_utf16().collect();
    let _ = TextOutW(hdc, x, y, &wide);
    let _ = SelectObject(hdc, old);
    let _ = DeleteObject(font.into());
    y + px + 8
}

/// 文字块总高（与 draw_text_panel 的行进公式一致）。
fn panel_height(dpi: i32) -> i32 {
    let pts = [9i32, 10, 11, 12, 14, 16];
    let text_rows: i32 = pts.iter().map(|pt| (pt * dpi / 72 + 8) * 2).sum();
    let color_rows: i32 = 3 * (14 * dpi / 72 + 8);
    text_rows + color_rows
}

/// 文字块：多字号 ClearType/灰度 + 彩色字配彩色背景。
unsafe fn draw_text_panel(hdc: HDC, x: i32, y0: i32, dpi: i32) {
    let mut y = y0;
    for pt in [9i32, 10, 11, 12, 14, 16] {
        y = draw_text(
            hdc,
            x,
            y,
            &format!("清晰度测试 {pt}pt ClearType 汉字 English 0123456789"),
            pt,
            [0, 0, 0],
            true,
            dpi,
            None,
        );
        y = draw_text(
            hdc,
            x,
            y,
            &format!("清晰度测试 {pt}pt 灰度   汉字 English 0123456789"),
            pt,
            [0, 0, 0],
            false,
            dpi,
            None,
        );
    }
    y = draw_text(
        hdc,
        x,
        y,
        "红字蓝底 Red on Blue 0123 汉字",
        14,
        [255, 0, 0],
        true,
        dpi,
        Some([0, 0, 255]),
    );
    y = draw_text(
        hdc,
        x,
        y,
        "蓝字绿底 Blue on Green 0123 汉字",
        14,
        [0, 0, 255],
        true,
        dpi,
        Some([0, 255, 0]),
    );
    let _ = draw_text(
        hdc,
        x,
        y,
        "绿字红底 Green on Red 0123 汉字",
        14,
        [0, 255, 0],
        true,
        dpi,
        Some([255, 0, 0]),
    );
}

/// clock 模式：右上角毫秒时间码 + 帧计数（右对齐，等宽字体防抖动）。
unsafe fn draw_clock(hdc: HDC, w: i32, dpi: i32, frame: u64) {
    let t = GetLocalTime();
    let text = format!(
        "{:02}:{:02}:{:02}.{:03}  frame={}",
        t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, frame
    );
    let px = 36 * dpi / 72;
    let font = create_font(px, true, w!("Consolas"));
    let old = SelectObject(hdc, font.into());
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
    let x = (w - size.cx - 24).max(340);
    SetTextColor(hdc, rgb(0, 0, 0));
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = TextOutW(hdc, x, 56, &wide);
    let _ = SelectObject(hdc, old);
    let _ = DeleteObject(font.into());
}

unsafe fn draw_click_info(hdc: HDC, h: i32, dpi: i32, clicks: u32, last: Option<(i32, i32)>) {
    let text = match last {
        Some((x, y)) => format!("clicks={} last={},{}", clicks, x, y),
        None => format!("clicks={} last=-,-", clicks),
    };
    let _ = draw_text(hdc, 48, h - 56, &text, 14, [0, 0, 0], true, dpi, None);
}

unsafe fn draw_scene(hdc: HDC, w: i32, h: i32) {
    let dpi = GetDeviceCaps(Some(hdc), LOGPIXELSY);
    fill_rect(hdc, 0, 0, w, h, rgb(255, 255, 255));
    draw_markers(hdc, w, h);
    draw_lines_panel(hdc, w - 320, 150);
    let (mode, frame, clicks, last) = STATE.with(|s| {
        let st = s.borrow();
        (st.opts.mode, st.frame, st.clicks, st.last_click)
    });
    match mode {
        Mode::Static => draw_text_panel(hdc, 48, 48, dpi),
        Mode::Clock => {
            draw_text_panel(hdc, 48, 48, dpi);
            draw_clock(hdc, w, dpi, frame);
        }
        Mode::Scroll => {
            SaveDC(hdc);
            let _ = IntersectClipRect(hdc, 40, 40, 660, h - 72);
            let content = panel_height(dpi).max(1);
            let off = (frame % content as u64) as i32;
            draw_text_panel(hdc, 48, 48 - off, dpi);
            draw_text_panel(hdc, 48, 48 - off + content, dpi);
            let _ = RestoreDC(hdc, -1);
        }
    }
    draw_click_info(hdc, h, dpi, clicks, last);
}
