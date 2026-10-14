//! 像素比对：文字区逐像素一致性度量（09_test/测试策略.md，T-024）。
//!
//! 测试窗口四角绘制 16×16 的 `#FF00FF` 定位标记。compare 先按两侧标记的
//! 外接矩形对齐（天然支持整图平移），再在标记区域内计算：
//! 全图逐像素一致率（exact_ratio）、文字区一致率（text_exact_ratio）、
//! PSNR 与最大通道误差（max_err）。
//!
//! 本模块自包含：`tests/compare.rs` 通过 `#[path]` 直接引入本文件编译，
//! 因此不得依赖 `main.rs` / `testwin.rs` 中的任何符号。

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

/// 定位标记颜色 `#FF00FF`（品红）。
pub const MARKER: [u8; 3] = [0xFF, 0x00, 0xFF];

/// 8-bit RGB 图像（行优先、无行填充）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGB 三通道，`len() == width * height * 3`。
    pub data: Vec<u8>,
}

// Image 构造器等部分方法仅测试使用。
#[allow(dead_code)]
impl Image {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0u8; width as usize * height as usize * 3],
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    /// 供测试构造图像使用。
    #[allow(dead_code)]
    pub fn set_pixel(&mut self, x: u32, y: u32, rgb: [u8; 3]) {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        self.data[i..i + 3].copy_from_slice(&rgb);
    }

    fn contains(&self, r: Region) -> bool {
        r.x.checked_add(r.w).is_some_and(|end| end <= self.width)
            && r.y.checked_add(r.h).is_some_and(|end| end <= self.height)
    }
}

/// 矩形区域（像素坐标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            w: width,
            h: height,
        }
    }
}

/// 比对选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct CompareOptions {
    /// 显式文字区（source 图坐标）。缺省 = 对齐区域向内收缩得到。
    pub text_region: Option<Region>,
}

/// 比对结果。`--json` 直接序列化本结构。
#[derive(Debug, Serialize)]
pub struct CompareResult {
    /// 比对区域内逐像素完全一致的比例（0.0 ~ 1.0）。
    pub exact_ratio: f64,
    /// 文字区内逐像素完全一致的比例（0.0 ~ 1.0）。
    pub text_exact_ratio: f64,
    /// PSNR（dB）。逐像素一致时 PSNR 为无穷大，JSON 输出 null。
    pub psnr: Option<f64>,
    /// 最大单通道误差（0 ~ 255）。
    pub max_err: u32,
    /// true = 按两侧定位标记完成对齐；false = 退化为整图同坐标比对。
    pub aligned: bool,
    /// 实际比对区域（source 图坐标）。
    pub region: Region,
    /// 文字区（source 图坐标）。
    pub text_region: Region,
}

/// 读取 PNG 并归一为 8-bit RGB。
pub fn load_png_rgb(path: impl AsRef<Path>) -> Result<Image> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("打开 PNG 失败: {}", path.display()))?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    // 统一为 8-bit：灰度<8bit 提升、16bit 降为 8bit、palette 展开。
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .with_context(|| format!("读取 PNG 头失败: {}", path.display()))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .with_context(|| format!("解码 PNG 失败: {}", path.display()))?;
    buf.truncate(info.buffer_size());
    decoded_to_rgb(info.width, info.height, info.color_type, buf)
}

fn decoded_to_rgb(
    width: u32,
    height: u32,
    color_type: png::ColorType,
    data: Vec<u8>,
) -> Result<Image> {
    let px = width as usize * height as usize;
    let mut img = Image::new(width, height);
    match color_type {
        png::ColorType::Rgb => {
            if data.len() != px * 3 {
                bail!("PNG 数据长度不符（RGB，{} != {}）", data.len(), px * 3);
            }
            img.data = data;
        }
        png::ColorType::Rgba => {
            if data.len() != px * 4 {
                bail!("PNG 数据长度不符（RGBA，{} != {}）", data.len(), px * 4);
            }
            for i in 0..px {
                let s = i * 4;
                img.data[i * 3..i * 3 + 3].copy_from_slice(&data[s..s + 3]);
            }
        }
        png::ColorType::Grayscale => {
            if data.len() != px {
                bail!("PNG 数据长度不符（灰度，{} != {}）", data.len(), px);
            }
            for (i, g) in data.iter().enumerate() {
                let v = i * 3;
                img.data[v..v + 3].fill(*g);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            if data.len() != px * 2 {
                bail!(
                    "PNG 数据长度不符（灰度+alpha，{} != {}）",
                    data.len(),
                    px * 2
                );
            }
            for i in 0..px {
                let v = i * 3;
                img.data[v..v + 3].fill(data[i * 2]);
            }
        }
        other => bail!("不支持的 PNG 颜色类型（归一化后仍为 {other:?}）"),
    }
    Ok(img)
}

/// 写出 8-bit RGB PNG（供测试与后续 capture 接入复用）。
#[allow(dead_code)]
pub fn save_png_rgb(path: impl AsRef<Path>, img: &Image) -> Result<()> {
    let path = path.as_ref();
    let file = File::create(path).with_context(|| format!("创建 PNG 失败: {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), img.width, img.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .with_context(|| format!("写 PNG 头失败: {}", path.display()))?;
    writer
        .write_image_data(&img.data)
        .with_context(|| format!("写 PNG 像素失败: {}", path.display()))?;
    writer
        .finish()
        .with_context(|| format!("收尾 PNG 失败: {}", path.display()))?;
    Ok(())
}

/// 找出所有 `#FF00FF` 像素的外接矩形（测试窗口四角定位标记）。
pub fn find_marker_bbox(img: &Image) -> Option<Region> {
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for y in 0..img.height {
        for x in 0..img.width {
            if img.pixel(x, y) == MARKER {
                found = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if !found {
        return None;
    }
    Some(Region {
        x: min_x,
        y: min_y,
        w: max_x - min_x + 1,
        h: max_y - min_y + 1,
    })
}

/// 比对两张图：先按定位标记对齐，再计算各项指标。
pub fn compare_images(
    source: &Image,
    received: &Image,
    opts: &CompareOptions,
) -> Result<CompareResult> {
    let (src_region, rcv_region, aligned) = align_regions(source, received)?;
    if !source.contains(src_region) {
        bail!("比对区域越界（source {source:?} vs {src_region:?}）");
    }
    if !received.contains(rcv_region) {
        bail!("比对区域越界（received {received:?} vs {rcv_region:?}）");
    }

    let (src_text, rcv_text) =
        resolve_text_regions(opts.text_region, src_region, rcv_region, source, received)?;

    let m = region_metrics(source, src_region, received, rcv_region);
    let mt = region_metrics(source, src_text, received, rcv_text);

    Ok(CompareResult {
        exact_ratio: m.exact as f64 / m.total as f64,
        text_exact_ratio: mt.exact as f64 / mt.total as f64,
        psnr: psnr_from_sum_sq(m.sum_sq, m.total),
        max_err: m.max_err,
        aligned,
        region: src_region,
        text_region: src_text,
    })
}

/// 对齐：两侧定位标记外接矩形尺寸一致即采用（支持整图平移）；
/// 否则要求两图尺寸一致并退化为同坐标比对。
fn align_regions(source: &Image, received: &Image) -> Result<(Region, Region, bool)> {
    let same_size = source.width == received.width && source.height == received.height;
    let (src_bbox, rcv_bbox) = (find_marker_bbox(source), find_marker_bbox(received));
    if let (Some(a), Some(b)) = (src_bbox, rcv_bbox) {
        if a.w == b.w && a.h == b.h {
            return Ok((a, b, true));
        }
    }
    if same_size {
        return Ok((
            Region::full(source.width, source.height),
            Region::full(received.width, received.height),
            false,
        ));
    }
    bail!(
        "两图尺寸不一致（{}x{} vs {}x{}）且定位标记无法对齐",
        source.width,
        source.height,
        received.width,
        received.height
    );
}

/// 解析文字区：显式给定（source 坐标）时映射到 received；
/// 缺省时对两侧比对区域向内收缩（5%，最多 24px）。
fn resolve_text_regions(
    explicit: Option<Region>,
    src_region: Region,
    rcv_region: Region,
    source: &Image,
    received: &Image,
) -> Result<(Region, Region)> {
    if let Some(r) = explicit {
        if !source.contains(r) {
            bail!("文字区越界（source {source:?} vs {r:?}）");
        }
        let dx = rcv_region.x as i64 - src_region.x as i64;
        let dy = rcv_region.y as i64 - src_region.y as i64;
        let shifted = Region {
            x: (r.x as i64 + dx).max(0) as u32,
            y: (r.y as i64 + dy).max(0) as u32,
            w: r.w,
            h: r.h,
        };
        if !received.contains(shifted) {
            bail!("文字区越界（received {received:?} vs {shifted:?}）");
        }
        return Ok((r, shifted));
    }
    Ok((shrink(src_region), shrink(rcv_region)))
}

/// 区域向内收缩：各方向收缩 5%（最多 24px），避开角部定位标记与棋盘。
fn shrink(r: Region) -> Region {
    let sx = ((r.w as u64 * 5 / 100).min(24)) as u32;
    let sy = ((r.h as u64 * 5 / 100).min(24)) as u32;
    Region {
        x: r.x + sx,
        y: r.y + sy,
        w: r.w.saturating_sub(sx * 2),
        h: r.h.saturating_sub(sy * 2),
    }
}

struct Metrics {
    exact: u64,
    total: u64,
    sum_sq: u128,
    max_err: u32,
}

fn region_metrics(source: &Image, src: Region, received: &Image, rcv: Region) -> Metrics {
    let mut m = Metrics {
        exact: 0,
        total: 0,
        sum_sq: 0,
        max_err: 0,
    };
    for dy in 0..src.h {
        for dx in 0..src.w {
            let a = source.pixel(src.x + dx, src.y + dy);
            let b = received.pixel(rcv.x + dx, rcv.y + dy);
            m.total += 1;
            let mut same = true;
            for c in 0..3 {
                let d = i32::from(a[c]) - i32::from(b[c]);
                if d != 0 {
                    same = false;
                    let dd = u128::from(d.unsigned_abs());
                    m.sum_sq += dd * dd;
                    m.max_err = m.max_err.max(d.unsigned_abs());
                }
            }
            if same {
                m.exact += 1;
            }
        }
    }
    m
}

fn psnr_from_sum_sq(sum_sq: u128, total: u64) -> Option<f64> {
    if total == 0 {
        return None;
    }
    if sum_sq == 0 {
        return None; // 逐像素一致，PSNR = ∞，JSON 输出 null
    }
    let mse = sum_sq as f64 / (total as f64 * 3.0);
    Some(10.0 * (255.0 * 255.0 / mse).log10())
}

/// 人读文本输出。
#[allow(dead_code)]
pub fn print_text(r: &CompareResult) {
    let psnr_text = match r.psnr {
        Some(v) => format!("{v:.2} dB"),
        None => "inf（逐像素一致）".to_string(),
    };
    println!(
        "aligned       = {}\nregion        = x={}, y={}, {}x{}\ntext_region   = x={}, y={}, {}x{}\n\
         exact_ratio      = {:.6}\ntext_exact_ratio = {:.6}\npsnr             = {psnr_text}\nmax_err          = {}",
        r.aligned,
        r.region.x, r.region.y, r.region.w, r.region.h,
        r.text_region.x, r.text_region.y, r.text_region.w, r.text_region.h,
        r.exact_ratio, r.text_exact_ratio, r.max_err,
    );
}

/// compare 子命令入口：`compare <source.png> <received.png> [--json] [--text-region x,y,w,h]`。
#[allow(dead_code)]
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut json = false;
    let mut text_region = None;
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--text-region" => {
                i += 1;
                let raw = args.get(i).context("--text-region 需要 x,y,w,h")?;
                text_region = Some(parse_region(raw)?);
            }
            s if s.starts_with('-') => bail!("compare 未知参数: {s}"),
            s => positional.push(s),
        }
        i += 1;
    }
    if positional.len() != 2 {
        bail!("用法: clarity-bench compare <source.png> <received.png> [--json] [--text-region x,y,w,h]");
    }
    let source = load_png_rgb(positional[0])?;
    let received = load_png_rgb(positional[1])?;
    let result = compare_images(&source, &received, &CompareOptions { text_region })?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        print_text(&result);
    }
    Ok(())
}

fn parse_region(raw: &str) -> Result<Region> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        bail!("--text-region 格式应为 x,y,w,h，收到: {raw}");
    }
    let nums: Vec<u32> = parts
        .iter()
        .map(|p| {
            p.parse::<u32>()
                .map_err(|_| anyhow::anyhow!("--text-region 含非整数: {raw}"))
        })
        .collect::<Result<_>>()?;
    Ok(Region::new(nums[0], nums[1], nums[2], nums[3]))
}
