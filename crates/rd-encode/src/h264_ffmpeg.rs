//! H.264 硬编（ffmpeg LGPL shared 动态链接，feature `ffmpeg`）。
//!
//! 探测顺序：`h264_nvenc` → `h264_qsv` → `h264_amf` → `h264_mf`，
//! 每个候选实际编码 1 帧验证可用性（驱动/DLL 缺失、显存不足等在探测期
//! 暴露），第一个成功者胜出；420 全部失败由 `create_best` 回退 OpenH264
//! （444 全部失败则明确报错，不静默回退，见 `create_best`）。
//!
//! 低延迟参数：`bf=0`（无 B 帧）、gop 极大（关键帧按需 IDR）、各编码器
//! 专属 zero-latency 选项、输入 NV12（420）/ yuv444p（444，打开方式与
//! `caps.rs` 探测一致：先带 `profile=high444p`，失败去掉 profile 重试）。
//! 仅链接 LGPL 构建，不含 libx264。
//!
//! 注意：avcodec 不支持运行中改码率/尺寸，`set_bitrate` / `reconfigure`
//! 通过重建编码器实现，重建后首帧即 IDR。

use std::collections::VecDeque;
use std::sync::Once;

use anyhow::{bail, Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::encoder::Video as FfmpegEncoderVideo;
use ffmpeg_next::codec::packet::Packet as FfmpegPacket;
use ffmpeg_next::util::frame::video::Video as FfmpegFrame;

use crate::frame::{even_geometry, ColorProfile, RawFrame};
use crate::{EncodedAu, EncoderConfig, EncoderInfo, PixelFormat, VideoEncoder};

/// Windows 常见 H.264 硬编探测顺序（NVIDIA → Intel → AMD → 系统内置 MF）。
pub const HW_CANDIDATES: &[&str] = &["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf"];

/// Windows 常见 HEVC (H.265) 硬编探测顺序（突破 H.264 4096×4096 限制，支撑 4K@144Hz）。
pub const HEVC_HW_CANDIDATES: &[&str] = &["hevc_nvenc", "hevc_qsv", "hevc_amf", "hevc_mf"];

/// Windows 常见 AV1 硬编探测顺序（RTX 40 / Arc / RX 7000+，4K@144Hz 极高压缩比）。
pub const AV1_HW_CANDIDATES: &[&str] = &["av1_nvenc", "av1_qsv", "av1_amf"];

/// `AVERROR(EAGAIN)`（POSIX EAGAIN 在各平台均为 11）。
const AVERROR_EAGAIN: i32 = -11;

/// ffmpeg 全局初始化（幂等）。
static FFMPEG_INIT: Once = Once::new();

fn ffmpeg_init() {
    FFMPEG_INIT.call_once(|| {
        if let Err(e) = ffmpeg::init() {
            tracing::warn!("ffmpeg::init 失败: {e}");
        }
    });
}

/// ffmpeg 硬编编码器。
pub struct FfmpegEncoder {
    codec_name: &'static str,
    octx: FfmpegEncoderVideo,
    frame: FfmpegFrame,
    packet: FfmpegPacket,
    /// 输入色度采样（420 → NV12，444 → yuv444p）。
    pix_fmt: PixelFormat,
    /// 与编码器 VUI 严格对齐的色彩矩阵/范围（优先 BT.709 Full，MFT 不支持时降级 Limited）。
    color_profile: ColorProfile,
    /// NV12 紧凑暂存（dcv 输出 → 行拷贝进 ffmpeg 帧平面，规避双平面
    /// 同时可变借用，1080p 拷贝开销 < 0.5 ms；420 路径）。
    nv12_y: Vec<u8>,
    nv12_uv: Vec<u8>,
    /// I444 紧凑三平面暂存（行距均 enc_w；444 路径）。
    y444: Vec<u8>,
    u444: Vec<u8>,
    v444: Vec<u8>,
    /// MFT 等 delay 型编码器的就绪 AU 队列（按 pts 出队）。
    pending: VecDeque<EncodedAu>,
    fps: u32,
    bitrate_kbps: u32,
    /// 最近一次实际落到 avcodec 的码率（用于防抖，避免 200ms rate tick 频繁重建硬编）。
    applied_bitrate_kbps: u32,
    /// 最近一次创建/重建时刻（硬编重开节流）。
    last_rebuild_at: std::time::Instant,
    /// 最近一次送入 avcodec 的 pts（保证 MFT 等编码器严格单调递增时间戳）。
    last_pts_us: Option<u64>,
    /// 编码宽高（偶数）。
    enc_width: u32,
    enc_height: u32,
    /// 右/下 padding 像素（相对输入帧）。
    crop: (u32, u32),
}

impl FfmpegEncoder {
    /// 按 `HW_CANDIDATES` 顺序探测并创建第一个可用的硬编编码器。
    ///
    /// `codec_hint` 非空时只探测指定编码器（必须在候选列表内）。
    pub fn create(cfg: &EncoderConfig, codec_hint: Option<&str>) -> Result<Self> {
        let candidates: &[&str] = match codec_hint {
            Some(name) => {
                if !HW_CANDIDATES.contains(&name) {
                    bail!("未知硬编编码器: {name}");
                }
                std::slice::from_ref(
                    &HW_CANDIDATES[HW_CANDIDATES
                        .iter()
                        .position(|c| *c == name)
                        .expect("已校验")],
                )
            }
            None => HW_CANDIDATES,
        };
        let mut errs = Vec::new();
        for name in candidates {
            match Self::try_new(cfg, name) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    tracing::debug!("硬编候选 {name} 不可用: {e:#}");
                    errs.push(format!("{name}: {e:#}"));
                }
            }
        }
        if errs.is_empty() {
            bail!("无可用硬编候选");
        }
        bail!("{}", errs.join(" | "))
    }

    /// 按协议 [`rd_proto::v0::Codec`] 选择对应硬编候选列表并创建首个可用编码器。
    pub fn create_for_codec(cfg: &EncoderConfig, codec: rd_proto::v0::Codec) -> Result<Self> {
        let candidates: &[&str] = match codec {
            rd_proto::v0::Codec::Av1 => AV1_HW_CANDIDATES,
            rd_proto::v0::Codec::Hevc444 => HEVC_HW_CANDIDATES,
            rd_proto::v0::Codec::H264420 | rd_proto::v0::Codec::H264444 => HW_CANDIDATES,
        };
        let mut errs = Vec::new();
        for name in candidates {
            match Self::try_new(cfg, name) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    tracing::debug!("硬编候选 {name} ({codec:?}) 不可用: {e:#}");
                    errs.push(format!("{name}: {e:#}"));
                }
            }
        }
        if errs.is_empty() {
            bail!("无可用 {codec:?} 硬编候选");
        }
        bail!("{}", errs.join(" | "))
    }

    fn try_new(cfg: &EncoderConfig, codec_name: &'static str) -> Result<Self> {
        let (enc_w, enc_h, crop) = even_geometry(cfg.width, cfg.height);
        let (octx, frame, color_profile) = open_encoder(
            codec_name,
            enc_w,
            enc_h,
            cfg.fps,
            cfg.bitrate_kbps,
            cfg.pixel_format,
        )?;
        let mut enc = Self {
            codec_name,
            octx,
            frame,
            packet: FfmpegPacket::empty(),
            pix_fmt: cfg.pixel_format,
            color_profile,
            nv12_y: Vec::new(),
            nv12_uv: Vec::new(),
            y444: Vec::new(),
            u444: Vec::new(),
            v444: Vec::new(),
            pending: VecDeque::new(),
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            applied_bitrate_kbps: cfg.bitrate_kbps,
            last_rebuild_at: std::time::Instant::now(),
            last_pts_us: None,
            enc_width: enc_w,
            enc_height: enc_h,
            crop,
        };
        // 探测：以目标分辨率实际编码测试图案，且限时内必须拿到真实 AU。
        // 仅「创建成功 + 编码不报错」不够：无 GPU 合成环境（CI 虚拟机、
        // RDP 会话、部分云主机）上 MFT 能打开但 drain 的 receive_packet
        // 永久阻塞——编码线程卡死、用户永远黑屏且无任何错误。
        // 注意：MFT 要求单调递增 pts_us（重复 pts=0 会被丢弃），且默认无
        // display_remoting 时可能缓冲 8~16 帧才吐出首包，故喂 24 帧递增时间戳。
        const PROBE_FRAMES: u8 = 24;
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
        let (probe_tx, probe_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut e = enc;
            let mut probe = probe_frame(e.enc_width, e.enc_height);
            let _ = probe_tx.send((|| -> Result<Self> {
                for n in 0..PROBE_FRAMES {
                    probe.pts_us = u64::from(n) * 16_667;
                    if e.encode_raw(&probe, n == 0)?.is_some() {
                        return Ok(e);
                    }
                }
                anyhow::bail!("{} 探测 {} 帧未产出任何 AU", e.codec_name, PROBE_FRAMES)
            })());
        });
        enc = match probe_rx.recv_timeout(PROBE_TIMEOUT) {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => bail!("{} 探测失败: {e:#}", codec_name),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                bail!(
                    "{} 探测 {}s 内未产出 AU（编码器打开但无输出，疑似无 GPU 合成环境），按不可用处理",
                    codec_name,
                    PROBE_TIMEOUT.as_secs()
                )
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("{} 探测线程异常退出", codec_name)
            }
        };
        // 丢弃探测所在的编码器流水线（MFT 类编码器带 delay，探测帧的 AU
        // 会在后续 encode 调用中混入正式流），重建后首帧即干净 IDR。
        enc.rebuild(enc_w, enc_h)?;
        Ok(enc)
    }

    fn encode_raw(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedAu>> {
        let enc_w = self.enc_width as usize;
        let enc_h = self.enc_height as usize;

        let black_y = self.color_profile.black_y();
        match self.pix_fmt {
            PixelFormat::Yuv420 => {
                // 1) BGRA → NV12（与编码器 VUI 同口径 BT.709 Full/Limited）。
                self.nv12_y.clear();
                self.nv12_uv.clear();
                self.nv12_y.resize(enc_w * enc_h, black_y);
                self.nv12_uv.resize(enc_w * (enc_h / 2), 128);
                {
                    let (y, uv) = (&mut self.nv12_y[..], &mut self.nv12_uv[..]);
                    frame.bgra_to_nv12_with(y, enc_w, uv, enc_w, self.color_profile)?;
                }

                // 2) 行拷贝进 ffmpeg 帧平面（帧平面可能带对齐 stride）。
                {
                    let y_stride = self.frame.stride(0);
                    let uv_stride = self.frame.stride(1);
                    for (row, src) in self.nv12_y.chunks_exact(enc_w).enumerate() {
                        self.frame.data_mut(0)[row * y_stride..row * y_stride + enc_w]
                            .copy_from_slice(src);
                    }
                    for (row, src) in self.nv12_uv.chunks_exact(enc_w).take(enc_h / 2).enumerate() {
                        self.frame.data_mut(1)[row * uv_stride..row * uv_stride + enc_w]
                            .copy_from_slice(src);
                    }
                }
            }
            PixelFormat::Yuv444 => {
                // 1) BGRA → I444（紧凑三平面，行距均 enc_w；与编码器 VUI 同口径）。
                self.y444.clear();
                self.u444.clear();
                self.v444.clear();
                self.y444.resize(enc_w * enc_h, black_y);
                self.u444.resize(enc_w * enc_h, 128);
                self.v444.resize(enc_w * enc_h, 128);
                {
                    let (y, u, v) = (&mut self.y444[..], &mut self.u444[..], &mut self.v444[..]);
                    frame.bgra_to_yuv444_with(y, u, v, enc_w, self.color_profile)?;
                }

                // 2) 行拷贝进 ffmpeg 帧三平面。
                for (plane, buf) in [(0usize, &self.y444), (1, &self.u444), (2, &self.v444)] {
                    let plane_stride = self.frame.stride(plane);
                    for (row, src) in buf.chunks_exact(enc_w).enumerate() {
                        self.frame.data_mut(plane)[row * plane_stride..row * plane_stride + enc_w]
                            .copy_from_slice(src);
                    }
                }
            }
        }

        // 3) 按需 IDR + 提交。帧对象跨调用复用，必须每帧显式重置
        // pict_type，否则一次 force_key 之后所有帧都会被请求成 I 帧。
        self.frame.set_kind(if force_key {
            // 请求 I 帧；nvenc 配合 "forced-idr" 使其成为 IDR。
            ffmpeg::util::picture::Type::I
        } else {
            ffmpeg::util::picture::Type::None
        });
        let pts_us = match self.last_pts_us {
            Some(prev) if frame.pts_us <= prev => prev.saturating_add(1_000),
            _ => frame.pts_us,
        };
        self.last_pts_us = Some(pts_us);
        self.frame.set_pts(Some(pts_us as i64));

        // 先清空可能的内部缓冲，避免 send_frame 因 EAGAIN 失败。
        self.drain();
        match self.octx.send_frame(&self.frame) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other {
                errno: AVERROR_EAGAIN,
            }) => {
                self.drain();
                self.octx
                    .send_frame(&self.frame)
                    .with_context(|| format!("{} send_frame 失败", self.codec_name))?;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("{} send_frame 失败", self.codec_name));
            }
        }
        self.drain();

        // MFT 类编码器（h264_mf 等）带 delay：早期帧输出为空是正常现象，
        // 已产出的 AU 在 pending 队列中按 pts 顺序出队。
        Ok(self.pending.pop_front())
    }

    /// 取出编码器内部所有已就绪的 packet（EAGAIN/EOF 均表示取完）。
    fn drain(&mut self) {
        while self.octx.receive_packet(&mut self.packet).is_ok() {
            let keyframe = self
                .packet
                .flags()
                .contains(ffmpeg::codec::packet::Flags::KEY);
            let data = self.packet.data().unwrap_or_default().to_vec();
            if data.is_empty() {
                continue;
            }
            let pts_us = self.packet.pts().unwrap_or(0).max(0) as u64;
            self.pending.push_back(EncodedAu {
                data,
                keyframe,
                pts_us,
                crop: self.crop,
            });
        }
    }

    /// 重建内部编码器（改码率/改尺寸共用，重建后首帧即 IDR）。
    fn rebuild(&mut self, enc_w: u32, enc_h: u32) -> Result<()> {
        let (octx, frame, color_profile) = open_encoder(
            self.codec_name,
            enc_w,
            enc_h,
            self.fps,
            self.bitrate_kbps,
            self.pix_fmt,
        )?;
        self.octx = octx;
        self.frame = frame;
        self.color_profile = color_profile;
        self.packet = FfmpegPacket::empty();
        self.pending.clear();
        self.nv12_y = Vec::new();
        self.nv12_uv = Vec::new();
        self.y444 = Vec::new();
        self.u444 = Vec::new();
        self.v444 = Vec::new();
        self.enc_width = enc_w;
        self.enc_height = enc_h;
        self.applied_bitrate_kbps = self.bitrate_kbps;
        self.last_rebuild_at = std::time::Instant::now();
        self.last_pts_us = None;
        Ok(())
    }
}

/// 打开指定硬编编码器并配置低延迟参数，返回编码上下文与对应像素格式的
/// 帧缓冲。
///
/// 对 `h264_mf` / `hevc_mf`：
/// 1. 优先尝试 `hw_encoding=true` + `scenario=display_remoting`（调用系统 GPU 硬件 D3D11 MFT，
///    即使 FFmpeg 自带 `h264_nvenc` 因驱动 NVENC SDK 版本要求过高失败，系统级硬件 MFT 仍可直接跑在 GPU 上）；
/// 2. 依次降级尝试不带 `scenario` 的硬件 MFT、软件低延迟 MFT；
/// 3. 若因高刷帧率（如 144fps）触发 MFT Level 媒体类型上限拒绝，仅将 MFT 协商帧率元数据降档到 120 / 60 重试
///    （编码时间基固定 1us，实际推帧吞吐仍按真实 `pts_us` 全速运行，不锁死帧率）。
fn open_encoder(
    codec_name: &str,
    enc_w: u32,
    enc_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    pix_fmt: PixelFormat,
) -> Result<(FfmpegEncoderVideo, FfmpegFrame, ColorProfile)> {
    let av_fmt = match pix_fmt {
        PixelFormat::Yuv420 => ffmpeg::util::format::Pixel::NV12,
        PixelFormat::Yuv444 => ffmpeg::util::format::Pixel::YUV444P,
    };
    let attempts: Vec<Vec<(&'static str, &'static str)>> = match pix_fmt {
        PixelFormat::Yuv444 => {
            let mut list = Vec::new();
            for base in codec_option_candidates(codec_name) {
                if codec_name != "h264_mf" && codec_name != "hevc_mf" {
                    let mut with_prof = base.clone();
                    with_prof.push(("profile", "high444p"));
                    list.push(with_prof);
                }
                list.push(base);
            }
            list
        }
        PixelFormat::Yuv420 => codec_option_candidates(codec_name),
    };

    let mut fps_candidates = vec![fps.max(1)];
    if codec_name.ends_with("_mf") {
        if fps > 120 {
            fps_candidates.push(120);
        }
        if fps > 60 {
            fps_candidates.push(60);
        }
    }

    // 色彩动态范围候选：NVENC/QSV/AMF 优先 BT.709 Full Range（0..255，黑纯黑、白纯白、文字灰阶无损），
    // 若驱动拒绝 Full Range（常见于部分 Windows MFT）则自动降级 BT.709 Limited 并同步矩阵，避免范围错配。
    let color_candidates: &[ColorProfile] = if codec_name.ends_with("_mf") {
        &[ColorProfile::Bt709Limited, ColorProfile::Bt709Full]
    } else {
        &[ColorProfile::Bt709Full, ColorProfile::Bt709Limited]
    };

    let mut first_err = None;
    for &color_profile in color_candidates {
        for &try_fps in &fps_candidates {
            for options in &attempts {
                match open_encoder_with_options(
                    codec_name,
                    enc_w,
                    enc_h,
                    try_fps,
                    bitrate_kbps,
                    av_fmt,
                    color_profile,
                    options,
                ) {
                    Ok((octx, frame)) => {
                        tracing::debug!(
                            codec = codec_name,
                            ?color_profile,
                            fps = try_fps,
                            ?options,
                            "硬编上下文打开成功"
                        );
                        return Ok((octx, frame, color_profile));
                    }
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
        }
    }
    Err(first_err.unwrap_or_else(|| anyhow::anyhow!("{codec_name} 打开失败（无可用参数组合）")))
}

/// 以确定的选项集打开编码器上下文（`open_encoder` 的单次尝试）。
#[allow(clippy::too_many_arguments)]
fn open_encoder_with_options(
    codec_name: &str,
    enc_w: u32,
    enc_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    av_fmt: ffmpeg::util::format::Pixel,
    color_profile: ColorProfile,
    options: &[(&'static str, &'static str)],
) -> Result<(FfmpegEncoderVideo, FfmpegFrame)> {
    ffmpeg_init();
    let codec = ffmpeg::codec::encoder::find_by_name(codec_name)
        .with_context(|| format!("未找到编码器 {codec_name}（本 ffmpeg 构建未编译该硬编）"))?;
    let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    // 时间基 1us：packet pts 直接等于 pts_us。
    ctx.set_time_base((1, 1_000_000));
    let mut enc = ctx
        .encoder()
        .video()
        .with_context(|| format!("{codec_name} 创建视频编码上下文失败"))?;
    enc.set_width(enc_w);
    enc.set_height(enc_h);
    enc.set_format(av_fmt);
    let bit_rate = bitrate_kbps.saturating_mul(1000) as usize;
    enc.set_bit_rate(bit_rate);
    enc.set_max_bit_rate(bit_rate);
    enc.set_gop(100_000);
    enc.set_frame_rate(Some(ffmpeg::Rational(fps as i32, 1)));
    enc.set_max_b_frames(0);

    // 显式写入 BT.709 VUI 色彩元数据：让 WebCodecs / 硬件解码器严格按同色域与同动态范围还原，
    // 彻底消除未标 VUI 导致的偏绿/偏红与 16..235 灰阶发灰问题（对标 Sunshine / RustDesk 最佳实践）。
    let av_range = match color_profile {
        ColorProfile::Bt709Full => ffmpeg::util::color::Range::JPEG,
        ColorProfile::Bt709Limited => ffmpeg::util::color::Range::MPEG,
    };
    enc.set_colorspace(ffmpeg::util::color::Space::BT709);
    enc.set_color_primaries(ffmpeg::util::color::Primaries::BT709);
    enc.set_color_transfer_characteristic(ffmpeg::util::color::TransferCharacteristic::BT709);
    enc.set_color_range(av_range);

    let mut opts = ffmpeg::Dictionary::new();
    for (key, value) in options {
        opts.set(key, value);
    }
    let opened = enc
        .open_with(opts)
        .with_context(|| format!("{codec_name} 打开失败（驱动/硬件不可用或参数不支持）"))?;

    let mut frame = FfmpegFrame::new(av_fmt, enc_w, enc_h);
    frame.set_color_space(ffmpeg::util::color::Space::BT709);
    frame.set_color_range(av_range);
    Ok((opened, frame))
}

/// 各硬编编码器的低延迟高画质私有选项候选集（按画质与兼容性降序排列）。
pub(crate) fn codec_option_candidates(codec_name: &str) -> Vec<Vec<(&'static str, &'static str)>> {
    match codec_name {
        "h264_nvenc" | "hevc_nvenc" | "av1_nvenc" => vec![
            // 首选：p3 平衡速度与细节 + spatial-aq 保护文字锐度与暗部层次 + VBR 避免静止浪费/复杂起块
            vec![
                ("preset", "p3"),
                ("tune", "ll"),
                ("zerolatency", "1"),
                ("delay", "0"),
                ("rc", "vbr"),
                ("spatial-aq", "1"),
                ("aq-strength", "8"),
                ("forced-idr", "1"),
            ],
            // 兜底：旧版驱动不支持 spatial-aq / zerolatency 选项时回落基础低延迟配置
            vec![
                ("preset", "p1"),
                ("tune", "ll"),
                ("delay", "0"),
                ("rc", "cbr"),
                ("forced-idr", "1"),
            ],
        ],
        "h264_qsv" | "hevc_qsv" | "av1_qsv" => vec![
            vec![
                ("preset", "medium"),
                ("async_depth", "1"),
                ("low_power", "0"),
            ],
            vec![("async_depth", "1"), ("low_power", "0")],
        ],
        "h264_amf" | "hevc_amf" | "av1_amf" => vec![
            vec![
                ("usage", "lowlatency"),
                ("quality", "balanced"),
                ("vbaq", "true"),
                ("rc", "vbr_latency"),
                ("async_depth", "1"),
            ],
            vec![
                ("usage", "lowlatency"),
                ("quality", "speed"),
                ("rc", "cbr"),
                ("async_depth", "1"),
            ],
        ],
        // Windows 10/11 Media Foundation：优先启用系统级 GPU 硬件 MFT + 远程桌面零延迟模式，
        // 若硬件 MFT 不存在或拒绝某项参数再逐步降级到低延迟软件 MFT。
        "h264_mf" | "hevc_mf" => vec![
            vec![
                ("rate_control", "cbr"),
                ("scenario", "display_remoting"),
                ("hw_encoding", "true"),
            ],
            vec![("rate_control", "cbr"), ("hw_encoding", "true")],
            vec![("rate_control", "cbr"), ("scenario", "display_remoting")],
            vec![("rate_control", "cbr")],
        ],
        _ => vec![Vec::new()],
    }
}

/// 探测用测试图案帧（与编码分辨率同尺寸的灰阶渐变 + 色块）。
fn probe_frame(enc_w: u32, enc_h: u32) -> RawFrame {
    let mut frame = RawFrame::new(enc_w, enc_h, 0);
    let stride = frame.stride as usize;
    for y in 0..frame.height as usize {
        for x in 0..frame.width as usize {
            let off = y * stride + x * 4;
            let v = ((x * 255) / frame.width.max(1) as usize) as u8;
            let c = ((y * 255) / frame.height.max(1) as usize) as u8;
            frame.data[off..off + 4].copy_from_slice(&[v, c, v, 255]);
        }
    }
    frame
}

impl VideoEncoder for FfmpegEncoder {
    fn encode(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedAu>> {
        if frame.width > self.enc_width || frame.height > self.enc_height {
            bail!(
                "输入帧 {}x{} 大于编码器 {}x{}，请先 reconfigure",
                frame.width,
                frame.height,
                self.enc_width,
                self.enc_height
            );
        }
        self.crop = (self.enc_width - frame.width, self.enc_height - frame.height);
        self.encode_raw(frame, force_key)
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        let kbps = kbps.max(1);
        self.bitrate_kbps = kbps;
        let base = self.applied_bitrate_kbps.max(1);
        let pct_diff = u64::from(kbps.abs_diff(base)) * 100 / u64::from(base);
        // avcodec 硬编改码率需重建编码器并强制 IDR；对 200ms 周期的细小 AIMD 爬坡做
        // 防抖（幅度 <25% 暂存不重建，25%~80% 至少间隔 1.5s，>=80% 手动跳变立即重建）。
        let should_rebuild = pct_diff >= 80
            || (pct_diff >= 25
                && self.last_rebuild_at.elapsed() >= std::time::Duration::from_millis(1500));
        if !should_rebuild {
            return Ok(());
        }
        let (enc_w, enc_h) = (self.enc_width, self.enc_height);
        self.rebuild(enc_w, enc_h)
    }

    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        let (enc_w, enc_h, _) = even_geometry(width, height);
        self.rebuild(enc_w, enc_h)
    }

    fn info(&self) -> EncoderInfo {
        EncoderInfo {
            name: self.codec_name,
            hardware: true,
        }
    }
}
