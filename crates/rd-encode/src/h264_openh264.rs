//! H.264 软编兜底（OpenH264，BSD-2-Clause，源码静态编译进二进制）。
//!
//! 参数：Constrained Baseline（OpenH264 `PRO_BASELINE` 即输出 Constrained
//! Baseline）、无 B 帧（OpenH264 本身不产 B 帧）、码率模式
//! （RC_BITRATE_MODE，可运行中调整）、关键帧间隔无限（首帧 IDR，其余按需）。
//!
//! 与任务卡配置的唯一偏差：`usage` 用 `CameraVideoRealTime` 而非
//! `ScreenContentRealTime`——实测 1080p 单帧软编 ScreenContent 档约
//! 28-33 ms（本机单线程，且 OpenH264 屏幕内容档不支持多线程、强制开
//! 场景切换检测），超出 DoD 的 20 ms 上限；Camera 档 + 4 线程实测
//! 平均 12 ms。文字清晰度主要受码率与 4:2:0 采样影响，两者相同，
//! 静止区域的精细还原由 T-010 无损瓦片负责。
//!
//! 尺寸变化通过重建参数实现（openh264 crate 在输入尺寸变化时自动 reinit，
//! reinit 后首帧为 IDR）；`force_key` / `reconfigure` 都走 `force_intra_frame`。

use std::os::raw::c_void;

use anyhow::{bail, Context, Result};
use openh264::encoder::{
    BitRate, Encoder as Oh264Encoder, EncoderConfig as Oh264Config, FrameType, Profile,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264_sys2::{
    SBitrateInfo, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0,
};

use crate::frame::{even_geometry, RawFrame, YuvPlanes};
use crate::{EncodedAu, EncoderConfig, EncoderInfo, PixelFormat, VideoEncoder};

/// OpenH264 兜底编码器的固定标识。
pub const INFO: EncoderInfo = EncoderInfo {
    name: "openh264",
    hardware: false,
};

/// OpenH264 软编码器。
pub struct OpenH264Encoder {
    inner: Oh264Encoder,
    /// `reconfigure` 设置的覆盖尺寸；未设置时按输入帧偶数化。
    override_dims: Option<(u32, u32)>,
    /// `reconfigure` 后下一帧强制 IDR（与尺寸无关，保证语义稳定）。
    force_idr_pending: bool,
    /// I420 复用缓冲。
    yuv: YuvPlanes,
}

impl OpenH264Encoder {
    /// 创建 OpenH264 编码器（编译期静态链接，创建即验证可用性）。
    ///
    /// # Errors
    /// 请求 [`PixelFormat::Yuv444`] 时明确报错——OpenH264 仅支持 4:2:0，
    /// 不做静默降级（回退决策归调用方）。
    pub fn new(cfg: &EncoderConfig) -> Result<Self> {
        if cfg.pixel_format == PixelFormat::Yuv444 {
            bail!("OpenH264 不支持 4:4:4（仅 4:2:0）；请改用支持 444 的硬编或回退 Yuv420");
        }
        let (enc_w, enc_h, _) = even_geometry(cfg.width, cfg.height);
        let config = Oh264Config::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .profile(Profile::Baseline)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(cfg.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(cfg.fps as f32))
            // 远程桌面不丢帧：带宽自适应由上层（T-011）调码率/QP 完成。
            .skip_frames(false)
            // 低复杂度档 + 关场景切换检测：IDR 完全按需
            // （force_key / reconfigure），避免误判全屏 IDR。
            .complexity(openh264::encoder::Complexity::Low)
            .num_threads(4)
            .scene_change_detect(false)
            .sps_pps_strategy(openh264::encoder::SpsPpsStrategy::ConstantId)
            .vui(VuiConfig::bt601());
        let inner = Oh264Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("OpenH264 编码器初始化失败")?;
        Ok(Self {
            inner,
            override_dims: None,
            force_idr_pending: false,
            yuv: YuvPlanes::new(enc_w, enc_h),
        })
    }

    /// 当前帧使用的编码几何：优先 reconfigure 覆盖值，否则偶数化输入帧。
    fn effective_geometry(&self, frame: &RawFrame) -> Result<(u32, u32, (u32, u32))> {
        let (enc_w, enc_h) = self.override_dims.unwrap_or((frame.width, frame.height));
        let (enc_w, enc_h) = {
            let (w, h, _) = even_geometry(enc_w, enc_h);
            (w, h)
        };
        let crop_w = enc_w
            .checked_sub(frame.width)
            .context("帧宽大于编码宽（reconfigure 尺寸与输入帧不匹配）")?;
        let crop_h = enc_h
            .checked_sub(frame.height)
            .context("帧高大于编码高（reconfigure 尺寸与输入帧不匹配）")?;
        Ok((enc_w, enc_h, (crop_w, crop_h)))
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn encode(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedAu>> {
        let (enc_w, enc_h, crop) = self.effective_geometry(frame)?;
        self.yuv.reset(enc_w, enc_h);
        frame.bgra_to_i420_into(&mut self.yuv)?;

        if force_key || self.force_idr_pending {
            self.inner.force_intra_frame();
            self.force_idr_pending = false;
        }

        // OpenH264 时间戳单位为毫秒。
        let ts = Timestamp::from_millis(frame.pts_us / 1000);
        let bitstream = self
            .inner
            .encode_at(&self.yuv, ts)
            .context("OpenH264 编码失败")?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedAu {
            data,
            keyframe,
            pts_us: frame.pts_us,
            crop,
        }))
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        // 运行中调整码率：SetOption(ENCODER_OPTION_BITRATE / _MAX_BITRATE)。
        // 必须作用于 SPATIAL_LAYER_0 且先设 MAX 再设 BITRATE——OpenH264 校验
        // "层 MaxSpatialBitrate >= SpatialBitrate"，只改 BITRATE 且升码率时会
        // 因 max 未同步被拒绝（cmInitParaError）。
        let bps = kbps.saturating_mul(1000) as std::os::raw::c_int;
        let mut max_info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_0,
            iBitrate: bps,
        };
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_0,
            iBitrate: bps,
        };
        // SAFETY: 两个 info 均为合法的 SBitrateInfo，编码器在 encode 时已
        // 初始化；OpenH264 仅在调用期间读取这些指针。
        unsafe {
            let api = self.inner.raw_api();
            let rc = api.set_option(
                ENCODER_OPTION_MAX_BITRATE,
                (&mut max_info as *mut SBitrateInfo).cast::<c_void>(),
            );
            if rc != 0 {
                bail!("OpenH264 SetOption(MAX_BITRATE) 失败: rc={rc}");
            }
            let rc = api.set_option(
                ENCODER_OPTION_BITRATE,
                (&mut info as *mut SBitrateInfo).cast::<c_void>(),
            );
            if rc != 0 {
                bail!("OpenH264 SetOption(BITRATE) 失败: rc={rc}");
            }
        }
        Ok(())
    }

    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        // openh264 crate 在下一次 encode 检测到输入尺寸变化时自动重建编码器，
        // 重建后首帧即 IDR；这里记录覆盖尺寸并显式标记下一帧 IDR（即使
        // 尺寸没变也保证 IDR 语义）。
        let (enc_w, enc_h, _) = even_geometry(width, height);
        self.override_dims = Some((enc_w, enc_h));
        self.yuv.reset(enc_w, enc_h);
        self.force_idr_pending = true;
        Ok(())
    }

    fn info(&self) -> EncoderInfo {
        INFO
    }
}
