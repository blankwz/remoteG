//! 帧抽象：BGRA 帧缓冲与 YUV（I420 / NV12）转换。
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
}
