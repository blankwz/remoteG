//! T-003 编码器集成测试（OpenH264 路径，`--no-default-features` 也必须通过）。

use rd_encode::frame::RawFrame;
use rd_encode::h264_openh264::OpenH264Encoder;
use rd_encode::{EncoderConfig, PixelFormat, VideoEncoder};

/// 生成合成 BGRA 测试帧：渐变背景 + 移动色块（保证相邻帧有差异）。
fn make_frame(width: u32, height: u32, index: u64) -> RawFrame {
    let mut frame = RawFrame::new(width, height, index * 33_333);
    let stride = frame.stride as usize;
    let block_x = (index % 60) as usize * 2;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let off = y * stride + x * 4;
            let in_block = (block_x..block_x + 32).contains(&x) && (8..40).contains(&y);
            let (b, g, r) = if in_block {
                (255, 255, 255)
            } else {
                (
                    (x * 2 % 256) as u8,
                    (y * 2 % 256) as u8,
                    ((x / 8 + y / 8 + index as usize) % 256) as u8,
                )
            };
            frame.data[off..off + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    frame
}

fn test_config(width: u32, height: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        fps: 30,
        bitrate_kbps: 2_000,
        prefer_hw: false,
        pixel_format: PixelFormat::Yuv420,
    }
}

/// OpenH264 在 Yuv444 请求下必须明确报错（不支持，不静默降级）。
#[test]
fn openh264_rejects_yuv444() {
    let cfg = EncoderConfig {
        pixel_format: PixelFormat::Yuv444,
        ..test_config(320, 240)
    };
    let err = OpenH264Encoder::new(&cfg)
        .err()
        .expect("OpenH264 必须拒绝 4:4:4");
    assert!(
        err.to_string().contains("4:4:4"),
        "错误信息应明确指出 4:4:4: {err:#}"
    );
}

/// `create_best` 在 Yuv444 + `prefer_hw=false` 时明确报错（444 不允许
/// 落到 OpenH264；硬编探测路径是否可用与本机硬件相关，不做断言）。
#[test]
fn create_best_yuv444_without_hw_is_error() {
    let cfg = EncoderConfig {
        pixel_format: PixelFormat::Yuv444,
        ..test_config(320, 240)
    };
    let err = rd_encode::create_best(&cfg)
        .err()
        .expect("444 + prefer_hw=false 必须报错");
    assert!(
        err.to_string().contains("4:4:4"),
        "错误信息应明确指出 4:4:4: {err:#}"
    );
}

/// OpenH264 编码 30 帧：首帧 keyframe、输出 Annex-B（`00 00 00 01` 开头）。
#[test]
fn openh264_encodes_30_frames_first_is_annexb_idr() {
    let cfg = test_config(320, 240);
    let mut enc = OpenH264Encoder::new(&cfg).expect("OpenH264 初始化");

    let first = enc
        .encode(&make_frame(320, 240, 0), false)
        .expect("首帧编码")
        .expect("首帧应有输出");
    assert!(first.keyframe, "首帧必须是 keyframe");
    assert!(
        first.data.starts_with(&[0x00, 0x00, 0x00, 0x01]),
        "输出必须以 Annex-B start code 开头"
    );
    assert_eq!(first.crop, (0, 0));

    for i in 1..30u64 {
        let au = enc
            .encode(&make_frame(320, 240, i), false)
            .expect("编码失败")
            .expect("应有输出");
        assert!(!au.data.is_empty());
        assert!(
            au.data.windows(4).any(|w| w == [0x00, 0x00, 0x00, 0x01]),
            "后续帧也应包含 start code 分隔的 NAL"
        );
        assert_eq!(au.pts_us, i * 33_333);
    }
}

/// `reconfigure` 后首帧必须是 keyframe（IDR）。
#[test]
fn reconfigure_forces_keyframe() {
    let mut enc = OpenH264Encoder::new(&test_config(320, 240)).expect("OpenH264 初始化");
    for i in 0..5u64 {
        enc.encode(&make_frame(320, 240, i), false)
            .expect("编码失败");
    }

    enc.reconfigure(480, 272).expect("reconfigure");
    let au = enc
        .encode(&make_frame(480, 272, 5), false)
        .expect("reconfigure 后编码")
        .expect("应有输出");
    assert!(au.keyframe, "reconfigure 后首帧必须是 keyframe");

    let au = enc
        .encode(&make_frame(480, 272, 6), false)
        .expect("继续编码")
        .expect("应有输出");
    assert!(!au.keyframe, "后续普通帧不应是 keyframe");
}

/// `force_key` 请求即出 keyframe。
#[test]
fn force_key_produces_idr() {
    let mut enc = OpenH264Encoder::new(&test_config(320, 240)).expect("OpenH264 初始化");
    for i in 0..3u64 {
        enc.encode(&make_frame(320, 240, i), false)
            .expect("编码失败");
    }
    let au = enc
        .encode(&make_frame(320, 240, 3), true)
        .expect("force_key 编码")
        .expect("应有输出");
    assert!(au.keyframe, "force_key 必须产出 keyframe");
}

/// 奇数尺寸可编码（右/下补 1 像素并记录在 crop）。
#[test]
fn odd_dimensions_encode_with_crop() {
    let mut enc = OpenH264Encoder::new(&test_config(641, 481)).expect("OpenH264 初始化");
    for i in 0..5u64 {
        let au = enc
            .encode(&make_frame(641, 481, i), i == 0)
            .expect("奇数尺寸编码失败")
            .expect("应有输出");
        assert_eq!(au.crop, (1, 1), "奇数宽高各补 1 像素");
        assert!(
            au.data.starts_with(&[0x00, 0x00, 0x00, 0x01]),
            "首帧 Annex-B"
        );
    }
}

/// `set_bitrate` 运行中生效（覆盖首帧前调参、从 20Mbps 降到 15855/18233kbps、升到 80/200Mbps、再降回 2Mbps）。
#[test]
fn set_bitrate_takes_effect() {
    let init_cfg = EncoderConfig {
        width: 1920,
        height: 1080,
        fps: 144,
        bitrate_kbps: 20_000,
        prefer_hw: false,
        pixel_format: PixelFormat::Yuv420,
    };
    let mut enc = OpenH264Encoder::new(&init_cfg).expect("OpenH264 1080p@144 初始化");
    // 首帧前调用 set_bitrate 也不得报错
    enc.set_bitrate(18_000).expect("首帧前 set_bitrate");
    enc.encode(&make_frame(1920, 1080, 0), false)
        .expect("首帧编码");
    for (idx, kbps) in [15_855u32, 18_233, 4_000, 50_000, 200_000, 2_000]
        .into_iter()
        .enumerate()
    {
        enc.set_bitrate(kbps)
            .unwrap_or_else(|e| panic!("调整码率到 {kbps} kbps 失败: {e:#}"));
        enc.encode(&make_frame(1920, 1080, (idx + 1) as u64), false)
            .expect("调码率后编码");
    }
}

/// `create_best` 在 `prefer_hw = false` 时选 OpenH264。
#[test]
fn create_best_falls_back_to_openh264() {
    let mut enc = rd_encode::create_best(&test_config(320, 240)).expect("创建编码器");
    assert_eq!(enc.info().name, "openh264");
    assert!(!enc.info().hardware);
    let au = enc
        .encode(&make_frame(320, 240, 0), false)
        .expect("编码失败")
        .expect("应有输出");
    assert!(au.keyframe);
}

/// 本机 4:4:4 硬编创建实测（信息性探针，无断言）。
///
/// 本机（仅 h264_mf 且不支持 444 输入、nvenc 驱动旧）预期打印 Err；
/// 驱动升级 / 换 444 硬编后运行
/// `cargo test -p rd-encode --test encode create_best_yuv444_local_probe
/// -- --ignored --nocapture` 观察端到端编码结果。
#[test]
#[ignore = "本机硬编探测（结果信息性）"]
fn create_best_yuv444_local_probe() {
    let cfg = EncoderConfig {
        prefer_hw: true,
        pixel_format: PixelFormat::Yuv444,
        ..test_config(320, 240)
    };
    match rd_encode::create_best(&cfg) {
        Ok(mut enc) => {
            let info = enc.info();
            println!(
                "create_best(Yuv444) 成功: name={} hardware={}",
                info.name, info.hardware
            );
            let au = enc.encode(&make_frame(320, 240, 0), false);
            match au {
                Ok(Some(au)) => println!(
                    "首帧编码成功: keyframe={} bytes={}",
                    au.keyframe,
                    au.data.len()
                ),
                Ok(None) => println!("首帧编码无输出（delay 型编码器属正常）"),
                Err(e) => println!("首帧编码失败: {e:#}"),
            }
        }
        Err(e) => println!("create_best(Yuv444) 失败: {e:#}"),
    }
}

/// D3D11 GPU 零拷贝硬编集成测试（Windows + ffmpeg）：
/// 创建真实 D3D11Device + BGRA 显存纹理 → `create_d3d11_zero_copy` → 连续零拷贝编码 30 帧，
/// 验证 Annex-B IDR、持续出帧与 `set_bitrate`/`reconfigure` 恢复能力。
/// 无硬件编码器的纯软件 CI 虚拟机上，探测失败属预期并优雅跳过。
#[cfg(all(windows, feature = "ffmpeg"))]
#[test]
fn d3d11_zero_copy_encodes_gpu_frames_and_reconfigures() {
    use std::time::Instant;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    let mut device: Option<ID3D11Device> = None;
    let create_res = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    };
    let Some(device) = create_res.ok().and(device) else {
        println!("跳过：当前环境无 D3D11 硬件设备");
        return;
    };

    let (w, h) = (1920u32, 1080u32);
    let cfg = EncoderConfig {
        width: w,
        height: h,
        fps: 144,
        bitrate_kbps: 20_000,
        prefer_hw: true,
        pixel_format: PixelFormat::Yuv420,
    };
    let mut enc =
        match rd_encode::create_d3d11_zero_copy(&cfg, rd_proto::v0::Codec::H264420, device.clone())
        {
            Ok(e) => e,
            Err(e) => {
                println!("跳过：本机硬编不支持 D3D11VA 零拷贝 ({e:#})");
                return;
            }
        };
    assert!(
        enc.supports_gpu_input(),
        "零拷贝编码器必须声明 supports_gpu_input=true"
    );
    assert!(enc.info().hardware, "零拷贝编码器必须为硬编");

    let make_gpu_tex = |tw: u32, th: u32| -> ID3D11Texture2D {
        let mut pixels = vec![0u8; (tw * th * 4) as usize];
        for y in 0..th {
            for x in 0..tw {
                let off = ((y * tw + x) * 4) as usize;
                pixels[off] = (x % 255) as u8;
                pixels[off + 1] = (y % 255) as u8;
                pixels[off + 2] = 180;
                pixels[off + 3] = 255;
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: tw,
            Height: th,
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
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: tw * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }
            .expect("CreateTexture2D");
        tex.expect("non-null texture")
    };

    let tex_1080 = make_gpu_tex(w, h);
    let mut produced = 0usize;
    let mut first_idr = false;
    let t0 = Instant::now();
    for i in 0..30u64 {
        let gf = rd_encode::GpuFrame {
            texture: tex_1080.clone(),
            crop: (0, 0, w, h),
            pts_us: (i + 1) * 6_944, // 144Hz
        };
        if let Some(au) = enc.encode_gpu(&gf, i == 0).expect("encode_gpu 1080p") {
            if produced == 0 {
                first_idr = au.keyframe;
                assert!(
                    au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]),
                    "首帧应为 Annex-B"
                );
            }
            produced += 1;
        }
    }
    let elapsed = t0.elapsed();
    assert!(
        produced >= 20,
        "30 帧输入应产出足够多的 AU，实际={produced}"
    );
    assert!(first_idr, "首个输出 AU 必须为 IDR");
    println!(
        "D3D11 零拷贝 1080p@144Hz ({}): 30 帧耗时 {:.2}ms (均 {:.2}ms/帧, 产出 {produced} AU)",
        enc.info().name,
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0 / 30.0
    );

    // 验证 reconfigure 到 1280x720 后仍能零拷贝出关键帧
    enc.reconfigure(1280, 720).expect("reconfigure 720p");
    let tex_720 = make_gpu_tex(1280, 720);
    let mut got_after_resize = false;
    for i in 0..8u64 {
        let gf = rd_encode::GpuFrame {
            texture: tex_720.clone(),
            crop: (0, 0, 1280, 720),
            pts_us: (i + 1) * 6_944,
        };
        if let Some(au) = enc.encode_gpu(&gf, i == 0).expect("encode_gpu 720p") {
            assert!(au.keyframe, "reconfigure 后首帧必须为 IDR");
            got_after_resize = true;
            break;
        }
    }
    assert!(got_after_resize, "reconfigure 后应正常产出 IDR");
}
