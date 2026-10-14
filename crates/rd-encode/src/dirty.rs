//! 脏区跟踪：64×64 块网格内容比对与变化块识别（T-010）。
//!
//! 每块维护一份 xxh3 内容哈希；[`BlockGrid::update`] 逐帧比对返回变更块数，
//! 并记录本轮变更块列表（[`BlockGrid::changed_blocks`]）供上层状态机使用。
//!
//! - 无 `hint`：全量比对所有块（WGC 无 dirty rect 时的兜底路径）；
//! - 有 `hint`：只比对 hint 矩形覆盖的块（调用方承诺变更都在 hint 内）；
//! - 帧（或首次 update）尺寸与网格不一致时重建网格，全部块视为变更。
//!
//! 块静止判定与瓦片精修状态机在 [`crate::tiles::TileRefiner`]，本模块只负责
//! "哪些块变了" 这一纯比对职责，不依赖 Windows API。

use xxhash_rust::xxh3::xxh3_64;

use crate::frame::RawFrame;

/// 块边长（像素）。网格行列数 = ceil(w / BLOCK) × ceil(h / BLOCK)。
pub const BLOCK: u32 = 64;

/// 矩形（像素坐标，窗口客户区系）。
///
/// 协议侧瓦片坐标与 WGC dirty rect 都以此表达；字段为 `u32`，
/// 序列化时由各层自行收窄（瓦片帧头用 `u16`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// 左上角 x。
    pub x: u32,
    /// 左上角 y。
    pub y: u32,
    /// 宽。
    pub w: u32,
    /// 高。
    pub h: u32,
}

impl Rect {
    /// 构造矩形。
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// 与另一矩形的交集（不相交返回 `None`）。
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self
            .x
            .saturating_add(self.w)
            .min(other.x.saturating_add(other.w));
        let y1 = self
            .y
            .saturating_add(self.h)
            .min(other.y.saturating_add(other.h));
        if x1 > x0 && y1 > y0 {
            Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
        } else {
            None
        }
    }
}

/// 64×64 块网格的内容哈希表。
///
/// `update` 返回本轮变更块数；变更块列表（升序、去重）通过
/// [`changed_blocks`](Self::changed_blocks) 读取。每次全量重建会使
/// [`generation`](Self::generation) 自增（尺寸变化信号）。
pub struct BlockGrid {
    cols: u32,
    rows: u32,
    /// 每块 xxh3 哈希，长度 = cols * rows。
    hashes: Vec<u64>,
    /// 最近一次 update 的变更块索引（升序去重）。
    changed: Vec<u32>,
    /// 网格重建代数：尺寸变化或首次观测时 +1。
    generation: u64,
    /// 是否已观测过至少一帧（首帧按重建处理，保证全部块上报变更）。
    seen: bool,
    /// 块哈希前的行拷贝缓冲（跨 update 复用，避免每块分配）。
    scratch: Vec<u8>,
}

impl BlockGrid {
    /// 建立指定尺寸的网格（哈希在首次 `update` 时填充）。
    pub fn new(w: u32, h: u32) -> Self {
        let (cols, rows) = grid_dims(w, h);
        Self {
            cols,
            rows,
            hashes: vec![0; (cols * rows) as usize],
            changed: Vec::new(),
            generation: 0,
            seen: false,
            scratch: Vec::with_capacity((BLOCK * BLOCK * 4) as usize),
        }
    }

    /// 网格列数（水平块数）。
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// 网格行数（垂直块数）。
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// 网格重建代数：尺寸变化（或首次观测）后自增。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 最近一次 `update` 的变更块索引（升序去重）。
    pub fn changed_blocks(&self) -> &[u32] {
        &self.changed
    }

    /// 按帧内容比对网格。
    ///
    /// 返回本轮变更块数；尺寸变化时重建网格，全部块计为变更。
    /// 有 `hint` 时只比对 hint 矩形覆盖的块（网格代数不变）。
    pub fn update(&mut self, f: &RawFrame, hint: Option<&[Rect]>) -> usize {
        let (cols, rows) = grid_dims(f.width, f.height);
        if !self.seen || cols != self.cols || rows != self.rows {
            self.rebuild(f, cols, rows);
            return self.changed.len();
        }

        self.changed.clear();
        for idx in self.candidates(hint) {
            let (bx, by) = block_pos(idx, cols);
            let rect = block_rect(bx, by, f.width, f.height);
            let hash = self.hash_block(f, &rect);
            if self.hashes[idx as usize] != hash {
                self.hashes[idx as usize] = hash;
                self.changed.push(idx);
            }
        }
        self.changed.len()
    }

    /// 全量重建：哈希逐块重填，全部块上报变更，代数 +1。
    fn rebuild(&mut self, f: &RawFrame, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.hashes = Vec::with_capacity((cols * rows) as usize);
        self.changed.clear();
        for idx in 0..cols * rows {
            let (bx, by) = block_pos(idx, cols);
            let rect = block_rect(bx, by, f.width, f.height);
            let hash = self.hash_block(f, &rect);
            self.hashes.push(hash);
            self.changed.push(idx);
        }
        self.generation += 1;
        self.seen = true;
    }

    /// 本轮待比对块索引（升序去重）。
    fn candidates(&self, hint: Option<&[Rect]>) -> Vec<u32> {
        let total = self.cols * self.rows;
        let Some(rects) = hint else {
            return (0..total).collect();
        };
        let mut idxs = Vec::new();
        for r in rects {
            // 限制在帧与网格范围内，空矩形与越界矩形自然跳过。
            let frame = Rect::new(0, 0, self.cols * BLOCK, self.rows * BLOCK);
            let Some(hit) = r.intersect(&frame) else {
                continue;
            };
            let c0 = hit.x / BLOCK;
            let c1 = (hit.x + hit.w - 1) / BLOCK;
            let r0 = hit.y / BLOCK;
            let r1 = (hit.y + hit.h - 1) / BLOCK;
            for by in r0..=r1 {
                for bx in c0..=c1 {
                    idxs.push(by * self.cols + bx);
                }
            }
        }
        idxs.sort_unstable();
        idxs.dedup();
        idxs
    }
}

/// `(cols, rows)`：宽高对块边长向上取整。
fn grid_dims(w: u32, h: u32) -> (u32, u32) {
    (w.div_ceil(BLOCK), h.div_ceil(BLOCK))
}

/// 块索引 → `(bx, by)`。
fn block_pos(idx: u32, cols: u32) -> (u32, u32) {
    (idx % cols, idx / cols)
}

/// 块像素矩形（右/下边缘块收窄到帧边界）。
fn block_rect(bx: u32, by: u32, frame_w: u32, frame_h: u32) -> Rect {
    let x = bx * BLOCK;
    let y = by * BLOCK;
    Rect::new(
        x,
        y,
        BLOCK.min(frame_w.saturating_sub(x)),
        BLOCK.min(frame_h.saturating_sub(y)),
    )
}

impl BlockGrid {
    /// 单块内容的 xxh3：行先拷入复用缓冲拼成连续块，再一次大输入哈希。
    ///
    /// 流式 `Xxh3::update` 逐行喂入实测 1080p 全量 4ms（小段调用的分摊
    /// 开销吃掉 SIMD 吞吐）；拷贝（memcpy）+ 单次 16KB 哈希快 2 倍以上。
    fn hash_block(&mut self, f: &RawFrame, r: &Rect) -> u64 {
        let stride = f.stride as usize;
        let row_len = r.w as usize * 4;
        self.scratch.clear();
        for row in 0..r.h as usize {
            let start = (r.y as usize + row) * stride + r.x as usize * 4;
            self.scratch
                .extend_from_slice(&f.data[start..start + row_len]);
        }
        xxh3_64(&self.scratch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn rect_intersect_clamps() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 60, 100, 100);
        assert_eq!(a.intersect(&b), Some(Rect::new(50, 60, 50, 40)));
        let c = Rect::new(200, 200, 10, 10);
        assert_eq!(a.intersect(&c), None);
        assert_eq!(a.intersect(&a), Some(a));
    }

    #[test]
    fn grid_dims_round_up() {
        assert_eq!(grid_dims(0, 0), (0, 0));
        assert_eq!(grid_dims(64, 64), (1, 1));
        assert_eq!(grid_dims(65, 1), (2, 1));
        assert_eq!(grid_dims(1920, 1080), (30, 17));
    }

    #[test]
    fn first_update_reports_all_blocks_then_stabilizes() {
        let f = pattern_frame(200, 100, 7);
        let mut g = BlockGrid::new(200, 100);
        assert_eq!(g.update(&f, None), 8); // 4x2 块
        assert_eq!(g.generation(), 1);
        assert_eq!(g.update(&f, None), 0);

        let mut g2 = BlockGrid::new(200, 100);
        g2.update(&f, None);
        // 局部修改 + hint 窄化：hint 只盖块 0、1（上半行），只比对它们。
        let mut f2 = f.clone();
        let top = Rect::new(0, 0, 128, 64);
        let seed = 0xDEAD_BEEF_u64;
        let mut s = seed;
        for y in 0..64usize {
            for x in 0..128usize {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                let off = y * f2.stride as usize + x * 4;
                f2.data[off..off + 3].copy_from_slice(&[
                    (s >> 33) as u8,
                    (s >> 41) as u8,
                    (s >> 49) as u8,
                ]);
            }
        }
        assert_eq!(g2.update(&f2, None), 2);
        assert_eq!(g2.changed_blocks(), &[0, 1]);
        // 新哈希已存储：无论全量还是 hint 复扫都无变化。
        assert_eq!(g2.update(&f2, None), 0);
        assert_eq!(g2.update(&f2, Some(&[top])), 0);
    }

    #[test]
    fn resize_rebuilds_grid_all_dirty() {
        let f = pattern_frame(128, 128, 3);
        let mut g = BlockGrid::new(128, 128);
        assert_eq!(g.update(&f, None), 4);
        let f2 = pattern_frame(256, 128, 5);
        assert_eq!(g.update(&f2, None), 8); // 全部块视为变更
        assert_eq!(g.cols(), 4);
        assert_eq!(g.generation(), 2);
        assert_eq!(g.update(&f2, None), 0);
    }

    #[test]
    fn edge_blocks_cover_remainder() {
        // 130x70：3 列 x 2 行，右/下边缘块 2x6 / 64x6 / 2x6。
        let f = pattern_frame(130, 70, 11);
        let mut g = BlockGrid::new(130, 70);
        assert_eq!(g.update(&f, None), 6);
        assert_eq!(g.update(&f, None), 0);
        // 修改右下角 1 像素只影响边缘块 (bx=2, by=1)。
        let mut f2 = f.clone();
        let off = (69 * f2.stride as usize) + 129 * 4;
        f2.data[off] ^= 0xFF;
        assert_eq!(g.update(&f2, None), 1);
        let (by, bx) = (1u32, 2u32); // 右下边缘块
        let cols = 3u32;
        assert_eq!(g.changed_blocks(), &[by * cols + bx]);
    }
}
