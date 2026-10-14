//! T-010 无损瓦片集成测试：静止精修时序、逐像素一致性、失效、预算、尺寸重建。

use std::time::{Duration, Instant};

use rd_encode::dirty::{BlockGrid, Rect};
use rd_encode::frame::RawFrame;
use rd_encode::tiles::{decode_tile, extract_bgra, TileOut, TileRefiner, FMT_INVALIDATE};

const STILL_MS: u32 = 300;

/// LCG 伪随机填充整帧（相邻 seed 之间每个像素都不同）。
fn pattern_frame(w: u32, h: u32, seed: u64) -> RawFrame {
    let mut f = RawFrame::new(w, h, 0);
    let mut s = seed | 1;
    for px in f.data.as_chunks_mut::<4>().0 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *px = [(s >> 33) as u8, (s >> 41) as u8, (s >> 49) as u8, 255];
    }
    f
}

/// 覆盖写入矩形区域为另一伪随机内容（用于制造变化）。
fn overwrite_region(f: &mut RawFrame, rect: Rect, seed: u64) {
    let mut s = seed | 1;
    for row in 0..rect.h as usize {
        let start = (rect.y as usize + row) * f.stride as usize + rect.x as usize * 4;
        for px in f.data[start..start + rect.w as usize * 4]
            .as_chunks_mut::<4>()
            .0
        {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            px.copy_from_slice(&[(s >> 33) as u8, (s >> 41) as u8, (s >> 49) as u8, 255]);
        }
    }
}

fn rect_of(t: &TileOut) -> Rect {
    Rect::new(
        u32::from(t.x),
        u32::from(t.y),
        u32::from(t.w),
        u32::from(t.h),
    )
}

/// 校验每张无损瓦片解码后与其覆盖的源帧区域逐像素一致。
fn assert_tiles_match_source(frame: &RawFrame, tiles: &[TileOut]) {
    for t in tiles {
        assert_ne!(t.fmt, FMT_INVALIDATE, "精修输出不应包含失效标记");
        let dec = decode_tile(t)
            .unwrap_or_else(|e| panic!("瓦片 ({},{},{},{}) 解码失败: {e:#}", t.x, t.y, t.w, t.h));
        assert_eq!(
            dec.bgra,
            extract_bgra(frame, &rect_of(t)),
            "瓦片 ({},{},{},{}) 与源帧不一致",
            t.x,
            t.y,
            t.w,
            t.h
        );
    }
}

#[test]
fn still_300ms_then_lossless_tiles_pixel_exact() {
    let (w, h) = (256u32, 128u32);
    let frame = pattern_frame(w, h, 42);
    let mut refiner = TileRefiner::new(w, h, STILL_MS);
    let t0 = Instant::now();

    let inv = refiner.on_frame(&frame, None, t0);
    assert!(inv.is_empty(), "首帧不应有失效");

    // 静止不足 300ms：无精修输出。
    assert!(refiner.tick(&frame, t0, usize::MAX, None).is_empty());
    assert!(refiner
        .tick(
            &frame,
            t0 + Duration::from_millis(STILL_MS as u64 - 1),
            usize::MAX,
            None
        )
        .is_empty());

    let tiles = refiner.tick(
        &frame,
        t0 + Duration::from_millis(u64::from(STILL_MS)),
        usize::MAX,
        None,
    );
    assert!(!tiles.is_empty(), "静止满 300ms 应产出瓦片");

    // 全帧 4x2 块都要被覆盖（8 块，行内最多 4 块/run → ≥2 张）。
    let covered: usize = tiles
        .iter()
        .map(|t| u32::from(t.w) as usize * u32::from(t.h) as usize / (64 * 64))
        .sum();
    assert_eq!(covered, 8, "应覆盖全部 8 块，实际 {covered}");
    assert_tiles_match_source(&frame, &tiles);

    // 已精修的块不再重复输出。
    assert!(refiner
        .tick(&frame, t0 + Duration::from_millis(1000), usize::MAX, None)
        .is_empty());
}

#[test]
fn refined_region_change_emits_invalidate_then_rerefines() {
    let (w, h) = (256u32, 128u32);
    let mut frame = pattern_frame(w, h, 7);
    let mut refiner = TileRefiner::new(w, h, STILL_MS);
    let t0 = Instant::now();
    refiner.on_frame(&frame, None, t0);
    let t1 = t0 + Duration::from_millis(u64::from(STILL_MS));
    assert!(
        !refiner.tick(&frame, t1, usize::MAX, None).is_empty(),
        "首精修"
    );

    // 上半行（4 块 256x64）变化 → 失效标记合并为一张 256x64。
    let hit = Rect::new(0, 0, 256, 64);
    overwrite_region(&mut frame, hit, 0xC0FF_EE00);
    let t2 = t1 + Duration::from_millis(10);
    let inv = refiner.on_frame(&frame, None, t2);
    assert_eq!(inv.len(), 1, "行内相邻失效块应合并为一张");
    let inv_tile = &inv[0];
    assert_eq!(inv_tile.fmt, FMT_INVALIDATE);
    assert_eq!(
        (inv_tile.x, inv_tile.y, inv_tile.w, inv_tile.h),
        (0, 0, 256, 64)
    );
    assert!(inv_tile.data.is_empty(), "失效瓦片 data 必须为空");
    assert!(decode_tile(inv_tile).is_err(), "失效瓦片不可解码");

    // 变化重置静止计时：立刻 tick 无输出。
    assert!(refiner.tick(&frame, t2, usize::MAX, None).is_empty());

    // 再次静止满 300ms → 重新精修，逐像素一致。
    let t3 = t2 + Duration::from_millis(u64::from(STILL_MS));
    let tiles = refiner.tick(&frame, t3, usize::MAX, None);
    assert!(!tiles.is_empty());
    assert_tiles_match_source(&frame, &tiles);
}

#[test]
fn budget_limits_output_and_defers_rest() {
    let (w, h) = (256u32, 512u32); // 4x8 = 32 块 → 8 个 run
    let frame = pattern_frame(w, h, 99);
    let t0 = Instant::now();
    let matured = t0 + Duration::from_millis(u64::from(STILL_MS));

    // 不限预算时全量输出：记录张数与总字节数。
    let mut generous_r = TileRefiner::new(w, h, STILL_MS);
    generous_r.on_frame(&frame, None, t0);
    let mut generous = generous_r.tick(&frame, matured, usize::MAX, None);
    assert_eq!(generous.len(), 8);
    let total: usize = generous.iter().map(|t| t.data.len()).sum();
    assert!(generous_r
        .tick(&frame, matured + Duration::from_millis(1), usize::MAX, None)
        .is_empty());

    // 预算 0 / 不足以装一张：全部留到下一 tick。
    let mut r1 = TileRefiner::new(w, h, STILL_MS);
    r1.on_frame(&frame, None, t0);
    assert!(r1.tick(&frame, matured, 0, None).is_empty());
    assert!(r1.tick(&frame, matured, 1, None).is_empty());

    // 预算严格等于总量：一次恰好装满 8 张，且与不限预算结果一致。
    let mut r2 = TileRefiner::new(w, h, STILL_MS);
    r2.on_frame(&frame, None, t0);
    let mut all = r2.tick(&frame, matured, total, None);
    assert_eq!(all.len(), 8);
    generous.sort_by_key(|t| (t.y, t.x));
    all.sort_by_key(|t| (t.y, t.x));
    assert_eq!(all, generous);
}

#[test]
fn focus_point_gets_budget_priority() {
    let (w, h) = (512u32, 64u32); // 8 块一行 → 2 个 run：x 0..256 与 256..512
    let frame = pattern_frame(w, h, 11);
    let t0 = Instant::now();
    let matured = t0 + Duration::from_millis(u64::from(STILL_MS));

    // 测量单张瓦片大小，构造"只够一张"的预算。
    let mut probe = TileRefiner::new(w, h, STILL_MS);
    probe.on_frame(&frame, None, t0);
    let all = probe.tick(&frame, matured, usize::MAX, None);
    assert_eq!(all.len(), 2);
    let one = all.iter().map(|t| t.data.len()).max().unwrap_or(0);

    // 焦点 (480, 32) 靠近右侧 run 中心 (384, 32) → 优先精修右侧。
    let mut r = TileRefiner::new(w, h, STILL_MS);
    r.on_frame(&frame, None, t0);
    let tiles = r.tick(&frame, matured, one, Some((480, 32)));
    assert_eq!(tiles.len(), 1);
    assert_eq!((tiles[0].x, tiles[0].w), (256, 256));

    // 剩余的留到下一 tick，按行序补齐。
    let rest = r.tick(&frame, matured + Duration::from_millis(1), usize::MAX, None);
    assert_eq!(rest.len(), 1);
    assert_eq!((rest[0].x, rest[0].w), (0, 256));

    // 无焦点时按行序装填，同样预算先出左侧。
    let mut r2 = TileRefiner::new(w, h, STILL_MS);
    r2.on_frame(&frame, None, t0);
    let tiles = r2.tick(&frame, matured, one, None);
    assert_eq!(tiles.len(), 1);
    assert_eq!((tiles[0].x, tiles[0].w), (0, 256));
}

#[test]
fn resize_rebuilds_grid_all_dirty_without_invalidate() {
    let (w, h) = (256u32, 128u32);
    let mut refiner = TileRefiner::new(w, h, STILL_MS);
    let frame = pattern_frame(w, h, 5);
    let t0 = Instant::now();
    refiner.on_frame(&frame, None, t0);
    assert!(!refiner
        .tick(
            &frame,
            t0 + Duration::from_millis(u64::from(STILL_MS)),
            usize::MAX,
            None
        )
        .is_empty());

    // 尺寸变化：重建网格（4x2 → 8x2），全部置脏且不产失效。
    let frame2 = pattern_frame(512, 128, 6);
    let t1 = t0 + Duration::from_millis(u64::from(STILL_MS));
    let inv = refiner.on_frame(&frame2, None, t1);
    assert!(inv.is_empty(), "尺寸重建不应产出失效标记");
    assert!(
        refiner.tick(&frame2, t1, usize::MAX, None).is_empty(),
        "重建后计时重置"
    );

    let tiles = refiner.tick(
        &frame2,
        t1 + Duration::from_millis(u64::from(STILL_MS)),
        usize::MAX,
        None,
    );
    let covered: usize = tiles
        .iter()
        .map(|t| u32::from(t.w) as usize * u32::from(t.h) as usize / (64 * 64))
        .sum();
    assert_eq!(covered, 16, "新网格 8x2=16 块全部精修");
    assert_tiles_match_source(&frame2, &tiles);
}

#[test]
fn hint_narrows_change_detection() {
    let (w, h) = (256u32, 64u32); // 4 块一行
    let f = pattern_frame(w, h, 13);
    let mut grid = BlockGrid::new(w, h);
    assert_eq!(grid.update(&f, None), 4);

    let mut f2 = f.clone();
    // 只改块 0、1 的像素，但 hint 只给块 3 区域。
    overwrite_region(&mut f2, Rect::new(0, 0, 128, 64), 0xABCD_0001);
    assert_eq!(grid.update(&f2, Some(&[Rect::new(192, 0, 64, 64)])), 0);
    assert_eq!(grid.update(&f2, None), 2);
    assert_eq!(grid.changed_blocks(), &[0, 1]);
}

#[test]
fn tick_before_on_frame_is_safe() {
    let mut refiner = TileRefiner::new(128, 128, STILL_MS);
    let f = pattern_frame(128, 128, 1);
    let now = Instant::now();
    assert!(refiner.tick(&f, now, usize::MAX, None).is_empty());
}
