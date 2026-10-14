//! 合成画面 → out.h264（可用 `ffplay out.h264` 播放）。
//!
//! 生成 300 帧滚动文字色块（1080p 默认），统计单帧编码耗时并打印
//! `EncoderInfo`。默认走 `create_best`（硬编优先，回退 OpenH264）；
//! `--sw` 强制 OpenH264，`--codec h264_mf` 强制指定硬编候选。
//!
//! ```text
//! cargo run -p rd-encode --release --example encode_file -- --sw
//! cargo run -p rd-encode --release --example encode_file
//! ```

use std::io::Write as _;
use std::time::Instant;

use rd_encode::frame::RawFrame;
use rd_encode::{create_best, EncoderConfig};

/// 5x7 点阵字体（列位图，bit0 = 顶行），覆盖 ASCII 可见子集。
const FONT: &[(char, [u8; 5])] = &[
    ('A', [0x7C, 0x12, 0x11, 0x12, 0x7C]),
    ('B', [0x7F, 0x49, 0x49, 0x49, 0x36]),
    ('C', [0x3E, 0x41, 0x41, 0x41, 0x22]),
    ('D', [0x7F, 0x41, 0x41, 0x22, 0x1C]),
    ('E', [0x7F, 0x49, 0x49, 0x49, 0x41]),
    ('F', [0x7F, 0x09, 0x09, 0x09, 0x01]),
    ('G', [0x3E, 0x41, 0x49, 0x49, 0x7A]),
    ('H', [0x7F, 0x08, 0x08, 0x08, 0x7F]),
    ('I', [0x00, 0x41, 0x7F, 0x41, 0x00]),
    ('K', [0x7F, 0x08, 0x14, 0x22, 0x41]),
    ('L', [0x7F, 0x40, 0x40, 0x40, 0x40]),
    ('M', [0x7F, 0x02, 0x0C, 0x02, 0x7F]),
    ('N', [0x7F, 0x04, 0x08, 0x10, 0x7F]),
    ('O', [0x3E, 0x41, 0x41, 0x41, 0x3E]),
    ('P', [0x7F, 0x09, 0x09, 0x09, 0x06]),
    ('R', [0x7F, 0x09, 0x19, 0x29, 0x46]),
    ('S', [0x46, 0x49, 0x49, 0x49, 0x31]),
    ('T', [0x01, 0x01, 0x7F, 0x01, 0x01]),
    ('V', [0x1F, 0x20, 0x40, 0x20, 0x1F]),
    ('W', [0x3F, 0x40, 0x38, 0x40, 0x3F]),
    ('X', [0x63, 0x14, 0x08, 0x14, 0x63]),
    ('Y', [0x07, 0x08, 0x70, 0x08, 0x07]),
    ('0', [0x3E, 0x51, 0x49, 0x45, 0x3E]),
    ('1', [0x00, 0x42, 0x7F, 0x40, 0x00]),
    ('2', [0x42, 0x61, 0x51, 0x49, 0x46]),
    ('3', [0x21, 0x41, 0x45, 0x4B, 0x31]),
    ('4', [0x18, 0x14, 0x12, 0x7F, 0x10]),
    ('5', [0x27, 0x45, 0x45, 0x45, 0x39]),
    ('6', [0x3C, 0x4A, 0x49, 0x49, 0x30]),
    ('7', [0x01, 0x71, 0x09, 0x05, 0x03]),
    ('8', [0x36, 0x49, 0x49, 0x49, 0x36]),
    ('9', [0x06, 0x49, 0x49, 0x29, 0x1E]),
    ('-', [0x08, 0x08, 0x08, 0x08, 0x08]),
    ('.', [0x00, 0x60, 0x60, 0x00, 0x00]),
    (' ', [0x00, 0x00, 0x00, 0x00, 0x00]),
];

/// 在 BGRA 帧上绘制一行缩放文字（白色，带深色描边底）。
fn draw_text(frame: &mut RawFrame, text: &str, x0: i64, y0: i64, scale: u32) {
    let stride = frame.stride as usize;
    let (w, h) = (frame.width as i64, frame.height as i64);
    for (i, ch) in text.chars().enumerate() {
        let Some((_, glyph)) = FONT.iter().find(|(c, _)| *c == ch) else {
            continue;
        };
        let gx = x0 + (i as i64) * 6 * scale as i64;
        for col in 0..5u32 {
            for row in 0..7u32 {
                if glyph[col as usize] & (1 << row) == 0 {
                    continue;
                }
                for dy in 0..scale as i64 {
                    for dx in 0..scale as i64 {
                        let x = gx + col as i64 * scale as i64 + dx;
                        let y = y0 + row as i64 * scale as i64 + dy;
                        if x < 0 || y < 0 || x >= w || y >= h {
                            continue;
                        }
                        let off = y as usize * stride + x as usize * 4;
                        frame.data[off..off + 4].copy_from_slice(&[255, 255, 255, 255]);
                    }
                }
            }
        }
    }
}

struct Args {
    out: String,
    width: u32,
    height: u32,
    frames: u32,
    fps: u32,
    bitrate_kbps: u32,
    force_sw: bool,
    codec: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        out: "out.h264".into(),
        width: 1920,
        height: 1080,
        frames: 300,
        fps: 60,
        bitrate_kbps: 4_000,
        force_sw: false,
        codec: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sw" => args.force_sw = true,
            "--out" => args.out = it.next().unwrap_or(args.out),
            "--width" => args.width = it.next().and_then(|v| v.parse().ok()).unwrap_or(args.width),
            "--height" => {
                args.height = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.height)
            }
            "--frames" => {
                args.frames = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.frames)
            }
            "--fps" => args.fps = it.next().and_then(|v| v.parse().ok()).unwrap_or(args.fps),
            "--bitrate" => {
                args.bitrate_kbps = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.bitrate_kbps)
            }
            "--codec" => args.codec = it.next(),
            other => {
                eprintln!("未知参数 {other}；支持 --sw --out --width --height --frames --fps --bitrate --codec");
            }
        }
    }
    args
}

/// 生成第 `i` 帧合成画面：深色纯背景 + 滚动文字 + 移动色块（贴近
/// 窗口远程桌面的真实内容分布，文字可辨认）。
fn synth_frame(width: u32, height: u32, i: u64) -> RawFrame {
    let mut frame = RawFrame::new(width, height, i * 1_000_000 / 60);
    let stride = frame.stride as usize;
    // 深色纯背景（BT.601 有限范围黑，Y≈16）。
    for px in frame.data.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&[24, 28, 32, 255]);
    }
    // 顶部静态标题。
    draw_text(&mut frame, "RD-H264 ENCODE TEST", 64, 64, 8);
    // 滚动文字。
    let text = format!("FRAME {i:04} - 1080P60 SCREEN CONTENT");
    let text_w = (text.len() * 6 * 8) as i64;
    let scroll = (i as i64 * 8) % (width as i64 + text_w);
    draw_text(
        &mut frame,
        &text,
        width as i64 - scroll,
        height as i64 / 2 - 28,
        8,
    );
    // 移动色块。
    let bx = (i as usize * 7) % (width as usize).saturating_sub(160).max(1);
    let by = (i as usize * 3) % (height as usize).saturating_sub(160).max(1);
    for y in by..by + 140 {
        for x in bx..bx + 140 {
            let off = y * stride + x * 4;
            frame.data[off..off + 4].copy_from_slice(&[40, 220, 250, 255]);
        }
    }
    frame
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args();
    let cfg = EncoderConfig {
        width: args.width,
        height: args.height,
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
        prefer_hw: !args.force_sw,
    };

    let t0 = Instant::now();
    let mut enc = if let Some(name) = args.codec.as_deref() {
        #[cfg(all(windows, feature = "ffmpeg"))]
        {
            use rd_encode::VideoEncoder;
            anyhow::ensure!(!args.force_sw, "--sw 与 --codec 互斥");
            let mut e = rd_encode::h264_ffmpeg::FfmpegEncoder::create(&cfg, Some(name))?;
            e.encode(&synth_frame(cfg.width, cfg.height, 0), false)?; // 预热
            Box::new(e) as Box<dyn VideoEncoder>
        }
        #[cfg(not(all(windows, feature = "ffmpeg")))]
        {
            let _ = name;
            anyhow::bail!(
                "本构建未启用 ffmpeg 硬编（非 Windows 或未启用 feature），无法指定 --codec"
            );
        }
    } else {
        create_best(&cfg)?
    };
    let setup = t0.elapsed();

    let info = enc.info();
    let mut file = std::io::BufWriter::new(std::fs::File::create(&args.out)?);
    let mut total_bytes = 0u64;
    let mut keyframes = 0u32;
    let mut buffered_frames = 0u32;
    let mut samples: Vec<u32> = Vec::with_capacity(args.frames as usize);

    let emit = |au: Option<rd_encode::EncodedAu>,
                file: &mut std::io::BufWriter<std::fs::File>,
                samples: &mut Vec<u32>,
                total_bytes: &mut u64,
                keyframes: &mut u32,
                buffered_frames: &mut u32,
                elapsed_ms: u32|
     -> anyhow::Result<()> {
        match au {
            Some(au) => {
                samples.push(elapsed_ms.max(1));
                if au.keyframe {
                    *keyframes += 1;
                }
                *total_bytes += au.data.len() as u64;
                file.write_all(&au.data)?;
            }
            None => {
                // delay 型编码器（h264_mf 等）流水线中的帧暂无输出。
                *buffered_frames += 1;
            }
        }
        Ok(())
    };

    for i in 0..args.frames as u64 {
        let frame = synth_frame(cfg.width, cfg.height, i);
        let start = Instant::now();
        let au = enc.encode(&frame, i == 0 || i == 100)?;
        let elapsed = (start.elapsed().as_secs_f64() * 1000.0) as u32;
        emit(
            au,
            &mut file,
            &mut samples,
            &mut total_bytes,
            &mut keyframes,
            &mut buffered_frames,
            elapsed,
        )?;
    }
    // 冲刷 delay 型编码器流水线：重复提交末帧直到无输出（最多 64 次）。
    let mut flushed = 0u64;
    for i in args.frames as u64..args.frames as u64 + 64 {
        let frame = synth_frame(cfg.width, cfg.height, i);
        let start = Instant::now();
        let au = enc.encode(&frame, false)?;
        let elapsed = (start.elapsed().as_secs_f64() * 1000.0) as u32;
        let before = total_bytes;
        emit(
            au,
            &mut file,
            &mut samples,
            &mut total_bytes,
            &mut keyframes,
            &mut buffered_frames,
            elapsed,
        )?;
        flushed += 1;
        if total_bytes == before {
            break;
        }
    }
    file.flush()?;
    samples.sort_unstable();

    let produced = samples.len();
    let n = produced as f64;
    let avg = samples.iter().map(|v| *v as f64).sum::<f64>() / n;
    let p50 = samples[produced / 2];
    let p99 = samples[(produced as f64 * 0.99) as usize % produced];
    let max = *samples.last().expect("非空");

    println!(
        "encoder          : {} (hardware={})",
        info.name, info.hardware
    );
    println!(
        "resolution       : {}x{} @ {} fps, target {} kbps",
        cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps
    );
    println!(
        "frames           : {} submitted, {} AUs out, {} in flight at EOF, keyframes: {} (frame 0/100 forced IDR)",
        args.frames + flushed as u32,
        produced,
        buffered_frames,
        keyframes
    );
    println!(
        "encode ms  [avg/p50/p99/max]: {avg:.2} / {p50} / {p99} / {max}   (含 BGRA→{} 转换)",
        if info.hardware { "NV12" } else { "I420" }
    );
    println!("setup/probe time : {setup:.2?}");
    println!(
        "output           : {} ({:.2} MiB, avg {} kbps)",
        args.out,
        total_bytes as f64 / 1024.0 / 1024.0,
        total_bytes * 8 * u64::from(args.fps) / produced as u64 / 1000
    );
    println!("playback         : ffplay {} -vf format=yuv420p", args.out);
    Ok(())
}
