//! D3D11 GPU 零拷贝硬件编码器（宪法 #1：4K@144Hz 零 CPU 读回管线）。
//!
//! 管线：WGC `ID3D11Texture2D` (BGRA8)
//! → `ID3D11VideoProcessor`（显存内一步完成裁剪 + BGRA→NV12 BT.709 色彩转换）
//! → FFmpeg `AVHWFramesContext` (`AV_PIX_FMT_D3D11` / `sw_format=NV12`)
//! → `avcodec_send_frame`（`nvenc` / `qsv` / `amf` / `mf` 硬编直吞显存纹理）
//! → Annex-B AU。
//!
//! 全程无 `D3D11_USAGE_STAGING` 拷贝、无 `Map` CPU 读回、无 CPU 色彩转换。
//! 4K@144Hz 场景下可消除 ~4.78 GB/s 的 PCIe/内存带宽与 CPU 占用。

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::sys as av;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEX2D_ARRAY_VPOV, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::frame::{even_geometry, ColorProfile, RawFrame};
use crate::h264_ffmpeg::{
    codec_option_candidates, AV1_HW_CANDIDATES, HEVC_HW_CANDIDATES, HW_CANDIDATES,
};
use crate::{EncodedAu, EncoderConfig, EncoderInfo, GpuFrame, VideoEncoder};

const AVERROR_EAGAIN: i32 = -11;

static FFMPEG_INIT: Once = Once::new();

fn ffmpeg_init() {
    FFMPEG_INIT.call_once(|| {
        if let Err(e) = ffmpeg::init() {
            tracing::warn!("ffmpeg::init 失败: {e}");
        }
    });
}

/// RAII 包装 `AVBufferRef*`，Drop 时自动 `av_buffer_unref`。
struct AvBufferGuard(*mut av::AVBufferRef);

impl Drop for AvBufferGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { av::av_buffer_unref(&mut self.0) };
        }
    }
}

/// RAII 包装 `AVCodecContext*`，Drop 时自动 `avcodec_free_context`。
struct AvCodecCtxGuard(*mut av::AVCodecContext);

impl Drop for AvCodecCtxGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { av::avcodec_free_context(&mut self.0) };
        }
    }
}

/// RAII 包装 `AVFrame*`，Drop 时自动 `av_frame_free`。
struct AvFrameGuard(*mut av::AVFrame);

impl AvFrameGuard {
    fn new() -> Result<Self> {
        let p = unsafe { av::av_frame_alloc() };
        if p.is_null() {
            bail!("av_frame_alloc 失败");
        }
        Ok(Self(p))
    }
}

impl Drop for AvFrameGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { av::av_frame_free(&mut self.0) };
        }
    }
}

/// RAII 包装 `AVPacket*`，Drop 时自动 `av_packet_free`。
struct AvPacketGuard(*mut av::AVPacket);

impl AvPacketGuard {
    fn new() -> Result<Self> {
        let p = unsafe { av::av_packet_alloc() };
        if p.is_null() {
            bail!("av_packet_alloc 失败");
        }
        Ok(Self(p))
    }
}

impl Drop for AvPacketGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { av::av_packet_free(&mut self.0) };
        }
    }
}

/// 包装 D3D11 VideoProcessor（负责 GPU 显存内 BGRA→NV12 色彩转换与客户区裁剪）。
struct GpuColorConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    src_size: (u32, u32),
    dst_size: (u32, u32),
    color_profile: ColorProfile,
    /// 按 `(texture_ptr, array_slice)` 缓存输出视图（FFmpeg 池固定 4 槽，命中率 100%）。
    out_views: HashMap<(usize, u32), ID3D11VideoProcessorOutputView>,
    /// 按源纹理指针缓存输入视图（WGC 2 槽，命中率 100%）。
    in_views: HashMap<usize, ID3D11VideoProcessorInputView>,
}

impl GpuColorConverter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
        color_profile: ColorProfile,
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("QI ID3D11VideoDevice 失败（显卡驱动不支持 D3D11 VideoProcessor）")?;
        let video_context: ID3D11VideoContext =
            context.cast().context("QI ID3D11VideoContext 失败")?;

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 144,
                Denominator: 1,
            },
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 144,
                Denominator: 1,
            },
            OutputWidth: dst_w,
            OutputHeight: dst_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
            .context("CreateVideoProcessorEnumerator 失败")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor 失败")?;

        // 禁用自动色彩处理（防止驱动私自做锐化/降噪/动态对比度导致文字发虚）。
        unsafe {
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }

        // 输入：RGB Full Range (0..255) BT.709
        // bitfields: Usage(0=Playback), RGB_Range(0=Full), YCbCr_Matrix(1=BT.709),
        //            YCbCr_xvYCC(0), Nominal_Range(2=0..255)
        let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: (1 << 2) | (2 << 4),
        };
        // 输出：YCbCr BT.709，Full(0..255) 或 Limited(16..235) 与编码器 VUI 严格对齐。
        let nominal_out = match color_profile {
            ColorProfile::Bt709Full => 2u32,    // 0..255
            ColorProfile::Bt709Limited => 1u32, // 16..235
        };
        let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: (1 << 2) | (nominal_out << 4),
        };
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &in_cs);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &out_cs);
        }

        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            src_size: (src_w, src_h),
            dst_size: (dst_w, dst_h),
            color_profile,
            out_views: HashMap::new(),
            in_views: HashMap::new(),
        })
    }

    /// 在 GPU 显存内把 `src_tex` 的 `crop` 区域转换为 NV12 并写入 `dst_tex[dst_slice]`。
    fn blit_bgra_to_nv12(
        &mut self,
        src_tex: &ID3D11Texture2D,
        crop: (u32, u32, u32, u32),
        dst_tex: &ID3D11Texture2D,
        dst_slice: u32,
    ) -> Result<()> {
        // 源尺寸若变化（窗口拖动/resize 过渡帧）则重建 enumerator/processor。
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src_tex.GetDesc(&mut src_desc) };
        if (src_desc.Width, src_desc.Height) != self.src_size {
            let device = unsafe { src_tex.GetDevice() }.context("GetDevice 失败")?;
            let context =
                unsafe { device.GetImmediateContext() }.context("GetImmediateContext 失败")?;
            *self = Self::new(
                &device,
                &context,
                src_desc.Width,
                src_desc.Height,
                self.dst_size.0,
                self.dst_size.1,
                self.color_profile,
            )?;
        }

        let in_key = src_tex.as_raw() as usize;
        if self.in_views.len() > 8 {
            self.in_views.clear();
        }
        let in_view = match self.in_views.get(&in_key) {
            Some(v) => v.clone(),
            None => {
                let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                };
                let mut view: Option<ID3D11VideoProcessorInputView> = None;
                unsafe {
                    self.video_device.CreateVideoProcessorInputView(
                        src_tex,
                        &self.enumerator,
                        &iv_desc,
                        Some(&mut view),
                    )
                }
                .context("CreateVideoProcessorInputView 失败")?;
                let view = view.ok_or_else(|| anyhow!("InputView 为空"))?;
                self.in_views.insert(in_key, view.clone());
                view
            }
        };

        let out_key = (dst_tex.as_raw() as usize, dst_slice);
        if self.out_views.len() > 16 {
            self.out_views.clear();
        }
        let out_view = match self.out_views.get(&out_key) {
            Some(v) => v.clone(),
            None => {
                let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { dst_tex.GetDesc(&mut dst_desc) };
                let ov_desc = if dst_desc.ArraySize > 1 {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                                MipSlice: 0,
                                FirstArraySlice: dst_slice,
                                ArraySize: 1,
                            },
                        },
                    }
                } else {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    }
                };
                let mut view: Option<ID3D11VideoProcessorOutputView> = None;
                unsafe {
                    self.video_device.CreateVideoProcessorOutputView(
                        dst_tex,
                        &self.enumerator,
                        &ov_desc,
                        Some(&mut view),
                    )
                }
                .context("CreateVideoProcessorOutputView 失败")?;
                let view = view.ok_or_else(|| anyhow!("OutputView 为空"))?;
                self.out_views.insert(out_key, view.clone());
                view
            }
        };

        let (cx, cy, cw, ch) = crop;
        let src_rect = RECT {
            left: cx as i32,
            top: cy as i32,
            right: (cx + cw).min(src_desc.Width) as i32,
            bottom: (cy + ch).min(src_desc.Height) as i32,
        };
        let dst_rect = RECT {
            left: 0,
            top: 0,
            right: cw.min(self.dst_size.0) as i32,
            bottom: ch.min(self.dst_size.1) as i32,
        };
        let target_rect = RECT {
            left: 0,
            top: 0,
            right: self.dst_size.0 as i32,
            bottom: self.dst_size.1 as i32,
        };

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(in_view)),
            ppFutureSurfaces: ptr::null_mut(),
            ppPastSurfacesRight: ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: ptr::null_mut(),
        };

        let blt_res = unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&src_rect),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&dst_rect),
            );
            self.video_context.VideoProcessorSetOutputTargetRect(
                &self.processor,
                true,
                Some(&target_rect),
            );
            self.video_context
                .VideoProcessorBlt(&self.processor, &out_view, 0, &[stream.clone()])
        };

        // 释放 ManuallyDrop 内的 COM 引用（stream.clone() 不会自动 drop 原 ManuallyDrop）。
        unsafe {
            ManuallyDrop::drop(&mut stream.pInputSurface);
        }

        blt_res.context("VideoProcessorBlt (BGRA→NV12) 失败")?;
        Ok(())
    }
}

/// D3D11 GPU 零拷贝硬件编码器。
pub struct D3d11HwEncoder {
    codec_name: &'static str,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    hw_device_ctx: AvBufferGuard,
    hw_frames_ctx: AvBufferGuard,
    avctx: AvCodecCtxGuard,
    pkt: AvPacketGuard,
    converter: GpuColorConverter,
    color_profile: ColorProfile,
    pending: VecDeque<EncodedAu>,
    fps: u32,
    bitrate_kbps: u32,
    applied_bitrate_kbps: u32,
    last_rebuild_at: Instant,
    last_pts_us: Option<u64>,
    enc_width: u32,
    enc_height: u32,
    crop: (u32, u32),
}

// 内部 COM/FFmpeg 上下文只在单一编码线程中独占访问，且共享的 ID3D11Device 已启用多线程保护。
unsafe impl Send for D3d11HwEncoder {}

impl D3d11HwEncoder {
    /// 按协议 [`rd_proto::v0::Codec`] 与共享的 `ID3D11Device` 探测并创建首个可用的零拷贝硬编编码器。
    pub fn create(
        cfg: &EncoderConfig,
        codec: rd_proto::v0::Codec,
        device: ID3D11Device,
    ) -> Result<Self> {
        let candidates: &[&str] = match codec {
            rd_proto::v0::Codec::Av1 => AV1_HW_CANDIDATES,
            rd_proto::v0::Codec::Hevc444 => HEVC_HW_CANDIDATES,
            rd_proto::v0::Codec::H264420 | rd_proto::v0::Codec::H264444 => HW_CANDIDATES,
        };
        let mut errs = Vec::new();
        for name in candidates {
            // FFmpeg `*_qsv` 编码器仅接受 `qsv`/`nv12` 像素格式，不接受 `d3d11`，直接跳过避免冗余报错日志。
            if name.ends_with("_qsv") {
                continue;
            }
            match Self::try_new(cfg, name, device.clone()) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    tracing::debug!("D3D11 零拷贝硬编候选 {name} ({codec:?}) 不可用: {e:#}");
                    errs.push(format!("{name}: {e:#}"));
                }
            }
        }
        if errs.is_empty() {
            bail!("无可用 {codec:?} D3D11 零拷贝硬编候选");
        }
        bail!("{}", errs.join(" | "))
    }

    fn try_new(
        cfg: &EncoderConfig,
        codec_name: &'static str,
        device: ID3D11Device,
    ) -> Result<Self> {
        ffmpeg_init();
        let (enc_w, enc_h, crop) = even_geometry(cfg.width, cfg.height);
        let context = unsafe { device.GetImmediateContext() }
            .context("ID3D11Device::GetImmediateContext 失败")?;

        let hw_device_ctx = create_d3d11_hw_device_ctx(&device)?;
        let hw_frames_ctx = create_d3d11_hw_frames_ctx(hw_device_ctx.0, enc_w, enc_h)?;
        let (avctx, color_profile) = open_d3d11_encoder(
            codec_name,
            enc_w,
            enc_h,
            cfg.fps,
            cfg.bitrate_kbps,
            hw_frames_ctx.0,
        )?;
        let converter =
            GpuColorConverter::new(&device, &context, enc_w, enc_h, enc_w, enc_h, color_profile)?;
        let pkt = AvPacketGuard::new()?;

        let mut enc = Self {
            codec_name,
            device,
            context,
            hw_device_ctx,
            hw_frames_ctx,
            avctx,
            pkt,
            converter,
            color_profile,
            pending: VecDeque::new(),
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            applied_bitrate_kbps: cfg.bitrate_kbps,
            last_rebuild_at: Instant::now(),
            last_pts_us: None,
            enc_width: enc_w,
            enc_height: enc_h,
            crop,
        };

        // 探测验证：在 GPU 上创建一块临时测试纹理实际编码，确认编码器能吐出真实非空 AU。
        let probe_tex = create_probe_bgra_texture(&enc.device, enc_w, enc_h)?;
        let probe_step_us = (1_000_000u64 / cfg.fps.max(1) as u64).max(1);
        let mut got_au = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        for i in 0..8u64 {
            if Instant::now() > deadline {
                break;
            }
            let gf = GpuFrame {
                texture: probe_tex.clone(),
                crop: (0, 0, enc_w, enc_h),
                pts_us: (i + 1) * probe_step_us,
            };
            if let Some(au) = enc.encode_gpu_inner(&gf, i == 0)? {
                if !au.data.is_empty() {
                    got_au = true;
                    break;
                }
            }
        }
        if !got_au {
            bail!("{codec_name} D3D11 零拷贝探测 8 帧未产出 AU，放弃此候选");
        }

        // 重建干净实例（重置内部缓冲与 pts 基线）。
        enc.rebuild(enc_w, enc_h)?;
        Ok(enc)
    }

    fn rebuild(&mut self, enc_w: u32, enc_h: u32) -> Result<()> {
        let hw_frames_ctx = create_d3d11_hw_frames_ctx(self.hw_device_ctx.0, enc_w, enc_h)?;
        let (avctx, color_profile) = open_d3d11_encoder(
            self.codec_name,
            enc_w,
            enc_h,
            self.fps,
            self.bitrate_kbps,
            hw_frames_ctx.0,
        )?;
        let converter = GpuColorConverter::new(
            &self.device,
            &self.context,
            enc_w,
            enc_h,
            enc_w,
            enc_h,
            color_profile,
        )?;
        self.avctx = avctx;
        self.hw_frames_ctx = hw_frames_ctx;
        self.converter = converter;
        self.color_profile = color_profile;
        self.enc_width = enc_w;
        self.enc_height = enc_h;
        self.applied_bitrate_kbps = self.bitrate_kbps;
        self.last_rebuild_at = Instant::now();
        self.last_pts_us = None;
        self.pending.clear();
        Ok(())
    }

    fn encode_gpu_inner(&mut self, frame: &GpuFrame, force_key: bool) -> Result<Option<EncodedAu>> {
        let av_frame = AvFrameGuard::new()?;
        let ret = unsafe { av::av_hwframe_get_buffer(self.hw_frames_ctx.0, av_frame.0, 0) };
        if ret < 0 {
            bail!("av_hwframe_get_buffer 失败 (ret={ret})");
        }

        let dst_raw = unsafe { (*av_frame.0).data[0] as *mut core::ffi::c_void };
        let dst_slice = unsafe { (*av_frame.0).data[1] as isize as u32 };
        let dst_tex = unsafe { ID3D11Texture2D::from_raw_borrowed(&dst_raw) }
            .ok_or_else(|| anyhow!("av_hwframe_get_buffer 返回空 ID3D11Texture2D"))?;

        // GPU 显存内一步完成客户区裁剪 + BGRA→NV12 色彩转换！
        self.converter
            .blit_bgra_to_nv12(&frame.texture, frame.crop, dst_tex, dst_slice)?;

        let pts_us = match self.last_pts_us {
            Some(prev) if frame.pts_us <= prev => prev.saturating_add(1),
            _ => frame.pts_us,
        };
        self.last_pts_us = Some(pts_us);

        unsafe {
            (*av_frame.0).pts = pts_us as i64;
            (*av_frame.0).pict_type = if force_key {
                av::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                av::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            (*av_frame.0).colorspace = av::AVColorSpace::AVCOL_SPC_BT709;
            (*av_frame.0).color_primaries = av::AVColorPrimaries::AVCOL_PRI_BT709;
            (*av_frame.0).color_trc = av::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            (*av_frame.0).color_range = match self.color_profile {
                ColorProfile::Bt709Full => av::AVColorRange::AVCOL_RANGE_JPEG,
                ColorProfile::Bt709Limited => av::AVColorRange::AVCOL_RANGE_MPEG,
            };
        }

        let send_ret = unsafe { av::avcodec_send_frame(self.avctx.0, av_frame.0) };
        if send_ret < 0 && send_ret != AVERROR_EAGAIN {
            bail!("{} D3D11 send_frame 失败 (ret={send_ret})", self.codec_name);
        }

        loop {
            let recv_ret = unsafe { av::avcodec_receive_packet(self.avctx.0, self.pkt.0) };
            if recv_ret == AVERROR_EAGAIN || recv_ret == av::AVERROR_EOF {
                break;
            }
            if recv_ret < 0 {
                bail!(
                    "{} D3D11 receive_packet 失败 (ret={recv_ret})",
                    self.codec_name
                );
            }
            let (data, keyframe, out_pts) = unsafe {
                let p = &*self.pkt.0;
                let slice = if p.data.is_null() || p.size <= 0 {
                    &[]
                } else {
                    std::slice::from_raw_parts(p.data, p.size as usize)
                };
                let kf = (p.flags & av::AV_PKT_FLAG_KEY) != 0;
                let pts = if p.pts >= 0 { p.pts as u64 } else { pts_us };
                (slice.to_vec(), kf, pts)
            };
            unsafe { av::av_packet_unref(self.pkt.0) };
            if !data.is_empty() {
                self.pending.push_back(EncodedAu {
                    data,
                    keyframe,
                    pts_us: out_pts,
                    crop: self.crop,
                });
            }
        }

        Ok(self.pending.pop_front())
    }
}

impl VideoEncoder for D3d11HwEncoder {
    fn encode(&mut self, _frame: &RawFrame, _force_key: bool) -> Result<Option<EncodedAu>> {
        bail!(
            "{} 为 D3D11 GPU 零拷贝编码器，请使用 encode_gpu() 输入显存纹理",
            self.codec_name
        )
    }

    fn supports_gpu_input(&self) -> bool {
        true
    }

    fn encode_gpu(&mut self, frame: &GpuFrame, force_key: bool) -> Result<Option<EncodedAu>> {
        let (_, _, cw, ch) = frame.crop;
        if cw > self.enc_width || ch > self.enc_height {
            bail!(
                "GPU 输入帧 {}x{} 大于编码器 {}x{}，请先 reconfigure",
                cw,
                ch,
                self.enc_width,
                self.enc_height
            );
        }
        self.crop = (self.enc_width - cw, self.enc_height - ch);
        self.encode_gpu_inner(frame, force_key)
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        let kbps = kbps.max(1);
        self.bitrate_kbps = kbps;
        let base = self.applied_bitrate_kbps.max(1);
        let pct_diff = u64::from(kbps.abs_diff(base)) * 100 / u64::from(base);
        let should_rebuild = pct_diff >= 80
            || (pct_diff >= 25 && self.last_rebuild_at.elapsed() >= Duration::from_millis(1500));
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

/// 将外部 `ID3D11Device` 绑定进 FFmpeg `AVHWDeviceContext`（`AV_HWDEVICE_TYPE_D3D11VA`）。
fn create_d3d11_hw_device_ctx(device: &ID3D11Device) -> Result<AvBufferGuard> {
    let dev_ref =
        unsafe { av::av_hwdevice_ctx_alloc(av::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA) };
    if dev_ref.is_null() {
        bail!("av_hwdevice_ctx_alloc(D3D11VA) 失败");
    }
    let guard = AvBufferGuard(dev_ref);
    unsafe {
        let dev_ctx = (*dev_ref).data as *mut av::AVHWDeviceContext;
        let d3d11_ctx = (*dev_ctx).hwctx as *mut av::AVD3D11VADeviceContext;
        // 移交一份 AddRef 后的 ID3D11Device 所有权给 FFmpeg（FFmpeg 销毁上下文时自动 Release）。
        (*d3d11_ctx).device = device.clone().into_raw() as *mut av::ID3D11Device;
        let ret = av::av_hwdevice_ctx_init(dev_ref);
        if ret < 0 {
            bail!("av_hwdevice_ctx_init(D3D11VA) 失败 (ret={ret})");
        }
    }
    Ok(guard)
}

/// 基于 D3D11 设备上下文创建 NV12 硬件帧池（`AV_PIX_FMT_D3D11`）。
///
/// 使用 `initial_pool_size = 0` 分配独立非数组纹理（`ArraySize = 1`），
/// 规避 NVIDIA/D3D11 驱动拒绝 `DXGI_FORMAT_NV12` Texture2DArray 叠加
/// `D3D11_BIND_RENDER_TARGET` 的 `E_INVALIDARG (0x80070057)` 限制，同时兼容 MFT/NVENC/QSV/AMF。
fn create_d3d11_hw_frames_ctx(
    dev_ref: *mut av::AVBufferRef,
    enc_w: u32,
    enc_h: u32,
) -> Result<AvBufferGuard> {
    // 优先绑定 RENDER_TARGET（供 ID3D11VideoProcessorOutputView 直写 NV12）；
    // 若驱动拒绝则回退默认标志（0）。
    let bind_candidates = [
        D3D11_BIND_RENDER_TARGET.0 as u32,
        (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        0u32,
    ];
    let mut last_ret = -1;
    for bind_flags in bind_candidates {
        let frames_ref = unsafe { av::av_hwframe_ctx_alloc(dev_ref) };
        if frames_ref.is_null() {
            bail!("av_hwframe_ctx_alloc 失败");
        }
        let guard = AvBufferGuard(frames_ref);
        let ret = unsafe {
            let frames_ctx = (*frames_ref).data as *mut av::AVHWFramesContext;
            (*frames_ctx).format = av::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*frames_ctx).sw_format = av::AVPixelFormat::AV_PIX_FMT_NV12;
            (*frames_ctx).width = enc_w as i32;
            (*frames_ctx).height = enc_h as i32;
            (*frames_ctx).initial_pool_size = 0;

            let d3d11_frames = (*frames_ctx).hwctx as *mut av::AVD3D11VAFramesContext;
            (*d3d11_frames).BindFlags = bind_flags;

            av::av_hwframe_ctx_init(frames_ref)
        };
        if ret >= 0 {
            return Ok(guard);
        }
        last_ret = ret;
    }
    bail!(
        "av_hwframe_ctx_init(NV12 {}x{}) 失败 (ret={last_ret})",
        enc_w,
        enc_h
    )
}

/// 打开支持 `AV_PIX_FMT_D3D11` 的硬件编码器（依次尝试 Full/Limited 色彩范围与私有低延迟选项）。
fn open_d3d11_encoder(
    codec_name: &str,
    enc_w: u32,
    enc_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    frames_ref: *mut av::AVBufferRef,
) -> Result<(AvCodecCtxGuard, ColorProfile)> {
    let c_name = CString::new(codec_name).context("codec_name 含 NUL")?;
    let codec = unsafe { av::avcodec_find_encoder_by_name(c_name.as_ptr()) };
    if codec.is_null() {
        bail!("找不到编码器 {codec_name}");
    }

    let color_profiles: &[ColorProfile] = match codec_name {
        "h264_mf" | "hevc_mf" => &[ColorProfile::Bt709Limited, ColorProfile::Bt709Full],
        _ => &[ColorProfile::Bt709Full, ColorProfile::Bt709Limited],
    };
    let option_sets = codec_option_candidates(codec_name);
    let mut last_err = None;

    for &profile in color_profiles {
        for opts in &option_sets {
            match try_open_d3d11_codec(
                codec,
                codec_name,
                enc_w,
                enc_h,
                fps,
                bitrate_kbps,
                profile,
                frames_ref,
                opts,
            ) {
                Ok(ctx) => {
                    tracing::debug!(
                        codec = codec_name,
                        ?profile,
                        "D3D11 零拷贝硬编上下文打开成功"
                    );
                    return Ok((ctx, profile));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("{codec_name} D3D11 零拷贝打开失败")))
}

#[allow(clippy::too_many_arguments)]
fn try_open_d3d11_codec(
    codec: *const av::AVCodec,
    codec_name: &str,
    enc_w: u32,
    enc_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    color_profile: ColorProfile,
    frames_ref: *mut av::AVBufferRef,
    options: &[(&str, &str)],
) -> Result<AvCodecCtxGuard> {
    let ctx_ptr = unsafe { av::avcodec_alloc_context3(codec) };
    if ctx_ptr.is_null() {
        bail!("avcodec_alloc_context3({codec_name}) 失败");
    }
    let guard = AvCodecCtxGuard(ctx_ptr);

    unsafe {
        let c = &mut *ctx_ptr;
        c.width = enc_w as i32;
        c.height = enc_h as i32;
        c.pix_fmt = av::AVPixelFormat::AV_PIX_FMT_D3D11;
        c.sw_pix_fmt = av::AVPixelFormat::AV_PIX_FMT_NV12;
        let bit_rate = i64::from(bitrate_kbps).saturating_mul(1000);
        c.bit_rate = bit_rate;
        c.rc_max_rate = bit_rate;
        c.gop_size = 100_000;
        c.max_b_frames = 0;
        c.time_base = av::AVRational {
            num: 1,
            den: 1_000_000,
        };
        c.framerate = av::AVRational {
            num: fps.max(1) as i32,
            den: 1,
        };
        c.colorspace = av::AVColorSpace::AVCOL_SPC_BT709;
        c.color_primaries = av::AVColorPrimaries::AVCOL_PRI_BT709;
        c.color_trc = av::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        c.color_range = match color_profile {
            ColorProfile::Bt709Full => av::AVColorRange::AVCOL_RANGE_JPEG,
            ColorProfile::Bt709Limited => av::AVColorRange::AVCOL_RANGE_MPEG,
        };
        c.hw_frames_ctx = av::av_buffer_ref(frames_ref);
    }

    let mut av_dict: *mut av::AVDictionary = ptr::null_mut();
    for (k, v) in options {
        let ck = CString::new(*k)?;
        let cv = CString::new(*v)?;
        unsafe {
            av::av_dict_set(&mut av_dict, ck.as_ptr(), cv.as_ptr(), 0);
        }
    }

    let ret = unsafe { av::avcodec_open2(ctx_ptr, codec, &mut av_dict) };
    if !av_dict.is_null() {
        unsafe { av::av_dict_free(&mut av_dict) };
    }
    if ret < 0 {
        bail!("{codec_name} D3D11 avcodec_open2 失败 (ret={ret})");
    }
    Ok(guard)
}

/// 在 GPU 上创建一块 BGRA8 测试纹理（用于探测验证 D3D11 零拷贝硬编是否真实出帧）。
fn create_probe_bgra_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .context("创建 D3D11 探测 BGRA 纹理失败")?;
    tex.ok_or_else(|| anyhow!("CreateTexture2D 返回空纹理"))
}
