//! 无损瓦片：脏块静止精修（QOI / zstd-BGRA）与失效清除（T-010）。
//!
//! 文字清晰核心路径（架构 §3.2 / D-004）：运动画面走 H.264，区域静止
//! ≥ `still_ms` 后由 [`TileRefiner`] 产出无损瓦片，控制端覆盖在视频层之上；
//! 该区域再次变化时输出 `fmt = 255` 失效标记清除覆盖层。
//!
//! [`TileOut`] 自包含（x/y/w/h/fmt/data），不带 window_id / frame_seq ——
//! 帧头由传输层（T-004 TileHeader，协议契约 §3：`u64 window_id, u32 frame_seq,
//! u16 x,y,w,h, u8 fmt, u32 len`）承载，本模块只产出瓦片负载。
//!
//! 合并与预算策略：
//! - 行内相邻块合并为矩形，行内最大 256 像素宽（4 块）；
//! - 每 `tick` 字节预算 `budget_bytes`，装不下的瓦片留到下一 tick；
//! - 提供 `focus_point` 时按块中心到焦点距离升序装填（鼠标/输入点优先）。

use std::time::{Duration, Instant};

use anyhow::{bail, Context};

use crate::dirty::{BlockGrid, Rect, BLOCK};
use crate::frame::RawFrame;

/// 瓦片格式：QOI（4 通道，像素字节序即 BGRA）。
pub const FMT_QOI: u8 = 0;
/// 瓦片格式：zstd 压缩的裸 BGRA（紧凑行距）。
pub const FMT_ZSTD_BGRA: u8 = 1;
/// 瓦片格式：失效清除（`data` 为空）。
pub const FMT_INVALIDATE: u8 = 255;

/// 行内合并的块数上限（256 / 64）。
const MAX_MERGE_BLOCKS: u32 = 4;

/// 一张瓦片（或一条失效清除）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileOut {
    /// 瓦片左上角 x（帧像素坐标）。
    pub x: u16,
    /// 瓦片左上角 y。
    pub y: u16,
    /// 宽（像素）。
    pub w: u16,
    /// 高（像素）。
    pub h: u16,
    /// 格式：[`FMT_QOI`] / [`FMT_ZSTD_BGRA`] / [`FMT_INVALIDATE`]。
    pub fmt: u8,
    /// 瓦片负载数据；失效瓦片为空。
    pub data: Vec<u8>,
}

/// 解码后的瓦片像素（紧凑行距 BGRA）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedTile {
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
    /// BGRA 像素数据，长度 = width * height * 4。
    pub bgra: Vec<u8>,
}

/// 行内合并出的瓦片候选矩形（含块范围，用于回写状态）。
struct Run {
    rect: Rect,
    row: u32,
    c0: u32,
    c1: u32,
}

/// 无损瓦片精修器：块状态机（Dirty{since} → Refined → 失效回滚）+ 预算装填。
///
/// 用法：每帧调用 [`on_frame`](Self::on_frame)（喂帧、回收失效标记），
/// 每个 tick 调用 [`tick`](Self::tick)（在预算内产出已静止区域的无损瓦片）。
pub struct TileRefiner {
    grid: BlockGrid,
    /// 每块状态；长度与网格块数一致（on_frame 里对齐）。
    states: Vec<BlockState>,
    still: Duration,
    /// on_frame 上次见到的网格代数（尺寸重建信号）。
    last_generation: u64,
}

#[derive(Clone, Copy)]
enum BlockState {
    /// 内容变化中/待精修，`since` 为最近一次变化时刻。
    Dirty { since: Instant },
    /// 已发送无损瓦片。
    Refined,
}

impl TileRefiner {
    /// 建立精修器；`still_ms` 为块静止判定阈值（通常 300）。
    pub fn new(w: u32, h: u32, still_ms: u32) -> Self {
        Self {
            grid: BlockGrid::new(w, h),
            states: Vec::new(),
            still: Duration::from_millis(u64::from(still_ms)),
            last_generation: 0,
        }
    }

    /// 网格尺寸（像素）。
    pub fn size(&self) -> (u32, u32) {
        (self.grid.cols() * BLOCK, self.grid.rows() * BLOCK)
    }

    /// 喂一帧：比对脏块，回收"已精修又变化"区域的失效标记。
    ///
    /// 返回 `fmt = 255` 的失效瓦片（矩形按行内合并，`data` 为空）。
    /// 网格重建（首帧/尺寸变化）时全部块置 `Dirty{now}`，不产失效——
    /// 尺寸变化走 H.264 IDR 新流，控制端旧瓦片随全帧重绘自然清除。
    pub fn on_frame(&mut self, f: &RawFrame, hint: Option<&[Rect]>, now: Instant) -> Vec<TileOut> {
        self.grid.update(f, hint);
        let total = (self.grid.cols() * self.grid.rows()) as usize;
        let generation = self.grid.generation();

        if generation != self.last_generation || self.states.len() != total {
            // 网格重建或尚未初始化：全部块置脏。
            self.states = vec![BlockState::Dirty { since: now }; total];
            self.last_generation = generation;
            return Vec::new();
        }

        let mut invalid: Vec<u32> = Vec::new();
        for &idx in self.grid.changed_blocks() {
            match &mut self.states[idx as usize] {
                st @ BlockState::Refined => {
                    invalid.push(idx);
                    *st = BlockState::Dirty { since: now };
                }
                BlockState::Dirty { since } => *since = now,
            }
        }

        let runs = runs_from_blocks(&invalid, self.grid.cols(), f.width, f.height);
        runs.into_iter()
            .map(|r| TileOut {
                x: r.rect.x as u16,
                y: r.rect.y as u16,
                w: r.rect.w as u16,
                h: r.rect.h as u16,
                fmt: FMT_INVALIDATE,
                data: Vec::new(),
            })
            .collect()
    }

    /// 精修一 tick：把静止满 `still` 的脏块在预算内编码下发。
    ///
    /// - 候选块按行内合并成瓦片矩形（≤256 像素宽）；
    /// - 有 `focus`（最近输入点）时按瓦片中心到焦点距离升序装填；
    /// - 每张瓦片 QOI / zstd(level 1) 双编码取更小者；超出预算的留到下一 tick。
    pub fn tick(
        &mut self,
        f: &RawFrame,
        now: Instant,
        budget_bytes: usize,
        focus: Option<(u32, u32)>,
    ) -> Vec<TileOut> {
        let total = (self.grid.cols() * self.grid.rows()) as usize;
        if self.states.len() != total {
            // 未喂帧或网格刚重建：本轮只对齐状态，不产出。
            self.states = vec![BlockState::Dirty { since: now }; total];
            return Vec::new();
        }

        let mut matured: Vec<u32> = Vec::new();
        for (idx, st) in self.states.iter().enumerate() {
            if let BlockState::Dirty { since } = st {
                if now.duration_since(*since) >= self.still {
                    matured.push(idx as u32);
                }
            }
        }

        let mut runs = runs_from_blocks(&matured, self.grid.cols(), f.width, f.height);
        if let Some((fx, fy)) = focus {
            let (fx, fy) = (i64::from(fx), i64::from(fy));
            runs.sort_by_key(|r| {
                let cx = i64::from(r.rect.x + r.rect.w / 2);
                let cy = i64::from(r.rect.y + r.rect.h / 2);
                let (dx, dy) = (cx - fx, cy - fy);
                dx * dx + dy * dy
            });
        }

        let mut out = Vec::new();
        let mut remaining = budget_bytes;
        for r in &runs {
            let tile = match encode_tile(f, &r.rect) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        rect = ?r.rect,
                        "瓦片编码失败，留到下一 tick: {e:#}"
                    );
                    continue;
                }
            };
            if tile.data.len() <= remaining {
                remaining -= tile.data.len();
                let cols = self.grid.cols();
                for bx in r.c0..=r.c1 {
                    self.states[(r.row * cols + bx) as usize] = BlockState::Refined;
                }
                out.push(tile);
            }
        }
        out
    }
}

/// 变更块索引 → 行内合并矩形（升序去重，行内最多 4 块）。
fn runs_from_blocks(blocks: &[u32], cols: u32, frame_w: u32, frame_h: u32) -> Vec<Run> {
    let mut sorted = blocks.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut runs = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let idx = sorted[i];
        let row = idx / cols;
        let start = idx % cols;
        let mut end = start;
        let mut j = i + 1;
        while j < sorted.len()
            && sorted[j] / cols == row
            && sorted[j] % cols == end + 1
            && end - start + 1 < MAX_MERGE_BLOCKS
        {
            end += 1;
            j += 1;
        }
        let x = start * BLOCK;
        let y = row * BLOCK;
        runs.push(Run {
            rect: Rect::new(
                x,
                y,
                ((end - start + 1) * BLOCK).min(frame_w.saturating_sub(x)),
                BLOCK.min(frame_h.saturating_sub(y)),
            ),
            row,
            c0: start,
            c1: end,
        });
        i = j;
    }
    runs
}

/// 按瓦片矩形从帧中抽取紧凑行距 BGRA（兼容任意源 stride）。
pub fn extract_bgra(f: &RawFrame, rect: &Rect) -> Vec<u8> {
    let stride = f.stride as usize;
    let row_len = rect.w as usize * 4;
    let mut out = Vec::with_capacity(rect.w as usize * rect.h as usize * 4);
    for row in 0..rect.h as usize {
        let start = (rect.y as usize + row) * stride + rect.x as usize * 4;
        out.extend_from_slice(&f.data[start..start + row_len]);
    }
    out
}

/// 编码一张瓦片：QOI 单格式（D-013 包4/E2：与 zstd-BGRA 双格式相比，压缩率
/// 收益仅个位数百分比，却让解码/测试矩阵翻倍——fmt=1 不再产出；解码端
/// `decode_tile` 保留 fmt=1 分支以兼容旧流量）。
fn encode_tile(f: &RawFrame, rect: &Rect) -> anyhow::Result<TileOut> {
    let bgra = extract_bgra(f, rect);
    let qoi_bytes = qoi::encode_to_vec(&bgra, rect.w, rect.h)
        .with_context(|| format!("QOI 编码失败 ({}x{})", rect.w, rect.h))?;
    Ok(TileOut {
        x: rect.x as u16,
        y: rect.y as u16,
        w: rect.w as u16,
        h: rect.h as u16,
        fmt: FMT_QOI,
        data: qoi_bytes,
    })
}

/// 解码瓦片负载回紧凑 BGRA（控制端渲染语义的对偶，测试与调试用）。
///
/// # Errors
/// 未知 `fmt`、QOI/zstd 解码失败、解码结果尺寸与瓦片头不符。
pub fn decode_tile(tile: &TileOut) -> anyhow::Result<DecodedTile> {
    let expect_len = usize::from(tile.w) * usize::from(tile.h) * 4;
    match tile.fmt {
        FMT_QOI => {
            let (hdr, px) = qoi::decode_to_vec(&tile.data)
                .with_context(|| format!("QOI 解码失败 ({}x{})", tile.w, tile.h))?;
            if hdr.width != u32::from(tile.w) || hdr.height != u32::from(tile.h) {
                bail!(
                    "QOI 头尺寸 {}x{} 与瓦片 {}x{} 不符",
                    hdr.width,
                    hdr.height,
                    tile.w,
                    tile.h
                );
            }
            if px.len() != expect_len {
                bail!("QOI 解码字节数 {} != 期望 {}", px.len(), expect_len);
            }
            Ok(DecodedTile {
                width: hdr.width,
                height: hdr.height,
                bgra: px,
            })
        }
        FMT_ZSTD_BGRA => {
            let px = zstd::bulk::decompress(&tile.data, expect_len)
                .with_context(|| format!("zstd 解压失败 ({}x{})", tile.w, tile.h))?;
            if px.len() != expect_len {
                bail!("zstd 解压字节数 {} != 期望 {}", px.len(), expect_len);
            }
            Ok(DecodedTile {
                width: u32::from(tile.w),
                height: u32::from(tile.h),
                bgra: px,
            })
        }
        FMT_INVALIDATE => bail!("失效瓦片无像素数据"),
        other => bail!("未知瓦片格式 {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_of(t: &TileOut) -> Rect {
        Rect::new(
            u32::from(t.x),
            u32::from(t.y),
            u32::from(t.w),
            u32::from(t.h),
        )
    }

    #[test]
    fn runs_merge_up_to_four_blocks_per_row() {
        // 8 块一行 → 两个 256 宽 run。
        let runs = runs_from_blocks(&[0, 1, 2, 3, 4, 5, 6, 7], 8, 512, 64);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].rect, Rect::new(0, 0, 256, 64));
        assert_eq!(runs[1].rect, Rect::new(256, 0, 256, 64));

        // 不相邻块各成 run；跨行不合并。
        // cols=4：块 0/2 在行 0（x 0/128），块 5/7 在行 1（x 64/192）。
        let runs = runs_from_blocks(&[0, 2, 5, 7], 4, 256, 256);
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].rect, Rect::new(0, 0, 64, 64));
        assert_eq!(runs[1].rect, Rect::new(128, 0, 64, 64));
        assert_eq!(runs[2].rect, Rect::new(64, 64, 64, 64));
        assert_eq!(runs[3].rect, Rect::new(192, 64, 64, 64));

        // 边缘块宽收窄到帧宽。
        let runs = runs_from_blocks(&[3], 4, 200, 64);
        assert_eq!(runs[0].rect, Rect::new(192, 0, 8, 64));
    }

    #[test]
    fn extract_bgra_handles_stride() {
        // stride 比 row 多 4 字节填充，抽取结果应为紧凑行距。
        let mut f = RawFrame::new(8, 2, 0);
        f.stride = 8 * 4 + 4;
        f.data = vec![7; f.stride as usize * 2];
        for (y, x) in [(0usize, 2usize), (1, 5)] {
            let off = y * f.stride as usize + x * 4;
            f.data[off..off + 4].copy_from_slice(&[1, 2, 3, 4]);
        }
        let got = extract_bgra(&f, &Rect::new(2, 0, 4, 2));
        assert_eq!(got.len(), 4 * 4 * 2);
        assert_eq!(&got[0..4], &[1, 2, 3, 4]);
        // 第二行从 (2,1) 起。
        assert_eq!(&got[16..20], &[7, 7, 7, 7]);
        let off = f.stride as usize + 5 * 4;
        assert_eq!(&f.data[off..off + 4], &[1, 2, 3, 4]);
    }

    #[test]
    fn decode_tile_roundtrips_both_fmts() {
        let mut f = RawFrame::new(64, 64, 0);
        for (i, px) in f.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px.copy_from_slice(&[(i % 251) as u8, (i / 7 % 200) as u8, (i / 13) as u8, 255]);
        }
        let rect = Rect::new(0, 0, 64, 64);
        let tile = encode_tile(&f, &rect).expect("编码成功");
        assert!(matches!(tile.fmt, FMT_QOI | FMT_ZSTD_BGRA));
        let dec = decode_tile(&tile).expect("解码成功");
        assert_eq!(dec.bgra, extract_bgra(&f, &rect));
        assert_eq!(rect_of(&tile), rect);

        let bad = TileOut { fmt: 42, ..tile };
        assert!(decode_tile(&bad).is_err());
        let inv = TileOut {
            fmt: FMT_INVALIDATE,
            ..bad
        };
        assert!(decode_tile(&inv).is_err());
    }
}
