//! H.264 硬编（ffmpeg LGPL shared 动态链接，feature `ffmpeg`）。
//!
//! 探测顺序：`h264_nvenc` → `h264_qsv` → `h264_amf` → `h264_mf`，
//! 每个候选实际编码 1 帧验证可用性（驱动/DLL 缺失、显存不足等在探测期
//! 暴露），第一个成功者胜出；全部失败由 `create_best` 回退 OpenH264。
//!
//! 低延迟参数：`bf=0`（无 B 帧）、gop 极大（关键帧按需 IDR）、各编码器
//! 专属 zero-latency 选项、NV12 输入。仅链接 LGPL 构建，不含 libx264。
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

use crate::frame::{even_geometry, RawFrame};
use crate::{EncodedAu, EncoderConfig, EncoderInfo, VideoEncoder};

/// Windows 常见硬编探测顺序（NVIDIA → Intel → AMD → 系统内置 MF）。
pub const HW_CANDIDATES: &[&str] = &["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf"];

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
    /// NV12 紧凑暂存（dcv 输出 → 行拷贝进 ffmpeg 帧平面，规避双平面
    /// 同时可变借用，1080p 拷贝开销 < 0.5 ms）。
    nv12_y: Vec<u8>,
    nv12_uv: Vec<u8>,
    /// MFT 等 delay 型编码器的就绪 AU 队列（按 pts 出队）。
    pending: VecDeque<EncodedAu>,
    fps: u32,
    bitrate_kbps: u32,
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
        let mut first_err = None;
        for name in candidates {
            match Self::try_new(cfg, name) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    tracing::debug!("硬编候选 {name} 不可用: {e:#}");
                    first_err.get_or_insert(e);
                }
            }
        }
        Err(first_err.unwrap_or_else(|| anyhow::anyhow!("无可用硬编候选")))
    }

    fn try_new(cfg: &EncoderConfig, codec_name: &'static str) -> Result<Self> {
        let (enc_w, enc_h, crop) = even_geometry(cfg.width, cfg.height);
        let (octx, frame) = open_encoder(codec_name, enc_w, enc_h, cfg.fps, cfg.bitrate_kbps)?;
        let mut enc = Self {
            codec_name,
            octx,
            frame,
            packet: FfmpegPacket::empty(),
            nv12_y: Vec::new(),
            nv12_uv: Vec::new(),
            pending: VecDeque::new(),
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            enc_width: enc_w,
            enc_height: enc_h,
            crop,
        };
        // 探测：以目标分辨率实际编码 1 帧测试图案。
        let probe = probe_frame(enc_w, enc_h);
        enc.encode_raw(&probe, false)?;
        // 丢弃探测所在的编码器流水线（MFT 类编码器带 delay，探测帧的 AU
        // 会在后续 encode 调用中混入正式流），重建后首帧即干净 IDR。
        enc.rebuild(enc_w, enc_h)?;
        Ok(enc)
    }

    fn encode_raw(&mut self, frame: &RawFrame, force_key: bool) -> Result<Option<EncodedAu>> {
        let enc_w = self.enc_width as usize;
        let enc_h = self.enc_height as usize;

        // 1) BGRA → NV12（紧凑暂存）。
        self.nv12_y.clear();
        self.nv12_uv.clear();
        self.nv12_y.resize(enc_w * enc_h, 16);
        self.nv12_uv.resize(enc_w * (enc_h / 2), 128);
        {
            let (y, uv) = (&mut self.nv12_y[..], &mut self.nv12_uv[..]);
            frame.bgra_to_nv12_into(y, enc_w, uv, enc_w)?;
        }

        // 2) 行拷贝进 ffmpeg 帧平面（帧平面可能带对齐 stride）。
        {
            let y_stride = self.frame.stride(0);
            let uv_stride = self.frame.stride(1);
            for (row, src) in self.nv12_y.chunks_exact(enc_w).enumerate() {
                self.frame.data_mut(0)[row * y_stride..row * y_stride + enc_w].copy_from_slice(src);
            }
            for (row, src) in self.nv12_uv.chunks_exact(enc_w).take(enc_h / 2).enumerate() {
                self.frame.data_mut(1)[row * uv_stride..row * uv_stride + enc_w]
                    .copy_from_slice(src);
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
        self.frame.set_pts(Some(frame.pts_us as i64));

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
        let (octx, frame) =
            open_encoder(self.codec_name, enc_w, enc_h, self.fps, self.bitrate_kbps)?;
        self.octx = octx;
        self.frame = frame;
        self.packet = FfmpegPacket::empty();
        self.nv12_y = Vec::new();
        self.nv12_uv = Vec::new();
        self.enc_width = enc_w;
        self.enc_height = enc_h;
        Ok(())
    }
}

/// 打开指定硬编编码器并配置低延迟参数，返回编码上下文与 NV12 帧缓冲。
fn open_encoder(
    codec_name: &str,
    enc_w: u32,
    enc_h: u32,
    fps: u32,
    bitrate_kbps: u32,
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
    enc.set_format(ffmpeg::util::format::Pixel::NV12);
    enc.set_bit_rate(bitrate_kbps.saturating_mul(1000) as usize);
    enc.set_gop(100_000);
    enc.set_frame_rate(Some(ffmpeg::Rational(fps as i32, 1)));
    enc.set_max_b_frames(0);

    let mut opts = ffmpeg::Dictionary::new();
    for (key, value) in codec_options(codec_name) {
        opts.set(key, value);
    }
    let opened = enc
        .open_with(opts)
        .with_context(|| format!("{codec_name} 打开失败（驱动/硬件不可用或参数不支持）"))?;

    let frame = FfmpegFrame::new(ffmpeg::util::format::Pixel::NV12, enc_w, enc_h);
    Ok((opened, frame))
}

/// 各硬编编码器的低延迟私有选项（名称已对照 `ffmpeg -h encoder=<name>`
/// 校验；未知选项会导致 avcodec_open2 失败，必须按编码器定制）。
fn codec_options(codec_name: &str) -> Vec<(&'static str, &'static str)> {
    match codec_name {
        "h264_nvenc" => vec![
            ("preset", "p1"),
            ("tune", "ll"),
            ("delay", "0"),
            ("rc", "cbr"),
            ("forced-idr", "1"),
        ],
        "h264_qsv" => vec![("async_depth", "1"), ("low_power", "0")],
        "h264_amf" => vec![
            ("usage", "lowlatency"),
            ("quality", "speed"),
            ("rc", "cbr"),
            ("async_depth", "1"),
        ],
        // Windows 10/11 系统内置；CBR 保证带宽可预测。
        "h264_mf" => vec![("rate_control", "cbr")],
        _ => Vec::new(),
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
        self.bitrate_kbps = kbps;
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
