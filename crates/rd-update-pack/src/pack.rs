//! 更新包清单与拼装逻辑。
//!
//! pack（`rd-agent-<版本>-win64.pack`）里依次放着新 zip 的每一块：逐块 zstd
//! 压缩、首尾相接。清单记着每块在包里的位置（offset/length 是压缩后的），
//! 以及新 zip 的整体 sha256（拼装全量校验）。
//!
//! 清单结构 derived from zephyr4123/snug (MIT)，Sources/SnugCore/Update/
//! UpdatePackage.swift，重写为 Rust。与 snug 的差异：remoteG Agent 分发形态是
//! 单个便携 zip，拼装 = 按清单块序拼接（snug 的 files 目录树映射暂不需要，
//! 字段留未来扩展；serde 忽略未知字段，新旧清单双向兼容）。
//!
//! 不挑从哪个版本升：缺什么取什么。每块都核对 sha256，拼出来的与发布产物
//! 逐字节相同（pack_sha256 全量校验）。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::chunker::Chunker;
use crate::sha256_hex;

/// 清单格式版本。清单结构不兼容演进时递增；读取方按此判断能否消费。
pub const PACK_FORMAT_VERSION: u32 = 1;

/// 更新包清单（即 `update.json` 的内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// 清单格式版本（serde 默认 1：旧清单没有该字段按 v1 处理）。
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    /// 目标版本号（x.y.z）。
    pub version: String,
    /// 包的文件名，与 update.json 放在同一个版本目录下。
    pub pack_file: String,
    /// 新 zip 的整体 sha256（hex 小写）：拼装全量校验 + 「本机已是该版本」判定。
    pub pack_sha256: String,
    /// 新 zip 总字节数。
    pub pack_size: u64,
    /// 切块参数：跨版本必须一致，不一致时调用方应放弃缺块更新（回退整包）。
    pub chunker: ChunkerParams,
    /// 包里的块，按在包里的顺序。
    pub chunks: Vec<ChunkInfo>,
}

/// 切块参数记录。清单参数与本机切块参数不一致 = 切法变了，缺块对比失真。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkerParams {
    pub min_size: u64,
    pub avg_size: u64,
    pub max_size: u64,
}

impl From<&Chunker> for ChunkerParams {
    fn from(c: &Chunker) -> Self {
        Self {
            min_size: c.min_size as u64,
            avg_size: c.avg_size as u64,
            max_size: c.max_size as u64,
        }
    }
}

/// 包里的一个块：原始内容 sha256 + 在包里的字节范围（压缩后的）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkInfo {
    /// 原始内容（压缩前）的 sha256，hex 小写。
    pub sha256: String,
    /// 在包里从哪个字节开始（压缩后的数据）。
    pub offset: u64,
    /// 占多少字节（压缩后的）。
    pub length: u64,
}

fn default_format_version() -> u32 {
    PACK_FORMAT_VERSION
}

/// 清单校验失败。
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("清单格式不支持: format_version={0}（本机支持 <= {PACK_FORMAT_VERSION}）")]
    NewerFormat(u32),
    #[error("包的文件名不对: {0}")]
    BadPackFile(String),
    #[error("块 {sha} 在包里的位置不对: {reason}")]
    BadChunk { sha: String, reason: String },
    #[error("路径不对: {0}")]
    BadPath(String),
    #[error("字段不对: {0}")]
    BadField(String),
}

impl Manifest {
    /// 发布端打包：把新 zip 切块打 pack，交回清单与包的内容。同一份 zip 每
    /// 次打出来一字不差（切块确定性 + zstd 确定性）。
    ///
    /// 有意不去重：清单是扁平的切点序列（每块首尾相接），重复内容块重复入
    /// 包——重复块在真实产物里稀少，多存的几份换来了校验（首尾相接）、下载
    /// （缺块范围连续）、拼装（按序拼接）三条规则零特例。snug 用
    /// files[].chunks 结构承载去重，扁平结构硬塞得不偿失。
    pub fn make(
        version: &str,
        pack_file: &str,
        content: &[u8],
        chunker: &Chunker,
    ) -> (Manifest, Vec<u8>) {
        let mut pack: Vec<u8> = Vec::new();
        let mut chunks: Vec<ChunkInfo> = Vec::new();
        for cut in chunker.cuts(content) {
            let raw = &content[cut];
            let packed = zstd::stream::encode_all(raw, 3).expect("zstd 压缩不失败");
            chunks.push(ChunkInfo {
                sha256: sha256_hex(raw),
                offset: pack.len() as u64,
                length: packed.len() as u64,
            });
            pack.extend_from_slice(&packed);
        }
        let manifest = Manifest {
            format_version: PACK_FORMAT_VERSION,
            version: version.to_string(),
            pack_file: pack_file.to_string(),
            pack_sha256: sha256_hex(content),
            pack_size: content.len() as u64,
            chunker: ChunkerParams::from(chunker),
            chunks,
        };
        (manifest, pack)
    }

    /// `update.json` 文件字节 → 清单。解析后立即核对（结构与网络数据不可信）。
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| ManifestError::BadField(format!("update.json 解析失败: {e}")))?;
        m.validate()?;
        Ok(m)
    }

    /// 解析后核对一遍：包名是普通文件名、块在包里首尾相接、版本号/哈希格式
    /// 合法。清单来自网络，先当不可信数据处理。
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.format_version > PACK_FORMAT_VERSION {
            return Err(ManifestError::NewerFormat(self.format_version));
        }
        let name_ok = !self.pack_file.is_empty()
            && !self.pack_file.contains(['/', '\\'])
            && self.pack_file != ".."
            && self.pack_file != ".";
        if !name_ok {
            return Err(ManifestError::BadPackFile(self.pack_file.clone()));
        }
        if !is_semverish(&self.version) {
            return Err(ManifestError::BadField(format!(
                "版本号格式不对: {}",
                self.version
            )));
        }
        if self.pack_sha256.len() != 64 || !self.pack_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ManifestError::BadField("pack_sha256 不是 64 位 hex".into()));
        }
        let mut offset = 0u64;
        for chunk in &self.chunks {
            // 长度来自网络：加法按 checked 防 u64 溢出
            let next = offset
                .checked_add(chunk.length)
                .ok_or_else(|| ManifestError::BadChunk {
                    sha: short_sha(&chunk.sha256),
                    reason: "长度溢出".into(),
                })?;
            if chunk.offset != offset || chunk.length == 0 {
                return Err(ManifestError::BadChunk {
                    sha: short_sha(&chunk.sha256),
                    reason: format!(
                        "期望 offset={} length>0，实际 offset={} length={}",
                        offset, chunk.offset, chunk.length
                    ),
                });
            }
            if chunk.sha256.len() != 64 || !chunk.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(ManifestError::BadChunk {
                    sha: short_sha(&chunk.sha256),
                    reason: "sha256 不是 64 位 hex".into(),
                });
            }
            offset = next;
        }
        let _ = offset;
        Ok(())
    }

    /// 本机已有的块：把本机 zip 按清单里记录的参数切开，sha256 → 内容。
    /// 清单参数与本机切块参数不一致时返回 None（调用方回退整包下载）。
    pub fn chunks_in(
        &self,
        local_zip: &[u8],
        local_chunker: &Chunker,
    ) -> Option<HashMap<String, Vec<u8>>> {
        if ChunkerParams::from(local_chunker) != self.chunker {
            return None;
        }
        let mut out: HashMap<String, Vec<u8>> = HashMap::new();
        for cut in local_chunker.cuts(local_zip) {
            let raw = &local_zip[cut];
            out.insert(sha256_hex(raw), raw.to_vec());
        }
        Some(out)
    }

    /// 还缺的块在包里的字节范围：按包里的顺序，相邻的并成一段（一段一次
    /// 请求，也即断点续传的自然单元）。
    pub fn missing_ranges(&self, have: &HashSet<&str>) -> Vec<Range<u64>> {
        let mut ranges: Vec<Range<u64>> = Vec::new();
        for chunk in &self.chunks {
            if have.contains(chunk.sha256.as_str()) {
                continue;
            }
            let range = chunk.offset..chunk.offset + chunk.length;
            if let Some(last) = ranges.last_mut() {
                if last.end == range.start {
                    last.end = range.end;
                    continue;
                }
            }
            ranges.push(range);
        }
        ranges
    }

    /// 从包里取回的一段（`at` 是它在包里的字节范围）解出其中的块，逐块核对
    /// sha256。返回 sha256 → 原始内容。
    pub fn unpack(
        &self,
        data: &[u8],
        at: &Range<u64>,
    ) -> Result<HashMap<String, Vec<u8>>, ManifestError> {
        if data.len() as u64 != at.end - at.start {
            return Err(ManifestError::BadChunk {
                sha: String::new(),
                reason: format!("取回 {} 字节，应为 {}", data.len(), at.end - at.start),
            });
        }
        let mut out: HashMap<String, Vec<u8>> = HashMap::new();
        for chunk in &self.chunks {
            let start = chunk.offset;
            let end = chunk.offset + chunk.length;
            if start < at.start || end > at.end {
                continue;
            }
            let begin = (start - at.start) as usize;
            let packed = &data[begin..begin + chunk.length as usize];
            let raw = zstd::stream::decode_all(packed).map_err(|_| ManifestError::BadChunk {
                sha: short_sha(&chunk.sha256),
                reason: "解压失败".into(),
            })?;
            if sha256_hex(&raw) != chunk.sha256 {
                return Err(ManifestError::BadChunk {
                    sha: short_sha(&chunk.sha256),
                    reason: "解压后内容对不上".into(),
                });
            }
            out.insert(chunk.sha256.clone(), raw);
        }
        Ok(out)
    }

    /// 用这些块拼出新 zip。块要先取齐（缺块返回 Err）；拼完核对整体 sha256
    /// 与 pack_size——通过即与发布产物逐字节相同。
    pub fn assemble(&self, available: &HashMap<String, Vec<u8>>) -> Result<Vec<u8>, ManifestError> {
        let have: HashSet<&str> = available.keys().map(|s| s.as_str()).collect();
        let missing = self.missing_ranges(&have);
        if !missing.is_empty() {
            return Err(ManifestError::BadField(format!(
                "还有 {} 段缺块未取齐",
                missing.len()
            )));
        }
        let mut out = Vec::with_capacity(self.pack_size as usize);
        for chunk in &self.chunks {
            let raw = available
                .get(&chunk.sha256)
                .ok_or_else(|| ManifestError::BadChunk {
                    sha: short_sha(&chunk.sha256),
                    reason: "拼装时缺块".into(),
                })?;
            out.extend_from_slice(raw);
        }
        if out.len() as u64 != self.pack_size || sha256_hex(&out) != self.pack_sha256 {
            return Err(ManifestError::BadField(
                "拼装结果与清单不符（全量校验失败）".into(),
            ));
        }
        Ok(out)
    }
}

/// 版本号宽松校验：`x[.y[.z…]]` 数字段。清单侧拒绝明显非法值，比较逻辑在
/// rd-agent 侧（见 `updater::compare_versions`）。
fn is_semverish(v: &str) -> bool {
    !v.is_empty()
        && v.split('.')
            .all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}
