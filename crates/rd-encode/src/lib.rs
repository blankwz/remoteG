//! rd-encode — 编码器抽象、H.264（OpenH264 / ffmpeg 硬编）、脏区跟踪与 QOI+zstd 无损瓦片。
//!
//! T-003 范围：[`VideoEncoder`] 抽象 + [`create_best`] 选路
//! （硬编 NVENC/QSV/AMF/MF 依次探测，失败回退 OpenH264）、
//! BGRA→I420/NV12 转换（[`frame`]）、Annex-B 输出。
//!
//! ```no_run
//! use rd_encode::{EncoderConfig, VideoEncoder, create_best, frame::RawFrame};
//!
//! let cfg = EncoderConfig { width: 1920, height: 1080, fps: 60, bitrate_kbps: 4000, prefer_hw: true };
//! let mut enc = create_best(&cfg).expect("至少 OpenH264 可用");
//! let frame = RawFrame::new(1920, 1080, 0);
//! if let Some(au) = enc.encode(&frame, false).expect("编码成功") {
//!     assert_eq!(au.crop, (0, 0)); // Annex-B 数据 + 奇数尺寸时的裁剪信息
//! }
//! ```

pub mod caps;
pub mod dirty;
pub mod frame;
#[cfg(all(windows, feature = "ffmpeg"))]
pub mod h264_ffmpeg;
pub mod h264_openh264;
pub mod rate;
pub mod tiles;

use anyhow::Result;

use crate::frame::RawFrame;

/// 编码器创建参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    /// 窗口客户区宽（奇数会在编码侧补 1 像素）。
    pub width: u32,
    /// 窗口客户区高（奇数会在编码侧补 1 像素）。
    pub height: u32,
    /// 目标帧率。
    pub fps: u32,
    /// 目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// true：优先探测硬件编码器；false：直接使用 OpenH264。
    pub prefer_hw: bool,
}

/// 一个访问单元（H.264 Annex-B）。
///
/// `data` 以 start code `00 00 00 01` 分隔 NAL，可直接送 WebCodecs
/// `annexb` 格式解码。奇数宽高时编码侧右/下补 1 像素，解码端按
/// `crop` 裁剪后显示。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncodedAu {
    /// Annex-B 比特流。
    pub data: Vec<u8>,
    /// 是否为关键帧（IDR，可独立解码）。
    pub keyframe: bool,
    /// 微秒时间戳（透传输入帧 `pts_us`）。
    pub pts_us: u64,
    /// 右/下 padding 像素数（奇数尺寸时为 (1, 1) 量级，解码端裁剪）。
    pub crop: (u32, u32),
}

/// 编码器标识（用于日志与控制端 HUD）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderInfo {
    /// 编码器名称，如 `h264_nvenc` / `openh264`。
    pub name: &'static str,
    /// 是否硬件编码。
    pub hardware: bool,
}

/// 统一视频编码器接口（低延迟 H.264，输入 BGRA，输出 Annex-B AU）。
pub trait VideoEncoder: Send {
    /// 编码一帧。
    ///
    /// `force_key` 请求下一输出为 IDR（丢包恢复/新观众加入时使用）。
    /// 返回 `None` 表示本帧无输出（如编码器内部缓冲未满）。
    ///
    /// # Errors
    /// 编码器内部错误（输入尺寸不匹配、硬编设备故障等）。
    fn encode(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedAu>>;

    /// 运行中调整目标码率（kbps）。
    ///
    /// # Errors
    /// 编码器拒绝该码率或重建失败。
    fn set_bitrate(&mut self, kbps: u32) -> Result<()>;

    /// 尺寸变化（窗口 resize）；下一帧必须是 IDR。
    ///
    /// # Errors
    /// 新尺寸超出编码器能力。
    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()>;

    /// 编码器标识。
    fn info(&self) -> EncoderInfo;
}

/// 按策略创建最优编码器：
///
/// 1. `prefer_hw` 且启用 `ffmpeg` feature：按
///    `h264_nvenc` → `h264_qsv` → `h264_amf` → `h264_mf` 探测
///    （每个候选实际编码 1 帧验证），首个成功者胜出；
/// 2. 否则/全部失败：OpenH264 软编兜底（静态编译，始终可用）。
///
/// 编码器选择会通过 `tracing` 记录 [`EncoderInfo`]。
///
/// # Errors
/// 仅当 OpenH264 也初始化失败时返回错误（理论上不应发生）。
pub fn create_best(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    if cfg.prefer_hw {
        #[cfg(all(windows, feature = "ffmpeg"))]
        match h264_ffmpeg::FfmpegEncoder::create(cfg, None) {
            Ok(enc) => {
                let info = enc.info();
                tracing::info!(name = info.name, hardware = info.hardware, "选用视频编码器");
                return Ok(Box::new(enc));
            }
            Err(e) => {
                tracing::warn!("硬编探测全部失败，回退 OpenH264: {e:#}");
            }
        }
        #[cfg(not(all(windows, feature = "ffmpeg")))]
        tracing::debug!("未启用 ffmpeg 硬编（非 Windows 或未启用 feature），跳过硬编探测");
    }
    let enc = h264_openh264::OpenH264Encoder::new(cfg)?;
    let info = enc.info();
    tracing::info!(name = info.name, hardware = info.hardware, "选用视频编码器");
    Ok(Box::new(enc))
}
