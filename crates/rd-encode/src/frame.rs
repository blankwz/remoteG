//! 帧抽象：BGRA 帧缓冲与 YUV（I420 / NV12 / I444）转换。
//!
//! - 源帧为 WGC 捕获侧常见的 BGRA 布局，支持任意源 stride；
//! - 颜色空间统一 BT.601 有限范围（H.264 未携带 VUI 时播放器的缺省假设，
//!   OpenH264 路径显式写入 VUI BT.601）；
//! - 编码输入宽高必须偶数：奇数时右/下各补 1 像素，padding 量随
//!   `EncodedAu.crop` 下发，由解码端裁掉；
//! - 转换使用 `dcv-color-primitives`（运行时按 CPU 指令集分派 SIMD）。

use anyhow::{Context, Result};

use dcv_color_primitives as dcv;

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

    /// BGRA → I420，写入 `dst`（其宽高必须为本帧宽高的偶数化几何）。
    pub fn bgra_to_i420_into(&self, dst: &mut YuvPlanes) -> Result<()> {
        let (enc_w, enc_h) = (dst.width, dst.height);
        let src_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Bgra,
            color_space: dcv::ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::I420,
            color_space: dcv::ColorSpace::Bt601,
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
        .map_err(|e| anyhow::anyhow!("BGRA->I420 转换失败: {e:?}"))
        .with_context(|| {
            format!(
                "帧 {w}x{h} stride {s} -> 编码尺寸 {ew}x{eh}",
                w = self.width,
                h = self.height,
                s = self.stride,
                ew = enc_w,
                eh = enc_h
            )
        })
    }

    /// BGRA → NV12，写入调用方提供的两个平面（Y 平面 + 交错 UV 平面，
    /// 可带自定义 stride，典型用途是直写 ffmpeg 帧缓冲避免拷贝）。
    pub fn bgra_to_nv12_into(
        &self,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
    ) -> Result<()> {
        let src_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Bgra,
            color_space: dcv::ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Nv12,
            color_space: dcv::ColorSpace::Bt601,
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
        .map_err(|e| anyhow::anyhow!("BGRA->NV12 转换失败: {e:?}"))
    }

    /// BGRA → I444（yuv444p，4:4:4），写入三个紧凑平面（行距均为 `stride`，
    /// 典型用途是直写编码器暂存缓冲；与 4:2:0 不同，U/V 平面与 Y 同尺寸，
    /// 每像素独立 Y/U/V，无色度下采样）。
    ///
    /// dcv-color-primitives 1.0.0 注册了 `Bgra → I444 (Bt601)` 全指令集
    /// 分派（含标量回退），无需手写转换。
    pub fn bgra_to_yuv444_into(
        &self,
        y: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
        stride: usize,
    ) -> Result<()> {
        let src_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Bgra,
            color_space: dcv::ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::I444,
            color_space: dcv::ColorSpace::Bt601,
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
        .map_err(|e| anyhow::anyhow!("BGRA->I444 转换失败: {e:?}"))
        .with_context(|| {
            format!(
                "帧 {w}x{h} stride {s} -> 行距 {st}",
                w = self.width,
                h = self.height,
                s = self.stride,
                st = stride
            )
        })
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
    fn bgra_to_i420_matches_bt601_reference() {
        // 纯红帧（BT.601 有限范围: Y=81, U=90, V=240）。
        let mut frame = RawFrame::new(4, 4, 0);
        for px in frame.data.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        let mut planes = YuvPlanes::new(4, 4);
        frame.bgra_to_i420_into(&mut planes).expect("转换应成功");
        assert!(planes.y.iter().all(|&v| (76..=86).contains(&v)));
        assert!(planes.u.iter().all(|&v| (85..=95).contains(&v)));
        assert!(planes.v.iter().all(|&v| (235..=245).contains(&v)));
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
        // 灰色: Y≈126..136（BT.601 有限范围），UV≈128。
        assert!(y.iter().all(|&v| (121..=131).contains(&v)));
        assert!(uv.iter().all(|&v| (123..=133).contains(&v)));
    }

    #[test]
    fn bgra_to_yuv444_pure_red_matches_bt601() {
        // 纯红帧（BT.601 有限范围: Y=81, U=90, V=240）。444 的 U/V 平面与
        // Y 同尺寸，每个像素三平面独立取值（无色度下采样）。
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
        assert!(y.iter().all(|&c| (76..=86).contains(&c)));
        assert!(u.iter().all(|&c| (85..=95).contains(&c)));
        assert!(v.iter().all(|&c| (235..=245).contains(&c)));
    }

    #[test]
    fn bgra_to_yuv444_gradient_matches_bt601_reference() {
        // 混合渐变（横向灰阶 R、纵向蓝阶 B，G 恒 0）：逐像素对照 BT.601
        // 有限范围参考公式，允许 ±3（dcv 定点实现的舍入偏差）。
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
            // BT.601 有限范围参考式（Y 16..235，U/V 16..240）。
            let ref_y = 16.0 + (65.481 * r + 128.553 * 0.0 + 24.966 * b) / 255.0;
            let ref_u = 128.0 + (-37.797 * r - 74.203 * 0.0 + 112.0 * b) / 255.0;
            let ref_v = 128.0 + (112.0 * r - 93.786 * 0.0 - 18.214 * b) / 255.0;
            assert!(
                (f64::from(oy) - ref_y).abs() <= 3.0,
                "Y[{i}]={oy} 偏离参考值 {ref_y}"
            );
            assert!(
                (f64::from(ou) - ref_u).abs() <= 3.0,
                "U[{i}]={ou} 偏离参考值 {ref_u}"
            );
            assert!(
                (f64::from(ov) - ref_v).abs() <= 3.0,
                "V[{i}]={ov} 偏离参考值 {ref_v}"
            );
        }
    }
}
