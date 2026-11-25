//! 编码能力探测与协商（T-011）。
//!
//! [`probe_encode_caps`] 报告本机可编码的 [`rd_proto::v0::Codec`] 列表，
//! 供握手（`HelloAck.encode_caps`）与 [`negotiate`] 使用：
//!
//! - `H264420` 始终可用（OpenH264 软编兜底）；
//! - `H264444` 用 ffmpeg 硬编以 `pix_fmt=yuv444p`、`profile=high444p`
//!   实际编码 1 帧验证（先行带 profile 打开，失败再去掉 profile 依赖
//!   编码器从像素格式自动选择），任一硬编候选成功即判定支持；探测用
//!   的编码上下文随即丢弃，不影响正式编码器（4:4:4 的实际创建走
//!   `create_best(Yuv444)`，与探测同参数）；
//! - 结果进程内缓存（[`OnceLock`]），探测只执行一次。
//!
//! 未启用 `ffmpeg` feature（非 Windows 或 `--no-default-features`）时
//! 仅报告 `H264420`。本模块不修改 `h264_ffmpeg` 的正式编码路径，独立
//! 走 ffmpeg-next API。

use std::sync::OnceLock;

use rd_proto::v0::Codec;

/// 4:4:4 能力探测结果缓存（进程生命周期内只探测一次）。
static CAPS: OnceLock<Vec<Codec>> = OnceLock::new();

/// 探测本机可编码的编解码器列表（结果缓存，重复调用零开销）。
pub fn probe_encode_caps() -> Vec<Codec> {
    CAPS.get_or_init(probe_once).clone()
}

/// 协商编解码器：双方都支持 4:4:4 且解码端显式声明 `dec_444`
/// （对应 `Hello.supports_444`，即 WebCodecs `isConfigSupported` 通过）
/// 时选 [`Codec::H264444`]，否则回退 [`Codec::H264420`]。
pub fn negotiate(enc: &[Codec], dec: &[Codec], dec_444: bool) -> Codec {
    let both_444 = dec_444 && enc.contains(&Codec::H264444) && dec.contains(&Codec::H264444);
    if both_444 {
        Codec::H264444
    } else {
        Codec::H264420
    }
}

/// 分辨率感知的编解码协商（宪法 #1：4K@144Hz 优先突破 H.264 4096 硬件上限）：
/// - 超过 1440p（宽 > 2560 或高 > 1440）且双方支持时优先 `AV1` → `HEVC_444`；
/// - 其余场景回落 [`negotiate`]（H.264 4:4:4 / 4:2:0）。
pub fn negotiate_for_resolution(
    enc: &[Codec],
    dec: &[Codec],
    dec_444: bool,
    width: u32,
    height: u32,
) -> Codec {
    let high_res = width > 2560 || height > 1440;
    if high_res {
        if enc.contains(&Codec::Av1) && dec.contains(&Codec::Av1) {
            return Codec::Av1;
        }
        if enc.contains(&Codec::Hevc444) && dec.contains(&Codec::Hevc444) {
            return Codec::Hevc444;
        }
    }
    negotiate(enc, dec, dec_444)
}

fn probe_once() -> Vec<Codec> {
    #[cfg(all(windows, feature = "ffmpeg"))]
    let (has_444, has_hevc, has_av1) = (
        probe_444_any_hw(),
        probe_nv12_codec(Codec::Hevc444),
        probe_nv12_codec(Codec::Av1),
    );
    #[cfg(not(all(windows, feature = "ffmpeg")))]
    let (has_444, has_hevc, has_av1) = (false, false, false);

    let mut caps = vec![Codec::H264420];
    if has_444 {
        caps.push(Codec::H264444);
    }
    if has_hevc {
        caps.push(Codec::Hevc444);
    }
    if has_av1 {
        caps.push(Codec::Av1);
    }
    tracing::info!("编码能力探测结果: {caps:?}");
    caps
}

/// 探测指定硬编族（HEVC / AV1）能否以 NV12 正常打开（`create_for_codec` 内部已按序遍历候选，
/// 仅需调用一次，避免外层再循环导致 O(N²) 重复初始化拖慢首连）。
#[cfg(all(windows, feature = "ffmpeg"))]
fn probe_nv12_codec(codec: Codec) -> bool {
    let probe_cfg = crate::EncoderConfig {
        width: PROBE_W,
        height: PROBE_H,
        fps: 60,
        bitrate_kbps: 4_000,
        prefer_hw: true,
        pixel_format: crate::PixelFormat::Yuv420,
    };
    if crate::h264_ffmpeg::FfmpegEncoder::create_for_codec(&probe_cfg, codec).is_ok() {
        tracing::info!("硬编 {codec:?} 可用");
        return true;
    }
    false
}

/// 依次探测各硬编候选的 4:4:4 能力，首个成功者即为可用。
#[cfg(all(windows, feature = "ffmpeg"))]
fn probe_444_any_hw() -> bool {
    for name in crate::h264_ffmpeg::HW_CANDIDATES {
        if probe_444(name) {
            tracing::info!("硬编 H.264 High 4:4:4 可用: {name}");
            return true;
        }
        tracing::debug!("硬编 4:4:4 不可用: {name}");
    }
    tracing::info!("所有硬编候选均不支持 4:4:4，仅保留 4:2:0");
    false
}

/// 用指定硬编编码器以 yuv444p 编码 1 帧验证 4:4:4 能力。
///
/// 先带 `profile=high444p` 尝试（nvenc/qsv 支持该 profile 名）；打开
/// 失败再去掉 profile 重试（amf/mf 可能不接受显式 profile，由编码器
/// 从像素格式自动选择）。两类路径都失败才判定不支持。
#[cfg(all(windows, feature = "ffmpeg"))]
fn probe_444(codec_name: &str) -> bool {
    // 两组选项：带 / 不带显式 profile。
    for with_profile in [true, false] {
        match try_encode_444(codec_name, &options_444(codec_name, with_profile)) {
            Ok(()) => return true,
            Err(e) => {
                tracing::debug!("4:4:4 探测 {codec_name}（profile={with_profile}）失败: {e:#}");
            }
        }
    }
    false
}

/// 4:4:4 探测用低延迟私有选项（与 `h264_ffmpeg` 正式路径一致，另加可选
/// profile；未消费的选项键会导致 avcodec_open2 失败，按编码器定制）。
#[cfg(all(windows, feature = "ffmpeg"))]
fn options_444(codec_name: &str, with_profile: bool) -> Vec<(&'static str, &'static str)> {
    let mut opts: Vec<(&'static str, &'static str)> = match codec_name {
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
        "h264_mf" => vec![("rate_control", "cbr")],
        _ => Vec::new(),
    };
    if with_profile && codec_name != "h264_mf" {
        opts.push(("profile", "high444p"));
    }
    opts
}

/// 小分辨率探测（只验证能否打开并产出 4:4:4 码流，压低耗时与显存）。
#[cfg(all(windows, feature = "ffmpeg"))]
const PROBE_W: u32 = 320;
#[cfg(all(windows, feature = "ffmpeg"))]
const PROBE_H: u32 = 240;

#[cfg(all(windows, feature = "ffmpeg"))]
fn try_encode_444(codec_name: &str, options: &[(&str, &str)]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use ffmpeg_next as ffmpeg;

    let codec = ffmpeg::codec::encoder::find_by_name(codec_name)
        .with_context(|| format!("未找到编码器 {codec_name}"))?;
    let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    ctx.set_time_base((1, 1_000_000));
    let mut enc = ctx
        .encoder()
        .video()
        .with_context(|| format!("{codec_name} 创建编码上下文失败"))?;
    enc.set_width(PROBE_W);
    enc.set_height(PROBE_H);
    enc.set_format(ffmpeg::util::format::Pixel::YUV444P);
    enc.set_bit_rate(2_000_000);
    enc.set_gop(100_000);
    enc.set_frame_rate(Some(ffmpeg::Rational(60, 1)));
    enc.set_max_b_frames(0);

    let mut opts = ffmpeg::Dictionary::new();
    for (key, value) in options {
        opts.set(key, value);
    }
    let mut opened = enc
        .open_with(opts)
        .with_context(|| format!("{codec_name} 以 yuv444p 打开失败（硬件或参数不支持）"))?;

    // 中灰 4:4:4 测试帧（内容无关，仅验证编码通路）。
    let mut frame = ffmpeg::util::frame::video::Video::new(
        ffmpeg::util::format::Pixel::YUV444P,
        PROBE_W,
        PROBE_H,
    );
    for plane in 0..3 {
        let stride = frame.stride(plane);
        for row in 0..PROBE_H as usize {
            frame.data_mut(plane)[row * stride..row * stride + PROBE_W as usize].fill(0x80);
        }
    }
    frame.set_pts(Some(0));
    opened
        .send_frame(&frame)
        .with_context(|| format!("{codec_name} send_frame 失败"))?;

    // 取 packet：立即能取到则用之；取不到（MFT 类编码器带 delay）时
    // EOF flush 后必有输出。探测上下文随即丢弃。
    let mut packet = ffmpeg::codec::packet::Packet::empty();
    if opened.receive_packet(&mut packet).is_err() {
        opened
            .send_eof()
            .with_context(|| format!("{codec_name} flush 失败"))?;
        opened
            .receive_packet(&mut packet)
            .with_context(|| format!("{codec_name} flush 后仍无输出"))?;
    }
    let non_empty = packet.data().is_some_and(|d| !d.is_empty());
    anyhow::ensure!(non_empty, "{codec_name} 4:4:4 探测输出为空 packet");
    Ok(())
}
