//! 按内容切块（FastCDC）。在哪里切只看那一小段内容本身，不看它在文件里的
//! 位置：一个文件改了一处，只有那附近的一两块变化，前后的块原样不动；整段
//! 内容挪了位置，块也认得出来。
//!
//! 算法与参数结构 derived from zephyr4123/snug (MIT)，Sources/SnugCore/Update/
//! Chunker.swift，重写为 Rust。与 snug 的差异：gear 表种子用 `rd-gear-i`（种
//! 子只需项目内一致：发布端与客户端用同一份本 crate 代码与参数，切点即一致）；
//! 常数 min/avg/max 32Ki/128Ki/512Ki 沿用 snug 实测基线（改造计划 §3：其调参
//! 依据是 ~10MB 级 App 包，remoteG zip 体积分布不同，后续按产物实测调参）。
//!
//! 关键不变量：同一份参数（min/avg/max）切同一内容，任何时间任何机器得到
//! 的切点完全一致——gear 表由固定种子派生，无随机成分。

use sha2::{Digest, Sha256};

/// 每个字节值对应的随机数（gear 表），由固定的种子算出来：发布和更新、
/// 每个版本都一样。
fn gear_table(seed_prefix: &str) -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut h = Sha256::new();
        h.update(format!("{seed_prefix}-{i}").as_bytes());
        let digest = h.finalize();
        *slot = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    }
    table
}

/// 内容定义切块器（FastCDC）。发布端与客户端共用同一份 [`Chunker`] 代码与
/// 参数；参数记录在清单里供跨版本校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunker {
    pub min_size: usize,
    pub avg_size: usize,
    pub max_size: usize,
    gear: [u32; 256],
    strict_mask: u32,
    loose_mask: u32,
}

impl Chunker {
    /// 用 snug 实测基线参数构造（32Ki/128Ki/512Ki）。
    pub fn with_defaults() -> Self {
        Self::new(32 * 1024, 128 * 1024, 512 * 1024)
    }

    /// 自定义参数构造。要求 `0 < min < avg < max` 且 avg 为 2 的幂（掩码推导
    /// 假设 log2(avg) 为整数）。
    pub fn new(min_size: usize, avg_size: usize, max_size: usize) -> Self {
        assert!(
            min_size > 0 && min_size < avg_size && avg_size < max_size,
            "切块参数必须满足 0 < min < avg < max"
        );
        let bits = (avg_size as f64).log2().round() as u32;
        debug_assert_eq!(1usize << bits, avg_size, "avg_size 必须是 2 的幂");
        // 判定切点的掩码，取哈希的高位（高位受前面更多字节影响）。不到平均
        // 长度时多看一位（难切），过了平均长度少看一位（好切）。
        let strict_mask = (((1u64 << (bits + 1)) - 1) << (31 - bits)) as u32;
        let loose_mask = (((1u64 << (bits - 1)) - 1) << (33 - bits)) as u32;
        Self {
            min_size,
            avg_size,
            max_size,
            gear: gear_table("rd-gear"),
            strict_mask,
            loose_mask,
        }
    }

    /// 依次的每一块在 `data` 里的范围。空内容没有块。
    pub fn cuts(&self, data: &[u8]) -> Vec<std::ops::Range<usize>> {
        let mut cuts: Vec<std::ops::Range<usize>> = Vec::new();
        let mut start = 0usize;
        while start < data.len() {
            let len = self.cut_length(data, start);
            cuts.push(start..start + len);
            start += len;
        }
        cuts
    }

    /// 从 `start` 起的这一块有多长。
    pub fn cut_length(&self, data: &[u8], start: usize) -> usize {
        let remaining = data.len() - start;
        if remaining <= self.min_size {
            return remaining;
        }
        let end = remaining.min(self.max_size);
        let normal = self.avg_size.min(end);
        let mut hash: u32 = 0;
        for i in self.min_size..end {
            hash = (hash << 1).wrapping_add(self.gear[data[start + i] as usize]);
            let mask = if i < normal {
                self.strict_mask
            } else {
                self.loose_mask
            };
            if hash & mask == 0 {
                return i;
            }
        }
        end
    }
}
