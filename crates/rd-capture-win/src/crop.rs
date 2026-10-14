//! 裁剪：从捕获纹理中取窗口客户区。
//!
//! WGC 窗口捕获的帧对应窗口的**视觉边界**（即 `DWMWA_EXTENDED_FRAME_BOUNDS`
//! 区域，不含 DWM 投影阴影）。客户区在该区域内的偏移 = 客户区屏幕原点 −
//! 视觉边界屏幕原点，两者均为物理像素，与帧尺寸同一坐标系。

/// 帧内矩形（原点为帧左上角；`dirty` 场景下原点为客户区左上角，见调用方文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// 计算客户区在捕获帧内的矩形。
///
/// - `frame_size`：捕获帧尺寸（物理像素）。
/// - `frame_bounds_origin`：窗口视觉边界（`DWMWA_EXTENDED_FRAME_BOUNDS`）左上角
///   在屏幕上的物理像素坐标。
/// - `client_origin`：客户区左上角在屏幕上的物理像素坐标（`GetClientRect` +
///   `ClientToScreen`，需在 per-monitor DPI aware 上下文调用）。
/// - `client_size`：客户区尺寸（物理像素）。
///
/// 客户区完全落在帧外（最小化 / 已销毁 / 帧为空）返回 `None`；
/// 客户区部分超出帧则裁剪到帧内。DPI 缩放由调用方换算为物理像素后传入，
/// 本函数与 DPI 无关。
pub fn client_rect_in_frame(
    frame_size: (u32, u32),
    frame_bounds_origin: (i32, i32),
    client_origin: (i32, i32),
    client_size: (u32, u32),
) -> Option<Rect> {
    if frame_size.0 == 0 || frame_size.1 == 0 || client_size.0 == 0 || client_size.1 == 0 {
        return None;
    }
    let x = (client_origin.0 - frame_bounds_origin.0).max(0);
    let y = (client_origin.1 - frame_bounds_origin.1).max(0);
    if x as u64 >= frame_size.0 as u64 || y as u64 >= frame_size.1 as u64 {
        return None;
    }
    let avail_w = frame_size.0 - x as u32;
    let avail_h = frame_size.1 - y as u32;
    let w = client_size.0.min(avail_w);
    let h = client_size.1.min(avail_h);
    if w == 0 || h == 0 {
        return None;
    }
    Some(Rect { x, y, w, h })
}
