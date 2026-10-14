//! T-013 信任列表测试：增删查、原子写入、重启后读取；身份持久化非明文（Windows DPAPI）。

use std::fs;

use rd_security::{load_or_create_identity, TrustStore, TrustedPeer};
use rd_transport::Fingerprint;
use serde_json::json;

/// 构造一个确定的测试指纹。
fn fp(seed: u8) -> Fingerprint {
    Fingerprint([seed; 32])
}

#[test]
fn add_remove_lookup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trusted.json");

    let mut store = TrustStore::open(&path).expect("open");
    assert!(store.list().is_empty(), "新文件应为空列表");
    assert!(!store.is_trusted(&fp(1)));

    store.add(fp(1), "设备A").expect("add 1");
    store.add(fp(2), "设备B").expect("add 2");
    assert_eq!(store.list().len(), 2);
    assert!(store.is_trusted(&fp(1)));
    assert!(store.is_trusted(&fp(2)));
    assert!(!store.is_trusted(&fp(3)));

    // 重复 add：仅更新名称，不产生重复条目
    store.add(fp(1), "设备A-改名").expect("add again");
    assert_eq!(store.list().len(), 2);
    assert_eq!(store.list()[0].name, "设备A-改名");

    store.remove(&fp(1)).expect("remove");
    assert!(!store.is_trusted(&fp(1)));
    assert!(store.is_trusted(&fp(2)));
    store.remove(&fp(1)).expect("remove 不存在时幂等");
    assert_eq!(store.list().len(), 1);
}

#[test]
fn reload_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trusted.json");
    {
        let mut store = TrustStore::open(&path).expect("open");
        store.add(fp(7), "work-laptop").expect("add");
    } // store 在此析构，模拟进程重启

    let store = TrustStore::open(&path).expect("reopen");
    let peers = store.list();
    assert_eq!(peers.len(), 1, "重启后应读到持久化条目");
    assert_eq!(peers[0].fingerprint_hex, fp(7).hex());
    assert_eq!(peers[0].name, "work-laptop");
    assert_eq!(peers[0].added_at, peers[0].last_seen, "加入时两时间戳相同");
}

#[test]
fn touch_updates_last_seen_keeps_added_at() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trusted.json");
    // 预置一个“很久以前”的条目
    let seed = json!([{
        "fingerprint_hex": fp(9).hex(),
        "name": "old-device",
        "added_at": 1000,
        "last_seen": 1000
    }]);
    fs::write(&path, seed.to_string()).expect("写种子文件");

    let mut store = TrustStore::open(&path).expect("open");
    assert!(store.is_trusted(&fp(9)), "外部写入的条目应可识别");
    store.touch(&fp(9)).expect("touch");
    store.touch(&fp(1)).expect("touch 不存在的指纹应静默成功");
    drop(store);

    let store = TrustStore::open(&path).expect("reopen");
    let peers = store.list();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].added_at, 1000, "added_at 不得被 touch 改动");
    assert!(peers[0].last_seen > 1000, "last_seen 应被刷新");
}

#[test]
fn atomic_save_leaves_no_tmp_and_valid_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trusted.json");

    let mut store = TrustStore::open(&path).expect("open");
    store.add(fp(1), "a").expect("add 1");
    store.add(fp(2), "b").expect("add 2");
    store.remove(&fp(1)).expect("remove 1");
    store.touch(&fp(2)).expect("touch 2");

    // 无 .tmp 残留（临时文件 + rename 原子替换）
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .expect("read_dir")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp"))
        .collect();
    assert!(leftovers.is_empty(), "不应残留临时文件：{leftovers:?}");

    // 目标文件是合法 JSON 数组（05_api/任务卡契约格式）
    let raw = fs::read_to_string(&path).expect("read trusted.json");
    let peers: Vec<TrustedPeer> = serde_json::from_str(&raw).expect("应为合法 JSON 数组");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].fingerprint_hex, fp(2).hex());
}

#[cfg(windows)]
#[test]
fn identity_bin_is_not_plaintext_der() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _identity = load_or_create_identity(dir.path(), "agent-test").expect("创建身份");

    let bin = fs::read(dir.path().join("identity.bin")).expect("identity.bin 应存在");
    assert!(!bin.is_empty());
    // 0x30 = DER SEQUENCE 标签（证书与 PKCS#8 私钥均以 0x30 开头）
    assert_ne!(bin[0], 0x30, "identity.bin 不应是明文 DER，应经 DPAPI 加密");
}

#[test]
fn identity_reload_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = load_or_create_identity(dir.path(), "agent-test").expect("首次创建");
    let second = load_or_create_identity(dir.path(), "agent-test").expect("再次加载");

    // 同一目录重复加载应得到同一身份，而不是生成新身份
    assert_eq!(first, second);
    assert_eq!(first.fingerprint(), second.fingerprint());
    assert_eq!(first.cert_der, second.cert_der);
    assert_eq!(first.key_der, second.key_der);
}
