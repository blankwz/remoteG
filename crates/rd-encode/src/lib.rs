//! rd-encode — 编码器抽象、H.264（OpenH264 / ffmpeg 硬编）、脏区跟踪与 QOI+zstd 无损瓦片。
//!
//! T-003 范围：[`VideoEncoder`] 抽象 + [`create_best`] 选路
//! （硬编 NVENC/QSV/AMF/MF 依次探测，失败回退 OpenH264）、
//! BGRA→I420/NV12 转换（[`frame`]）、Annex-B 输出。
//!
//! ```no_run
//! use rd_encode::{EncoderConfig, VideoEncoder, create_best, frame::RawFrame};
//!
//! let cfg = EncoderConfig {
//!     width: 1920,
//!     height: 1080,
//!     fps: 60,
//!     bitrate_kbps: 4000,
//!     prefer_hw: true,
//!     pixel_format: rd_encode::PixelFormat::default(),
//! };
//! let mut enc = create_best(&cfg).expect("至少 OpenH264 可用");
//! let frame = RawFrame::new(1920, 1080, 0);
//! if let Some(au) = enc.encode(&frame, false).expect("编码成功") {
//!     assert_eq!(au.crop, (0, 0)); // Annex-B 数据 + 奇数尺寸时的裁剪信息
//! }
//! ```

pub mod caps;
#[cfg(all(windows, feature = "ffmpeg"))]
pub mod d3d11_zero_copy;
pub mod dirty;
pub mod frame;
#[cfg(all(windows, feature = "ffmpeg"))]
pub mod h264_ffmpeg;
pub mod h264_openh264;
pub mod rate;
pub mod tiles;

use anyhow::Result;

use crate::frame::RawFrame;

/// 编码输入色度采样格式（D-011 T-027 遗留：4:4:4 创建接口）。
///
/// [`PixelFormat::Yuv444`] 仅部分硬编支持（由 `caps::probe_encode_caps`
/// 预先探测）；`create_best` 在 444 请求下只探测 444 硬编候选且**不做
/// 静默回退**——是否降级 420 由调用方（协商层）决策。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4:2:0（NV12/I420）。全平台可用，默认。
    #[default]
    Yuv420,
    /// 4:4:4（yuv444p，H.264 High 4:4:4 Predictive）。仅限支持的硬编。
    Yuv444,
}

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
    /// 编码输入色度采样（默认 4:2:0）。
    pub pixel_format: PixelFormat,
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

/// GPU 显存内捕获帧（Windows D3D11 零拷贝直通硬编输入）。
#[cfg(windows)]
#[derive(Clone)]
pub struct GpuFrame {
    /// 源 BGRA8 `ID3D11Texture2D`（与编码器共享同一 `ID3D11Device`）。
    pub texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    /// 源纹理内客户区裁剪矩形 `(x, y, width, height)`。
    pub crop: (u32, u32, u32, u32),
    /// 微秒时间戳。
    pub pts_us: u64,
}

/// 统一视频编码器接口（低延迟 H.264/HEVC/AV1，输入 BGRA 或 GPU D3D11 纹理，输出 Annex-B AU）。
pub trait VideoEncoder: Send {
    /// 编码一帧（CPU BGRA 路径）。
    ///
    /// `force_key` 请求下一输出为 IDR（丢包恢复/新观众加入时使用）。
    /// 返回 `None` 表示本帧无输出（如编码器内部缓冲未满）。
    ///
    /// # Errors
    /// 编码器内部错误（输入尺寸不匹配、硬编设备故障等）。
    fn encode(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedAu>>;

    /// 是否支持直接接收 GPU D3D11 纹理（零拷贝硬编路径）。
    fn supports_gpu_input(&self) -> bool {
        false
    }

    /// 零拷贝编码一帧 GPU D3D11 纹理（仅 `supports_gpu_input() == true` 时可用）。
    #[cfg(windows)]
    fn encode_gpu(&mut self, _frame: &GpuFrame, _force_key: bool) -> Result<Option<EncodedAu>> {
        anyhow::bail!("当前编码器不支持 GPU 零拷贝输入")
    }

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
/// - [`PixelFormat::Yuv420`]（默认）：
///   1. `prefer_hw` 且启用 `ffmpeg` feature：按
///      `h264_nvenc` → `h264_qsv` → `h264_amf` → `h264_mf` 探测
///      （每个候选实际编码 1 帧验证），首个成功者胜出；
///   2. 否则/全部失败：OpenH264 软编兜底（静态编译，始终可用）。
/// - [`PixelFormat::Yuv444`]：只探测 444 硬编候选（同样顺序，以
///   `yuv444p` 输入实际编码 1 帧验证），全部失败即返回明确错误——
///   **不静默回退 420**（回退决策归调用方，OpenH264 不支持 4:4:4）。
///
/// 编码器选择会通过 `tracing` 记录 [`EncoderInfo`]。
///
/// # Errors
/// 420 路径：仅当 OpenH264 也初始化失败时返回错误（理论上不应发生）。
/// 444 路径：所有硬编候选不支持 4:4:4、`prefer_hw=false` 或构建未启用
/// ffmpeg 硬编时返回明确错误。
pub fn create_best(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    match cfg.pixel_format {
        PixelFormat::Yuv444 => create_best_444(cfg),
        PixelFormat::Yuv420 => create_best_420(cfg),
    }
}

/// 按协商出的 [`rd_proto::v0::Codec`] 创建硬编编码器（支持 AV1 / HEVC / H.264 4:4:4 / H.264 4:2:0）。
/// AV1 / HEVC 仅支持硬编，若本机不支持则返回明确错误供调用方降级 H.264。
pub fn create_for_codec(
    cfg: &EncoderConfig,
    codec: rd_proto::v0::Codec,
) -> Result<Box<dyn VideoEncoder>> {
    match codec {
        rd_proto::v0::Codec::H264420 => create_best_420(cfg),
        rd_proto::v0::Codec::H264444 => create_best_444(cfg),
        rd_proto::v0::Codec::Hevc444 | rd_proto::v0::Codec::Av1 => {
            anyhow::ensure!(cfg.prefer_hw, "{codec:?} 仅支持硬件编码器");
            #[cfg(all(windows, feature = "ffmpeg"))]
            {
                let enc = h264_ffmpeg::FfmpegEncoder::create_for_codec(cfg, codec)?;
                let info = enc.info();
                tracing::info!(
                    name = info.name,
                    hardware = info.hardware,
                    codec = ?codec,
                    "选用高刷/4K 硬件编码器"
                );
                Ok(Box::new(enc))
            }
            #[cfg(not(all(windows, feature = "ffmpeg")))]
            Err(anyhow::anyhow!(
                "当前构建未启用 ffmpeg 硬编，无法创建 {codec:?} 编码器"
            ))
        }
    }
}

/// 尝试创建基于 D3D11 显存零拷贝的硬件编码器（WGC `ID3D11Texture2D` → `ID3D11VideoProcessor`
/// BGRA→NV12 → FFmpeg `AV_PIX_FMT_D3D11` 硬编，全程无 CPU 读回与色彩转换）。
///
/// 仅支持 4:2:0 编码（H.264 4:2:0 / HEVC / AV1）。若硬件、驱动或当前编码器不支持 D3D11VA
/// 硬件帧输入，则返回 `Err`，调用方应无缝回退至 [`create_for_codec`]。
#[cfg(all(windows, feature = "ffmpeg"))]
pub fn create_d3d11_zero_copy(
    cfg: &EncoderConfig,
    codec: rd_proto::v0::Codec,
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> Result<Box<dyn VideoEncoder>> {
    anyhow::ensure!(
        cfg.prefer_hw && cfg.pixel_format == PixelFormat::Yuv420,
        "D3D11 零拷贝仅支持 prefer_hw=true 且 4:2:0 模式"
    );
    anyhow::ensure!(
        codec != rd_proto::v0::Codec::H264444,
        "D3D11 零拷贝暂不支持 H.264 4:4:4"
    );
    let enc = d3d11_zero_copy::D3d11HwEncoder::create(cfg, codec, device)?;
    let info = enc.info();
    tracing::info!(
        name = info.name,
        hardware = info.hardware,
        codec = ?codec,
        "选用 D3D11 GPU 零拷贝硬件编码器"
    );
    Ok(Box::new(enc))
}

/// 4:2:0 创建路径（硬编探测失败回退 OpenH264）。
fn create_best_420(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
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

/// 4:4:4 创建路径：只探测支持 444 的硬编候选，全败明确报错，不回退
/// OpenH264（其仅支持 4:2:0）。回退 420 与否由调用方决策。
fn create_best_444(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    anyhow::ensure!(
        cfg.prefer_hw,
        "Yuv444 需要硬件编码器（prefer_hw=false 恒选 OpenH264，其不支持 4:4:4）"
    );
    #[cfg(all(windows, feature = "ffmpeg"))]
    match h264_ffmpeg::FfmpegEncoder::create(cfg, None) {
        Ok(enc) => {
            let info = enc.info();
            tracing::info!(
                name = info.name,
                hardware = info.hardware,
                "选用 4:4:4 硬件编码器"
            );
            Ok(Box::new(enc))
        }
        Err(e) => Err(anyhow::anyhow!(
            "4:4:4 硬编探测全部失败（OpenH264 不支持 4:4:4，不做静默回退，请由调用方决策降级 420）: {e:#}"
        )),
    }
    #[cfg(not(all(windows, feature = "ffmpeg")))]
    Err(anyhow::anyhow!(
        "当前构建未启用 ffmpeg 硬编（非 Windows 或未启用 feature），无法创建 4:4:4 编码器（OpenH264 不支持 4:4:4）"
    ))
}
