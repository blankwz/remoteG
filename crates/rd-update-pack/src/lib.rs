//! rd-update-pack — 更新包切块与清单（改造项 A，snug 借鉴）。
//!
//! 「任意旧版本 → 最新版，只下载缺块」：
//! - 发布端把 Agent 便携 zip 按内容切块打 pack（每块 zstd 压缩、sha256 认块），
//!   清单 `update.json` 随产物发布并对文件字节做 ed25519 签名（旁挂 `.sig`）。
//! - Agent 端把本机 zip 同法切块，按 sha256 对比清单得缺块集，HTTP Range 逐段
//!   拉取，拼装出新 zip（逐块+全量双重校验），原子对调后重启。
//!
//! 本 crate 只做纯逻辑（切块/清单/拼装/校验/签名），零 I/O：文件的读写、
//! 网络下载、进程对调由调用方（发布端 CLI 与 rd-agent 自更新执行器）负责。
//! 切块算法与清单结构参考 snug（zephyr4123/snug，MIT）的 Chunker/UpdatePackage
//! 重写为 Rust；压缩从 LZMA 换成 zstd（workspace 既有依赖）。
//!
//! 关键不变量：**发布端与客户端必须同一份切块代码与参数**——两者都依赖本
//! crate（snug 用 SnugCore 共享，remoteG 同构）。切块参数记录在清单里，
//! 跨版本校验参数一致，切法变了只是那一次更新认不出本机块（多下整包），
//! 不会出错。

pub mod chunker;
pub mod pack;
pub mod sign;

#[cfg(feature = "mirror")]
pub mod mirror;

pub use chunker::Chunker;
pub use pack::{ChunkInfo, ChunkerParams, Manifest, PACK_FORMAT_VERSION};
pub use sign::{generate_keypair, sign_bytes, verify_bytes};

/// 版本号比较：数字段逐段比，短的一方缺段按 0 补——`0.3.0` == `0.3.0.0`。
/// 非 ASCII 数字段按 0 处理（本 crate 只产生数字段，防御性兜底）。
///
/// 两处共用（对齐 snug Updater 的防回滚判定）：mirror 的 latest 指针防回滚、
/// rd-agent 更新检查的「远端是否比本机新」。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let parse = |s: &str| -> u64 { s.parse::<u64>().unwrap_or(0) };
    let (sa, sb) = (a.split('.'), b.split('.'));
    let mut ia = sa.map(parse);
    let mut ib = sb.map(parse);
    loop {
        let (va, vb) = (ia.next(), ib.next());
        match (va, vb) {
            (Some(va), Some(vb)) => {
                if va != vb {
                    return va.cmp(&vb);
                }
            }
            // 一方段数用尽：剩下段的数值非零才分胜负（全零尾段等价于缺段）
            (Some(v), None) => {
                if v != 0 {
                    return Ordering::Greater;
                }
            }
            (None, Some(v)) => {
                if v != 0 {
                    return Ordering::Less;
                }
            }
            (None, None) => return Ordering::Equal,
        }
    }
}

/// 块/内容指纹：sha256 十六进制小写。
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
