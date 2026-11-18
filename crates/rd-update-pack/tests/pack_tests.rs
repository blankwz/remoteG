//! rd-update-pack 集成测试：切块确定性、内容挪位识别、清单校验、双版本
//! 模拟升级「只走缺块请求」的核心验收。
//!
//! 测试用小切块参数（64/256/1024 字节）让内容变化落在块级粒度上，便于断言。

use std::collections::{HashMap, HashSet};

use rd_update_pack::{compare_versions, Chunker, Manifest, PACK_FORMAT_VERSION};

fn test_chunker() -> Chunker {
    Chunker::new(64, 256, 1024)
}

/// 造一段伪随机但确定的内容（每字节由位置决定），n 字节。
fn synthetic(n: usize, salt: u8) -> Vec<u8> {
    (0..n)
        .map(|i| {
            let x = (i as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(salt as u64);
            ((x >> 33) & 0xFF) as u8
        })
        .collect()
}

/// 模拟 HTTP Range 下载：从 pack 里切出指定字节范围。
fn fetch_range(pack: &[u8], range: std::ops::Range<u64>) -> Vec<u8> {
    pack[range.start as usize..range.end as usize].to_vec()
}

#[test]
fn chunking_is_deterministic() {
    let c = test_chunker();
    let data = synthetic(50_000, 1);
    let cuts1 = c.cuts(&data);
    let cuts2 = c.cuts(&data);
    assert_eq!(cuts1, cuts2, "同参数同内容两次切块必须完全一致");
    // 块不重不漏，拼回原样
    let mut rebuilt = Vec::new();
    for cut in &cuts1 {
        rebuilt.extend_from_slice(&data[cut.clone()]);
    }
    assert_eq!(rebuilt, data);
}

#[test]
fn content_shift_is_recognized() {
    // 内容整体挪位（前面加一段），中间块应被认出来（FastCDC 的价值所在）
    let c = test_chunker();
    let body = synthetic(20_000, 7);
    let shifted = {
        let mut v = synthetic(3_000, 99);
        v.extend_from_slice(&body);
        v
    };
    let cuts_body: HashSet<String> = c
        .cuts(&body)
        .iter()
        .map(|r| rd_update_pack::sha256_hex(&body[r.clone()]))
        .collect();
    let cuts_shifted: HashSet<String> = cuts_shifted_of(&c, &shifted);
    let common = cuts_body.intersection(&cuts_shifted).count();
    assert!(
        common >= 3,
        "挪位后至少多数中间块应被认出，实际共同块 {common}"
    );
}

fn cuts_shifted_of(c: &Chunker, data: &[u8]) -> HashSet<String> {
    c.cuts(data)
        .iter()
        .map(|r| rd_update_pack::sha256_hex(&data[r.clone()]))
        .collect()
}

#[test]
fn single_edit_changes_only_nearby_chunks() {
    // 改一处只影响附近块：前后块原样保留——缺块更新的省流量依据
    let c = test_chunker();
    let v1 = synthetic(30_000, 3);
    let mut v2 = v1.clone();
    v2[15_000] ^= 0xFF;
    let set1 = cuts_shifted_of(&c, &v1);
    let set2 = cuts_shifted_of(&c, &v2);
    let kept = set1.intersection(&set2).count();
    let total = set1.len();
    assert!(
        kept >= total * 3 / 4,
        "改 1 字节后应保留绝大部分块：kept {kept}/{total}"
    );
    assert!(kept < total, "改动的块必须至少变化一块，否则测试无意义");
}

#[test]
fn pack_roundtrip_is_byte_identical() {
    let c = test_chunker();
    let content = synthetic(100_000, 5);
    let (manifest, pack) = Manifest::make("0.3.0", "rd-agent-0.3.0-win64.pack", &content, &c);
    let decoded = Manifest::decode(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    let chunks: HashMap<String, Vec<u8>> = decoded.unpack(&pack, &(0..pack.len() as u64)).unwrap();
    let rebuilt = decoded.assemble(&chunks).unwrap();
    assert_eq!(rebuilt, content, "拼装结果必须与原内容逐字节相同");
}

#[test]
fn empty_content_roundtrip() {
    let c = test_chunker();
    let (manifest, pack) = Manifest::make("0.1.0", "p.pack", &[], &c);
    assert!(manifest.chunks.is_empty(), "空内容无块");
    assert!(pack.is_empty());
    let rebuilt = manifest.assemble(&HashMap::new()).unwrap();
    assert!(rebuilt.is_empty());
}

#[test]
fn corrupt_pack_chunk_is_rejected() {
    // 坏块注入：篡改 pack 里的一个字节 → unpack 必须报错（解压失败或哈希不符）
    let c = test_chunker();
    let content = synthetic(50_000, 11);
    let (manifest, mut pack) = Manifest::make("0.3.0", "p.pack", &content, &c);
    let mid = pack.len() / 2;
    pack[mid] ^= 0xA5;
    let result = manifest.unpack(&pack, &(0..pack.len() as u64));
    assert!(result.is_err(), "篡改后的 pack 必须被逐块校验拦下");
}

#[test]
fn malicious_manifests_are_rejected() {
    let c = test_chunker();
    let content = synthetic(10_000, 13);
    let (mut manifest, _pack) = Manifest::make("0.3.0", "p.pack", &content, &c);

    // 新于本机支持的格式版本
    manifest.format_version = PACK_FORMAT_VERSION + 1;
    assert!(manifest.validate().is_err());
    manifest.format_version = PACK_FORMAT_VERSION;

    // 包名带路径分隔符（路径穿越）
    manifest.pack_file = "../evil.pack".to_string();
    assert!(manifest.validate().is_err());
    manifest.pack_file = "sub/evil.pack".to_string();
    assert!(manifest.validate().is_err());
    manifest.pack_file = "p.pack".to_string();

    // sha256 格式不对
    manifest.pack_sha256 = "deadbeef".to_string();
    assert!(manifest.validate().is_err());

    // 块位置不首尾相接 / 长度溢出
    manifest.pack_sha256 = rd_update_pack::sha256_hex(&content);
    let original = manifest.chunks.clone();
    manifest.chunks[1].offset += 1;
    assert!(manifest.validate().is_err(), "块偏移不衔接必须被拒");
    manifest.chunks = original.clone();
    manifest.chunks.last_mut().unwrap().length = u64::MAX;
    assert!(
        manifest.validate().is_err(),
        "长度溢出必须被拒（checked_add）"
    );
    manifest.chunks = original;
    assert!(manifest.validate().is_ok());
}

#[test]
fn chunker_params_mismatch_signals_full_fallback() {
    let content = synthetic(20_000, 17);
    let (manifest, _) = Manifest::make("0.3.0", "p.pack", &content, &test_chunker());
    // 本机切块参数与清单不一致 → chunks_in 返回 None（调用方回退整包）
    let other = Chunker::new(128, 512, 2048);
    assert!(manifest.chunks_in(&content, &other).is_none());
    // 参数一致 → 正常给出本机块
    let local = manifest.chunks_in(&content, &test_chunker()).unwrap();
    assert!(!local.is_empty());
}

#[test]
fn compare_versions_orders_numerically() {
    use std::cmp::Ordering;
    assert_eq!(compare_versions("0.3.0", "0.2.9"), Ordering::Greater);
    assert_eq!(
        compare_versions("0.10.0", "0.9.0"),
        Ordering::Greater,
        "数字比较非字典序"
    );
    assert_eq!(compare_versions("0.3.0", "0.3.0"), Ordering::Equal);
    assert_eq!(
        compare_versions("0.3.0", "0.3.0.0"),
        Ordering::Equal,
        "缺段补零等价"
    );
    assert_eq!(compare_versions("1.0.0", "0.3.0"), Ordering::Greater);
    assert_eq!(compare_versions("0.3.1", "0.3"), Ordering::Greater);
}

/// 核心验收：双版本模拟升级，只走缺块请求。
///
/// v1 已在本机（旧 zip）。发布 v2（在 v1 中间改了一处）。流程对齐
/// rd-agent 自更新执行器将要走的路径：
/// 1. 本机 zip 切块得已有块集（`chunks_in`）
/// 2. 清单算缺块范围（`missing_ranges`，相邻合并）
/// 3. 对每个范围做 Range 下载（模拟：从 v2 pack 切片）
/// 4. 取回段解块核对（`unpack`），并入已有块集
/// 5. 拼装（`assemble`）——与 v2 逐字节相同
///
/// 并断言：请求的字节数 < 整包，请求次数 < 块总数。
#[test]
fn two_version_upgrade_fetches_only_missing_ranges() {
    let c = test_chunker();
    let v1 = synthetic(120_000, 21);
    let mut v2 = v1.clone();
    v2[60_000] ^= 0xFF; // 中间改 1 字节

    let (m1, pack1) = Manifest::make("0.1.0", "rd-agent-0.1.0-win64.pack", &v1, &c);
    let _ = pack1; // 旧 pack 不参与升级
    let (m2, pack2) = Manifest::make("0.2.0", "rd-agent-0.2.0-win64.pack", &v2, &c);
    assert_ne!(m1.pack_sha256, m2.pack_sha256);

    // 1. 本机已有的块（模拟：旧 zip 还在）
    let local = m2.chunks_in(&v1, &c).expect("参数一致时必须给出本机块");
    let have: HashSet<&str> = local.keys().map(|s| s.as_str()).collect();

    // 2. 缺块范围（相邻合并）
    let ranges = m2.missing_ranges(&have);
    let fetched_bytes: usize = ranges.iter().map(|r| (r.end - r.start) as usize).sum();
    assert!(
        fetched_bytes < pack2.len() / 2,
        "改 1 字节的升级不应拉超过半个包：拉 {fetched_bytes} / 全包 {}",
        pack2.len()
    );
    assert!(
        ranges.len() < m2.chunks.len(),
        "相邻合并后请求数应远小于块总数：{} 段 / {} 块",
        ranges.len(),
        m2.chunks.len()
    );

    // 3+4. Range 下载 + 解块核对
    let mut available = local;
    for r in &ranges {
        let segment = fetch_range(&pack2, r.clone());
        let recovered = m2.unpack(&segment, r).expect("取回段解块必须通过");
        available.extend(recovered);
    }

    // 5. 拼装与 v2 逐字节相同
    let rebuilt = m2.assemble(&available).expect("缺块取齐后拼装必须成功");
    assert_eq!(rebuilt, v2, "升级产物必须与发布产物逐字节相同");
    assert_eq!(rd_update_pack::sha256_hex(&rebuilt), m2.pack_sha256);
}
