//! 帧抽象：BGRA 帧缓冲与 YUV（I420 / NV12 / I444）转换。
//!
//! - 源帧为 WGC 捕获侧常见的 BGRA 布局，支持任意源 stride；
//! - 颜色空间升级为 **BT.709**（与现代 PC 显示器 sRGB 原色及 WebCodecs HD/4K 缺省矩阵对齐，
//!   消除 BT.601→BT.709 绿偏/红偏色差）；默认优先 **Full Range（0..255）** 保留 100% 灰阶，
//!   对不支持 Full Range VUI 的硬编（如部分 MFT 驱动）可切 [`ColorProfile::Bt709Limited`]
//!   保持编解码范围严格一致；
//! - 编码输入宽高必须偶数：奇数时右/下各补 1 像素（边缘像素复制而非黑边，避免边缘绿线/暗边），
//!   padding 量随 `EncodedAu.crop` 下发；
//! - 转换使用 `dcv-color-primitives`（运行时按 CPU 指令集分派 SIMD）。

use anyhow::{Context, Result};

use dcv_color_primitives as dcv;

/// 编码色彩矩阵与动态范围配置。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorProfile {
    /// BT.709 全动态范围（0..255）：PC 桌面/代码文字首选，黑是纯黑、白是纯白，无灰阶损失。
    #[default]
    Bt709Full,
    /// BT.709 有限动态范围（Y 16..235, UV 16..240）：仅用于不支持 Full Range VUI 的硬编兜底。
    Bt709Limited,
}

impl ColorProfile {
    fn to_dcv(self) -> dcv::ColorSpace {
        match self {
            ColorProfile::Bt709Full => dcv::ColorSpace::Bt709FR,
            ColorProfile::Bt709Limited => dcv::ColorSpace::Bt709,
        }
    }

    /// 本色彩配置下纯黑对应的亮度平面（Y）初始值。
    pub const fn black_y(self) -> u8 {
        match self {
            ColorProfile::Bt709Full => 0,
            ColorProfile::Bt709Limited => 16,
        }
    }
}

/// 复制边缘像素填充奇数宽/高产生的 1 像素 padding（避免 4:2:0 色度采样在边缘混入黑线/绿线）。
fn pad_plane_edges(
    plane: &mut [u8],
    stride: usize,
    valid_w: usize,
    valid_h: usize,
    enc_w: usize,
    enc_h: usize,
) {
    if valid_w == 0 || valid_h == 0 {
        return;
    }
    if enc_w > valid_w {
        for row in 0..valid_h {
            let base = row * stride;
            let edge = plane[base + valid_w - 1];
            for col in valid_w..enc_w {
                plane[base + col] = edge;
            }
        }
    }
    if enc_h > valid_h {
        let src_row = (valid_h - 1) * stride;
        for row in valid_h..enc_h {
            let dst_row = row * stride;
            for col in 0..enc_w {
                plane[dst_row + col] = plane[src_row + col];
            }
        }
    }
}

/// BGRA 源帧（来自窗口捕获）。
#[derive(Clone, Debug)]
pub struct RawFrame {
    /// 逻辑像素宽。
    pub width: u32,
    /// 逻辑像素高。
    pub height: u32,
    /// 每行字节数，可以大于 `width * 4`（对齐行距）。
    pub stride: i32,
    /// 单调微秒时间戳（相对流起点）。
    pub pts_us: u64,
    /// BGRA 像素数据，长度必须 >= `stride * height`。
    pub data: Vec<u8>,
}

impl RawFrame {
    /// 行距紧凑的空帧（像素为 0，即黑色）。
    pub fn new(width: u32, height: u32, pts_us: u64) -> Self {
        let stride = (width * 4) as i32;
        Self {
            width,
            height,
            stride,
            pts_us,
            data: vec![0; stride as usize * height as usize],
        }
    }

    /// BGRA → I420（默认 BT.709 Full Range），写入 `dst`（其宽高必须为本帧宽高的偶数化几何）。
    pub fn bgra_to_i420_into(&self, dst: &mut YuvPlanes) -> Result<()> {
        self.bgra_to_i420_with(dst, ColorProfile::Bt709Full)
    }

    /// BGRA → I420（指定色彩配置），写入 `dst`。
    pub fn bgra_to_i420_with(&self, dst: &mut YuvPlanes, profile: ColorProfile) -> Result<()> {
        let (enc_w, enc_h) = (dst.width, dst.height);
        let src_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Bgra,
            color_space: dcv::ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::I420,
            color_space: profile.to_dcv(),
            num_planes: 3,
        };
        dcv::convert_image(
            self.width,
            self.height,
            &src_format,
            Some(&[self.stride as usize]),
            &[&self.data],
            &dst_format,
            Some(&[enc_w as usize, (enc_w / 2) as usize, (enc_w / 2) as usize]),
            &mut [&mut dst.y, &mut dst.u, &mut dst.v],
        )
        .map_err(|e| anyhow::anyhow!("BGRA->I420 转换失败 ({profile:?}): {e:?}"))
        .with_context(|| {
            format!(
                "帧 {w}x{h} stride {s} -> 编码尺寸 {ew}x{eh}",
                w = self.width,
                h = self.height,
                s = self.stride,
                ew = enc_w,
                eh = enc_h
            )
        })?;
        pad_plane_edges(
            &mut dst.y,
            enc_w as usize,
            self.width as usize,
            self.height as usize,
            enc_w as usize,
            enc_h as usize,
        );
        Ok(())
    }

    /// BGRA → NV12（默认 BT.709 Full Range），写入调用方提供的两个平面。
    pub fn bgra_to_nv12_into(
        &self,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
    ) -> Result<()> {
        self.bgra_to_nv12_with(y, y_stride, uv, uv_stride, ColorProfile::Bt709Full)
    }

    /// BGRA → NV12（指定色彩配置），写入调用方提供的两个平面。
    pub fn bgra_to_nv12_with(
        &self,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
        profile: ColorProfile,
    ) -> Result<()> {
        let src_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Bgra,
            color_space: dcv::ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Nv12,
            color_space: profile.to_dcv(),
            num_planes: 2,
        };
        dcv::convert_image(
            self.width,
            self.height,
            &src_format,
            Some(&[self.stride as usize]),
            &[&self.data],
            &dst_format,
            Some(&[y_stride, uv_stride]),
            &mut [y, uv],
        )
        .map_err(|e| anyhow::anyhow!("BGRA->NV12 转换失败 ({profile:?}): {e:?}"))?;
        let (enc_w, enc_h, _) = even_geometry(self.width, self.height);
        pad_plane_edges(
            y,
            y_stride,
            self.width as usize,
            self.height as usize,
            enc_w as usize,
            enc_h as usize,
        );
        Ok(())
    }

    /// BGRA → I444（yuv444p，4:4:4，默认 BT.709 Full Range）。
    pub fn bgra_to_yuv444_into(
        &self,
        y: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
        stride: usize,
    ) -> Result<()> {
        self.bgra_to_yuv444_with(y, u, v, stride, ColorProfile::Bt709Full)
    }

    /// BGRA → I444（yuv444p，4:4:4，指定色彩配置），写入三个紧凑平面。
    pub fn bgra_to_yuv444_with(
        &self,
        y: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
        stride: usize,
        profile: ColorProfile,
    ) -> Result<()> {
        let src_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Bgra,
            color_space: dcv::ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::I444,
            color_space: profile.to_dcv(),
            num_planes: 3,
        };
        dcv::convert_image(
            self.width,
            self.height,
            &src_format,
            Some(&[self.stride as usize]),
            &[&self.data],
            &dst_format,
            Some(&[stride, stride, stride]),
            &mut [y, u, v],
        )
        .map_err(|e| anyhow::anyhow!("BGRA->I444 转换失败 ({profile:?}): {e:?}"))
        .with_context(|| {
            format!(
                "帧 {w}x{h} stride {s} -> 行距 {st}",
                w = self.width,
                h = self.height,
                s = self.stride,
                st = stride
            )
        })?;
        let (enc_w, enc_h, _) = even_geometry(self.width, self.height);
        for plane in [y, u, v] {
            pad_plane_edges(
                plane,
                stride,
                self.width as usize,
                self.height as usize,
                enc_w as usize,
                enc_h as usize,
            );
        }
        Ok(())
    }
}

/// 编码几何：偶数化后的宽高与右/下 padding 像素数。
pub type Geometry = (u32, u32, (u32, u32));

/// 宽高向上取偶；返回 `(enc_w, enc_h, (右 padding, 下 padding))`。
pub fn even_geometry(width: u32, height: u32) -> Geometry {
    let enc_w = (width + 1) & !1;
    let enc_h = (height + 1) & !1;
    (enc_w, enc_h, (enc_w - width, enc_h - height))
}

/// I420 平面集（可直接作为 OpenH264 的 [`openh264::formats::YUVSource`]）。
#[derive(Clone, Debug)]
pub struct YuvPlanes {
    /// 平面宽（编码宽，偶数）。
    pub width: u32,
    /// 平面高（编码高，偶数）。
    pub height: u32,
    /// 亮度平面，长度 `width * height`，行距 `width`。
    pub y: Vec<u8>,
    /// 蓝色色度平面，长度 `(width/2) * (height/2)`，行距 `width/2`。
    pub u: Vec<u8>,
    /// 红色色度平面，长度 `(width/2) * (height/2)`，行距 `width/2`。
    pub v: Vec<u8>,
}

impl YuvPlanes {
    /// 分配并填充为 BT.601 有限范围黑色（Y=16，U=V=128）。
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            y: vec![16; width as usize * height as usize],
            u: vec![128; (width as usize / 2) * (height as usize / 2)],
            v: vec![128; (width as usize / 2) * (height as usize / 2)],
        }
    }

    /// 尺寸变化时重新分配，尺寸不变则复用缓冲。
    pub fn reset(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        *self = Self::new(width, height);
    }
}

impl openh264::formats::YUVSource for YuvPlanes {
    fn dimensions(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        let cw = self.width as usize / 2;
        (self.width as usize, cw, cw)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_geometry_pads_right_bottom() {
        assert_eq!(even_geometry(640, 480), (640, 480, (0, 0)));
        assert_eq!(even_geometry(641, 481), (642, 482, (1, 1)));
        assert_eq!(even_geometry(1, 1), (2, 2, (1, 1)));
    }

    #[test]
    fn bgra_to_i420_matches_bt709_full_reference() {
        // 纯红帧（BT.709 Full Range: Y≈54, U≈99, V≈255）。
        let mut frame = RawFrame::new(4, 4, 0);
        for px in frame.data.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        let mut planes = YuvPlanes::new(4, 4);
        frame.bgra_to_i420_into(&mut planes).expect("转换应成功");
        assert!(
            planes.y.iter().all(|&v| (49..=59).contains(&v)),
            "Y={:?}",
            planes.y
        );
        assert!(
            planes.u.iter().all(|&v| (94..=104).contains(&v)),
            "U={:?}",
            planes.u
        );
        assert!(
            planes.v.iter().all(|&v| (250..=255).contains(&v)),
            "V={:?}",
            planes.v
        );
    }

    #[test]
    fn bgra_to_nv12_fills_both_planes() {
        let mut frame = RawFrame::new(4, 4, 0);
        for px in frame.data.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[128, 128, 128, 255]);
        }
        let mut y = vec![0u8; 4 * 4];
        let mut uv = vec![0u8; 4 * 2];
        frame
            .bgra_to_nv12_into(&mut y, 4, &mut uv, 4)
            .expect("转换应成功");
        // 灰色: Y≈128（BT.709 Full Range），UV≈128。
        assert!(y.iter().all(|&v| (124..=132).contains(&v)));
        assert!(uv.iter().all(|&v| (124..=132).contains(&v)));
    }

    #[test]
    fn bgra_to_yuv444_pure_red_matches_bt709_full() {
        // 纯红帧（BT.709 Full Range: Y≈54, U≈99, V≈255）。
        let mut frame = RawFrame::new(4, 4, 0);
        for px in frame.data.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        let mut y = vec![0u8; 16];
        let mut u = vec![0u8; 16];
        let mut v = vec![0u8; 16];
        frame
            .bgra_to_yuv444_into(&mut y, &mut u, &mut v, 4)
            .expect("转换应成功");
        assert!(y.iter().all(|&c| (49..=59).contains(&c)));
        assert!(u.iter().all(|&c| (94..=104).contains(&c)));
        assert!(v.iter().all(|&c| (250..=255).contains(&c)));
    }

    #[test]
    fn bgra_to_yuv444_gradient_matches_bt709_full_reference() {
        // 混合渐变（横向灰阶 R、纵向蓝阶 B，G 恒 0）：逐像素对照 BT.709
        // Full Range 参考公式，允许 ±4（dcv 定点实现的舍入偏差）。
        const W: u32 = 16;
        const H: u32 = 16;
        let mut frame = RawFrame::new(W, H, 0);
        for (i, px) in frame.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let gray = (i % W as usize * 17) as u8; // R：0..=255 横向渐变
            let blue = (i / W as usize * 17) as u8; // B：0..=255 纵向渐变
            px.copy_from_slice(&[blue, 0, gray, 255]);
        }
        let n = (W * H) as usize;
        let mut y = vec![0u8; n];
        let mut u = vec![0u8; n];
        let mut v = vec![0u8; n];
        frame
            .bgra_to_yuv444_into(&mut y, &mut u, &mut v, W as usize)
            .expect("转换应成功");

        for (i, (&oy, (&ou, &ov))) in y.iter().zip(u.iter().zip(&v)).enumerate() {
            let r = (i % W as usize * 17) as f64;
            let b = (i / W as usize * 17) as f64;
            // BT.709 Full Range 参考式（Kr=0.2126, Kb=0.0722, Y/U/V 0..255）。
            let ref_y = (0.2126 * r + 0.0722 * b).clamp(0.0, 255.0);
            let ref_u = (128.0 + 0.5 * (b - ref_y) / (1.0 - 0.0722)).clamp(0.0, 255.0);
            let ref_v = (128.0 + 0.5 * (r - ref_y) / (1.0 - 0.2126)).clamp(0.0, 255.0);
            assert!(
                (f64::from(oy) - ref_y).abs() <= 4.0,
                "Y[{i}]={oy} 偏离参考值 {ref_y}"
            );
            assert!(
                (f64::from(ou) - ref_u).abs() <= 4.0,
                "U[{i}]={ou} 偏离参考值 {ref_u}"
            );
            assert!(
                (f64::from(ov) - ref_v).abs() <= 4.0,
                "V[{i}]={ov} 偏离参考值 {ref_v}"
            );
        }
    }
}
