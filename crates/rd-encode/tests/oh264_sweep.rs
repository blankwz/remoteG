//! OpenH264 编码参数扫描（手动工具，`#[ignore]` 不入常规测试）。
//!
//! 目的：验证软编单帧延迟最优的（线程数 × 复杂度）组合是否已选对。
//! OpenH264 软编是 1080p 软件路径的主导项，参数直接影响「每帧发送延迟」。
//!
//! 运行（release 才可信，debug 未优化会虚高）：
//! ```text
//! cargo test --release -p rd-encode --test oh264_sweep -- --ignored --nocapture
//! ```
//!
//! 结论（2026-11-05，本机 release，1920×1080 @4Mbps 固定内容，跳过前 4 帧）：
//! - **线程数近乎无关**：threads 1→16 时延 22→18ms，输出字节完全不变
//!   （2061）——openh264 逐帧 `encode_at` 不吃多线程，4 线程既不增也不减
//!   单帧延迟。当前 `num_threads(4)` 无改参价值。
//! - **复杂度是有用杠杆**：Low ~18.9ms < Medium ~23.5ms（快约 20%），
//!   当前选择 `Complexity::Low` 正确（低延迟/低字节取向）。
//! - 真实特征：**纯平文档内容软编 ~19ms/帧，贴近 20ms DoD 预算**；混合/噪声
//!   内容更低。软件兜底在超大纯平窗口下单帧延迟逼近预算——缓解靠硬编优先
//!   （已有）与静止帧 gating（R3，静止跳过编码）。
//!
//! ## 硬编 vs 软编（`bench_hardware_path`，用 `create_best` 生产接口）
//! - **硬编 h264_mf（本机生产路径）：7.2ms/帧；静止内容 P 帧仅 ~18B**
//!   （字节样本 `[100425, 3365, 18, 18, …]` = IDR→首 P→全跳过）。
//!   快且省字节，1080p60 余量充足，无码率超发。
//! - **软编 openh264：~23-31ms/帧；对相同静止内容仍发 3-17KB/帧**（不积极
//!   跳过）——4 倍慢且浪费带宽，更印证静止帧 gating（R3）对软编价值更大。

use std::time::Instant;

use openh264::encoder::{
    BitRate, Complexity, Encoder as Oh264Encoder, EncoderConfig as Oh264Config, FrameRate, Profile,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::OpenH264API;
use openh264::Timestamp;
use rd_encode::frame::{RawFrame, YuvPlanes};
use rd_encode::{create_best, EncoderConfig, PixelFormat};

/// 1080p 拟真"文档"帧：白底灰字形带 + 抗锯齿边缘 + 顶部彩色头带 + 噪声，
/// 同时覆盖平面与纹理区（贴近真实窗口内容对软编的负载）。
fn doc_frame(w: u32, h: u32) -> RawFrame {
    let mut f = RawFrame::new(w, h, 0);
    let mut s = 0x1234_5678_9abc_def0u64;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let header = y < (h / 10) as usize;
            let text_row = !header && (y % 24) < 16;
            let glyph = text_row && ((x / 5 + (y / 24) * 29) % 22) < 9;
            let edge = text_row && !glyph && ((x / 5 + (y / 24) * 29) % 22) == 9;
            let noise = {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 56) as u8
            };
            let (r, g, b) = if header {
                (
                    40u8.wrapping_add(noise % 60),
                    90u8.wrapping_add(noise % 50),
                    150u8.wrapping_add((y as u8 / 8) % 80),
                )
            } else if glyph {
                (28, 30, 34)
            } else if edge {
                (
                    110u8.wrapping_add(noise % 60),
                    112u8.wrapping_add(noise % 60),
                    118u8.wrapping_add(noise % 50),
                )
            } else {
                (249, 249, 249)
            };
            let off = y * f.stride as usize + x * 4;
            f.data[off..off + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    f
}

fn build(threads: u16, complexity: Complexity, kbps: u32) -> Oh264Encoder {
    let config = Oh264Config::new()
        .usage_type(UsageType::CameraVideoRealTime)
        .profile(Profile::Baseline)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(60.0))
        .skip_frames(false)
        .complexity(complexity)
        .num_threads(threads)
        .scene_change_detect(false)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .vui(VuiConfig::bt601());
    Oh264Encoder::with_api_config(OpenH264API::from_source(), config).expect("创建编码器")
}

#[test]
#[ignore]
fn sweep_openh264_threads_complexity() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const KBPS: u32 = 4000;
    const FRAMES: u32 = 16;

    let raw = doc_frame(W, H);
    let mut planes = YuvPlanes::new(W, H);
    raw.bgra_to_i420_into(&mut planes).expect("转换");

    println!(
        "\nOpenH264 扫参（{W}x{H}，{KBPS}Mbps，固定内容，跳过前 4 帧 warmup；release 才是可信值）"
    );
    println!("{:<26} {:>12} {:>10}", "config", "ms/frame", "bytes");
    println!("{}", "-".repeat(50));

    for complexity in [Complexity::Low, Complexity::Medium] {
        for threads in [1u16, 2, 4, 8, 16] {
            let mut enc = build(threads, complexity, KBPS);
            let mut t_acc = 0.0f64;
            let mut n = 0.0f64;
            let mut last_bytes = 0usize;
            for i in 0..FRAMES {
                let ts = Timestamp::from_millis(u64::from(i) * 33);
                let t0 = Instant::now();
                let bs = enc.encode_at(&planes, ts).expect("编码");
                let dt = t0.elapsed().as_secs_f64() * 1000.0;
                last_bytes = bs.to_vec().len();
                if i >= 4 {
                    t_acc += dt;
                    n += 1.0;
                }
            }
            let avg = t_acc / n.max(1.0);
            println!(
                "{:<26} {:>8.3}ms {:>10}",
                format!("threads={threads:>2}, complexity={complexity:?}"),
                avg,
                last_bytes
            );
        }
    }
}

/// `create_best` 生产选路路径的每帧延迟（本机硬编会落到 h264_mf；无硬编即
/// OpenH264 软编）。这才是真正跑在产线的路径——衡量硬编优先是否兑现低延迟。
#[test]
#[ignore]
fn bench_hardware_path() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const KBPS: u32 = 4000;
    const FRAMES: u32 = 16;

    let raw = doc_frame(W, H);
    for prefer_hw in [true, false] {
        let cfg = EncoderConfig {
            width: W,
            height: H,
            fps: 60,
            bitrate_kbps: KBPS,
            prefer_hw,
            pixel_format: PixelFormat::Yuv420,
        };
        let mut enc = create_best(&cfg).expect("创建编码器");
        let info = enc.info();
        let _ = enc.encode(&raw, true).expect("IDR 预热");

        // 硬编带缓冲延迟，前几帧返回 Ok(None) 属正常（生产循环同样容忍
        // au=None）。只对真正产出 AU 的帧计时。
        let mut t_acc = 0.0f64;
        let mut n = 0.0f64;
        let mut produced = 0u32;
        let mut i = 0u32;
        let mut bytes_samples: Vec<usize> = Vec::new();
        while produced < FRAMES {
            let t0 = Instant::now();
            let au = enc.encode(&raw, false).expect("编码");
            let dt = t0.elapsed().as_secs_f64() * 1000.0;
            if let Some(au) = au {
                produced += 1;
                if bytes_samples.len() < 10 {
                    bytes_samples.push(au.data.len());
                }
                if produced > 4 {
                    t_acc += dt;
                    n += 1.0;
                }
            }
            i += 1;
            if i > 200 {
                panic!("编码器长时间不产出 AU（{info:?} 仍未流动）");
            }
        }
        let avg = t_acc / n.max(1.0);
        println!(
            "[create_best] prefer_hw={prefer_hw:<5} → {}（hardware={}）: {avg:>7.3}ms/帧  字节样本(mid10)={bytes_samples:?}",
            info.name, info.hardware
        );
    }
}
