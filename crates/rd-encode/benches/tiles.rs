//! T-010 瓦片基准（criterion）。
//!
//! 指标目标（任务卡）：
//! - 1080p 全脏帧 `BlockGrid::update` < 3 ms；
//! - 文本区 256×256 QOI 编码 < 2 ms。
//!
//! 运行：`cargo bench -p rd-encode --bench tiles`

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use rd_encode::dirty::BlockGrid;
use rd_encode::frame::RawFrame;
use rd_encode::tiles::TileRefiner;

/// LCG 伪随机填充整帧。
fn fill_pseudo(frame: &mut RawFrame, seed: u64) {
    let mut s = seed | 1;
    for px in frame.data.as_chunks_mut::<4>().0 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        px.copy_from_slice(&[(s >> 33) as u8, (s >> 41) as u8, (s >> 49) as u8, 255]);
    }
}

fn make_frame(w: u32, h: u32, seed: u64) -> RawFrame {
    let mut f = RawFrame::new(w, h, 0);
    fill_pseudo(&mut f, seed);
    f
}

/// 文本样 256×256：白底 + 深灰"字形"条带 + 伪随机抗锯齿边缘（QOI 友好）。
fn text_like_frame(w: u32, h: u32) -> RawFrame {
    let mut f = RawFrame::new(w, h, 0);
    let mut s = 88172645463325252u64;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let text_row = (y % 20) < 12;
            let glyph = text_row && ((x / 4 + (y / 20) * 13) % 16) < 6;
            let edge = text_row && !glyph && ((x / 4 + (y / 20) * 13) % 16) == 6;
            let noise = {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 56) as u8
            };
            let (r, g, b) = if glyph {
                (30, 32, 36)
            } else if edge {
                (
                    120 + u32::from(noise % 40),
                    120 + u32::from(noise % 40),
                    124 + u32::from(noise % 40),
                )
            } else {
                (250, 250, 250)
            };
            let off = (y * f.stride as usize) + x * 4;
            f.data[off..off + 4].copy_from_slice(&[b as u8, g as u8, r as u8, 255]);
        }
    }
    f
}

/// 1080p 全脏帧块比对：交替两帧保证每轮全部块哈希失配（最坏路径）。
fn bench_update_1080p(c: &mut Criterion) {
    let (w, h) = (1920u32, 1080u32);
    let fa = make_frame(w, h, 0x1234_5678);
    let fb = make_frame(w, h, 0x9ABC_DEF0);
    let frames: [&RawFrame; 2] = [&fa, &fb];
    let mut grid = BlockGrid::new(w, h);
    grid.update(&fa, None); // 预置基线哈希

    c.bench_function("dirty/update_1080p_full", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let f = frames[i & 1];
            i += 1;
            black_box(grid.update(f, None))
        })
    });
}

/// 文本区 256×256：QOI 编码（任务卡指标 < 2 ms）。
fn bench_qoi_encode_256(c: &mut Criterion) {
    let frame = text_like_frame(256, 256);
    c.bench_function("tiles/qoi_encode_256x256_text", |b| {
        b.iter(|| black_box(qoi::encode_to_vec(black_box(&frame.data), 256, 256)))
    });
}

/// 文本区 256×256：精修 tick 全链路（块比对 + 抽取 + QOI/zstd 双编码取小）。
fn bench_tick_256_text(c: &mut Criterion) {
    let frame = text_like_frame(256, 256);
    let base = Instant::now();
    let matured = base + Duration::from_millis(300);
    c.bench_function("tiles/tick_refine_256x256_text", |b| {
        b.iter_batched(
            || {
                let mut r = TileRefiner::new(256, 256, 300);
                r.on_frame(&frame, None, base);
                r
            },
            |mut r| black_box(r.tick(&frame, matured, usize::MAX, None)),
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_update_1080p,
    bench_qoi_encode_256,
    bench_tick_256_text
);
criterion_main!(benches);
