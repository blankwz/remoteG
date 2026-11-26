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

use std::os::raw::{c_int, c_void};

use anyhow::{bail, Context, Result};
use openh264::encoder::{
    BitRate, Encoder as Oh264Encoder, EncoderConfig as Oh264Config, FrameType, Level, Profile,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264_sys2::{
    SBitrateInfo, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_TRACE_LEVEL,
    SPATIAL_LAYER_0, SPATIAL_LAYER_ALL, WELS_LOG_QUIET,
};

use crate::frame::{even_geometry, RawFrame, YuvPlanes};
use crate::{EncodedAu, EncoderConfig, EncoderInfo, PixelFormat, VideoEncoder};

/// OpenH264 Level 5.2 安全码率上限（bps）：支持 LAN 200 Mbps 极限档位且不超 Level 5.2 (240 Mbps)。
const OH264_MAX_BPS: u32 = 200_000_000;
const OH264_MIN_BPS: u32 = 100_000;

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
    /// 最近一次已送入 `encode_at` 的尺寸（检测 openh264 自动 reinit 后同步最新码率）。
    encoded_dims: Option<(u32, u32)>,
    /// 当前生效目标码率（bps）。
    current_bps: u32,
    /// 首帧 `encode_at` 前暂存的待应用码率（openh264 在首帧才调用 `InitializeExt`）。
    pending_bps: Option<u32>,
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
        let init_bps = cfg
            .bitrate_kbps
            .saturating_mul(1000)
            .clamp(OH264_MIN_BPS, OH264_MAX_BPS);
        // 显式指定 Level 5.2（支持最高 4K 与 240 Mbps MaxBR，避免默认 Level 4.0 在 >14~20 Mbps 时拒绝调参）
        // 并关闭 C++ 控制台日志。
        let config = Oh264Config::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .profile(Profile::Baseline)
            .level(Level::Level_5_2)
            .debug(false)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(init_bps))
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(
                (cfg.fps.clamp(1, 144)) as f32,
            ))
            // 远程桌面不丢帧：带宽自适应由上层（T-011）调码率/QP 完成。
            .skip_frames(false)
            // 低复杂度档 + 关场景切换检测：IDR 完全按需
            // （force_key / reconfigure），避免误判全屏 IDR。
            .complexity(openh264::encoder::Complexity::Low)
            .num_threads(4)
            .scene_change_detect(false)
            .sps_pps_strategy(openh264::encoder::SpsPpsStrategy::ConstantId)
            // BT.709 Full Range（0..255）：对齐桌面 sRGB 原色与 frame.rs 色彩转换矩阵，
            // 彻底消除 BT.601→BT.709 偏色与 16..235 灰阶发灰。
            .vui(VuiConfig::bt709_full());
        let mut inner = Oh264Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("OpenH264 编码器初始化失败")?;
        // openh264 crate 在首帧 InitializeExt 之后才设 TRACE_LEVEL，导致 InitializeExt 期间会向
        // stderr 打印 `Warning:bEnableFrameSkip = 0...`；此处在创建期先行静默 C++ trace。
        unsafe {
            let mut quiet: c_int = WELS_LOG_QUIET as c_int;
            let _ = inner.raw_api().set_option(
                ENCODER_OPTION_TRACE_LEVEL,
                (&mut quiet as *mut c_int).cast::<c_void>(),
            );
        }
        Ok(Self {
            inner,
            override_dims: None,
            encoded_dims: None,
            current_bps: init_bps,
            pending_bps: None,
            force_idr_pending: false,
            yuv: YuvPlanes::new(enc_w, enc_h),
        })
    }

    /// 将目标码率（bps）按合法顺序写入已初始化的 OpenH264 实例。
    ///
    /// 关键不变量：OpenH264 在每次 `SetOption` 时都会立即校验
    /// `SpatialBitrate <= MaxSpatialBitrate <= TotalMaxBitrate` 以及 Level MaxBR：
    /// - **升码率**时必须先升 `MAX_BITRATE`（ALL → LAYER_0）再升 `BITRATE`；
    /// - **降码率**时必须先降 `BITRATE`（LAYER_0 → ALL）再降 `MAX_BITRATE`
    ///   （此前无论升降都先设 `MAX_BITRATE`，导致从 20Mbps 降到 15855kbps 时必报 `rc=1`！）。
    fn apply_initialized_bps(&mut self, target_bps: u32) -> Result<()> {
        let bps = target_bps.clamp(OH264_MIN_BPS, OH264_MAX_BPS) as c_int;
        let max_bps = (OH264_MAX_BPS as c_int).max(bps);
        unsafe {
            let api = self.inner.raw_api();
            // 先把全局与单层 MAX_BITRATE 拉到安全上限，彻底消除升降顺序冲突
            for layer in [SPATIAL_LAYER_ALL, SPATIAL_LAYER_0] {
                let mut max_info = SBitrateInfo {
                    iLayer: layer,
                    iBitrate: max_bps,
                };
                let _ = api.set_option(
                    ENCODER_OPTION_MAX_BITRATE,
                    (&mut max_info as *mut SBitrateInfo).cast::<c_void>(),
                );
            }
            // 再设置目标 BITRATE（单层 + 全局）
            let mut layer_info = SBitrateInfo {
                iLayer: SPATIAL_LAYER_0,
                iBitrate: bps,
            };
            let rc = api.set_option(
                ENCODER_OPTION_BITRATE,
                (&mut layer_info as *mut SBitrateInfo).cast::<c_void>(),
            );
            if rc != 0 {
                bail!("OpenH264 SetOption(BITRATE, bps={bps}) 失败: rc={rc}");
            }
            let mut all_info = SBitrateInfo {
                iLayer: SPATIAL_LAYER_ALL,
                iBitrate: bps,
            };
            let _ = api.set_option(
                ENCODER_OPTION_BITRATE,
                (&mut all_info as *mut SBitrateInfo).cast::<c_void>(),
            );
        }
        self.current_bps = target_bps.clamp(OH264_MIN_BPS, OH264_MAX_BPS);
        Ok(())
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

        let dims_reinit = self.encoded_dims != Some((enc_w, enc_h));
        // OpenH264 时间戳单位为毫秒。
        let ts = Timestamp::from_millis(frame.pts_us / 1000);
        let (keyframe, data) = {
            let bitstream = self
                .inner
                .encode_at(&self.yuv, ts)
                .context("OpenH264 编码失败")?;
            (
                matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I),
                bitstream.to_vec(),
            )
        };
        self.encoded_dims = Some((enc_w, enc_h));
        if let Some(bps) = self.pending_bps.take() {
            self.apply_initialized_bps(bps)?;
        } else if dims_reinit {
            // openh264 尺寸变化自动 reinit 会用初始 config.target_bitrate 覆盖运行中码率，需重刷
            let bps = self.current_bps;
            self.apply_initialized_bps(bps)?;
        }
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
        let bps = kbps
            .saturating_mul(1000)
            .clamp(OH264_MIN_BPS, OH264_MAX_BPS);
        if self.encoded_dims.is_none() {
            // 首帧尚未触发 InitializeExt，记录待生效码率并在首帧后自动落盘
            self.pending_bps = Some(bps);
            self.current_bps = bps;
            return Ok(());
        }
        self.apply_initialized_bps(bps)
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
