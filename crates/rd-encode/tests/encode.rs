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

/// `set_bitrate` 运行中生效（OpenH264 SetOption 路径）。
#[test]
fn set_bitrate_takes_effect() {
    let mut enc = OpenH264Encoder::new(&test_config(320, 240)).expect("OpenH264 初始化");
    enc.encode(&make_frame(320, 240, 0), false)
        .expect("首帧编码");
    enc.set_bitrate(4_000).expect("调整码率");
    // 调整后继续编码若干帧不报错。
    for i in 1..5u64 {
        enc.encode(&make_frame(320, 240, i), false)
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
