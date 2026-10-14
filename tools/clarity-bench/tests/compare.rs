//! compare 子命令集成测试（任务卡 T-024）：
//! 1. 同图 → 一致率 100%；
//! 2. 人为 3×3 均值模糊 → 一致率显著下降；
//! 3. 平移 3 px 后按定位标记对齐成功。

#[path = "../src/compare.rs"]
mod compare;

use compare::{
    compare_images, find_marker_bbox, load_png_rgb, save_png_rgb, CompareOptions, CompareResult,
    Image, MARKER,
};

/// 画布尺寸。
const CW: u32 = 240;
const CH: u32 = 190;
/// 内容块（含四角定位标记）默认位置与尺寸。
const BX: u32 = 20;
const BY: u32 = 20;
const BW: u32 = 200;
const BH: u32 = 150;

/// 构造一张"测试窗口截图"：灰底 + 内容块（四角 16×16 品红标记、
/// 8px 黑白棋盘、RGB 渐变、1px 竖条纹），整体可平移 (sx, sy)。
fn test_image(sx: u32, sy: u32) -> Image {
    let mut img = Image::new(CW, CH);
    let (bx, by) = (BX + sx, BY + sy);
    for y in 0..CH {
        for x in 0..CW {
            img.set_pixel(x, y, [200, 200, 200]);
        }
    }
    for dy in 0..16u32 {
        for dx in 0..16u32 {
            for (ox, oy) in [(0u32, 0u32), (BW - 16, 0), (0, BH - 16), (BW - 16, BH - 16)] {
                img.set_pixel(bx + ox + dx, by + oy + dy, MARKER);
            }
        }
    }
    for y in 16..96u32 {
        for x in 16..96u32 {
            let c = if (x / 8 + y / 8) % 2 == 0 { 0 } else { 255 };
            img.set_pixel(bx + x, by + y, [c, c, c]);
        }
    }
    for y in 16..(BH - 16) {
        for x in 100..(BW - 16) {
            let r = (x * 255 / BW) as u8;
            let g = (y * 255 / BH) as u8;
            let b = ((x + y) * 255 / (BW + BH)) as u8;
            img.set_pixel(bx + x, by + y, [r, g, b]);
        }
    }
    for y in 100..(BH - 16) {
        for x in 16..96u32 {
            let c = if x % 2 == 0 { 0 } else { 255 };
            img.set_pixel(bx + x, by + y, [c, c, c]);
        }
    }
    img
}

/// 3×3 均值模糊（边缘取 clamp 邻域）。
fn blur3x3(src: &Image) -> Image {
    let mut out = Image::new(src.width, src.height);
    for y in 0..src.height {
        for x in 0..src.width {
            let mut acc = [0u32; 3];
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let sx = (x as i32 + dx).clamp(0, src.width as i32 - 1) as u32;
                    let sy = (y as i32 + dy).clamp(0, src.height as i32 - 1) as u32;
                    let p = src.pixel(sx, sy);
                    for c in 0..3 {
                        acc[c] += u32::from(p[c]);
                    }
                }
            }
            out.set_pixel(
                x,
                y,
                [(acc[0] / 9) as u8, (acc[1] / 9) as u8, (acc[2] / 9) as u8],
            );
        }
    }
    out
}

fn run(source: &Image, received: &Image) -> CompareResult {
    compare_images(source, received, &CompareOptions::default()).expect("compare_images 不应失败")
}

#[test]
fn identical_image_is_100_percent() {
    let a = test_image(0, 0);
    let r = run(&a, &a);
    assert!(r.aligned, "应按定位标记对齐");
    assert!(
        (r.exact_ratio - 1.0).abs() < f64::EPSILON,
        "exact_ratio 应为 1.0，实际 {}",
        r.exact_ratio
    );
    assert!(
        (r.text_exact_ratio - 1.0).abs() < f64::EPSILON,
        "text_exact_ratio 应为 1.0，实际 {}",
        r.text_exact_ratio
    );
    assert_eq!(r.psnr, None, "逐像素一致时 psnr 应为 null");
    assert_eq!(r.max_err, 0);
}

#[test]
fn blur_3x3_sharply_reduces_ratio() {
    let src = test_image(0, 0);
    let blurred = blur3x3(&src);
    let r = run(&src, &blurred);
    // 模糊会侵蚀品红标记边缘，两侧标记外接矩形不再精确等大，
    // 此时按设计退化为整图同坐标比对（aligned=false），指标仍然有效。
    assert!(!r.aligned, "标记被侵蚀后应退化为整图比对");
    assert!(
        r.exact_ratio < 0.90,
        "3×3 均值模糊后 exact_ratio 应显著低于 1.0，实际 {:.4}",
        r.exact_ratio
    );
    assert!(r.exact_ratio > 0.0, "纯背景区域仍应保留部分一致像素");
    assert!(r.max_err > 0, "模糊应产生通道误差");
    assert!(r.psnr.is_some(), "非完全一致时 psnr 应有限");
    assert!(
        r.psnr.unwrap() < 45.0,
        "psnr 应明显低于完美值，实际 {:?}",
        r.psnr
    );
}

#[test]
fn shift_by_3px_aligns_successfully() {
    let src = test_image(0, 0);
    let shifted = test_image(3, 2); // 平移 ≥3px
    let sb = find_marker_bbox(&src).expect("source 应含定位标记");
    let rb = find_marker_bbox(&shifted).expect("received 应含定位标记");
    assert_eq!((rb.x - sb.x, rb.y - sb.y), (3, 2), "平移量应被标记检测捕获");
    let r = run(&src, &shifted);
    assert!(r.aligned, "平移后应按定位标记对齐");
    assert!(
        (r.exact_ratio - 1.0).abs() < f64::EPSILON,
        "对齐后 exact_ratio 应为 1.0，实际 {}",
        r.exact_ratio
    );
    assert!((r.text_exact_ratio - 1.0).abs() < f64::EPSILON);
    assert_eq!(r.psnr, None);
    assert_eq!(r.max_err, 0);
}

#[test]
fn png_roundtrip_preserves_pixels() {
    let img = test_image(0, 0);
    let path = std::env::temp_dir().join("clarity-bench-test-roundtrip.png");
    save_png_rgb(&path, &img).expect("写 PNG 不应失败");
    let loaded = load_png_rgb(&path).expect("读 PNG 不应失败");
    let _ = std::fs::remove_file(&path);
    assert_eq!(img, loaded, "PNG 往返应逐像素一致");
}
