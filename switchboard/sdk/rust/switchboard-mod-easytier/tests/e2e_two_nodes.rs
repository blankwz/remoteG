//! 双节点本机 e2e（默认 ignore；需真实 easytier 二进制）。
//!
//! 运行方式：
//! ```bash
//! SB_ET_CORE=path/to/easytier-core.exe SB_ET_CLI=path/to/easytier-cli.exe \
//!   cargo test -p switchboard-mod-easytier --test e2e_two_nodes -- --ignored --nocapture
//! ```
//!
//! 验证：同网络名两节点（A 监听 / B 引导指 A）在 no_tun 模式下完成组网，
//! 经 rpc-portal 查 A 的 route 能看到 B 的虚拟 IP——发现链路（S1）真实闭环。

use std::path::{Path, PathBuf};
use std::time::Duration;

use switchboard_mod_easytier::{cli_json, config::EasyTierNetConfig, EasyTierNode};

fn bin_from_env(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[tokio::test]
#[ignore = "需要 easytier 二进制（SB_ET_CORE / SB_ET_CLI）"]
async fn 双节点组网_route互见() {
    let Some(core_bin) = bin_from_env("SB_ET_CORE") else {
        eprintln!("跳过：未设 SB_ET_CORE");
        return;
    };
    let Some(cli_bin) = bin_from_env("SB_ET_CLI") else {
        eprintln!("跳过：未设 SB_ET_CLI");
        return;
    };
    let work = std::env::temp_dir().join(format!("sb-et-e2e-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&work);

    let cfg_a = EasyTierNetConfig {
        network_name: format!("sb-easytier-e2e-{}", std::process::id()),
        network_secret: Some("e2e-secret".into()),
        listeners: vec!["udp://127.0.0.1:41001".into()],
        no_tun: true,
        rpc_portal: "127.0.0.1:41011".parse().unwrap(),
        ..Default::default()
    };
    let cfg_b = EasyTierNetConfig {
        network_name: cfg_a.network_name.clone(),
        network_secret: cfg_a.network_secret.clone(),
        peer_urls: vec!["udp://127.0.0.1:41001".into()],
        no_tun: true,
        rpc_portal: "127.0.0.1:41012".parse().unwrap(),
        ..Default::default()
    };

    let mut node_a = EasyTierNode::launch(&core_bin, &cfg_a, &work).expect("节点 A 启动");
    let mut node_b = EasyTierNode::launch(&core_bin, &cfg_b, &work).expect("节点 B 启动");
    let _ = (&node_a, &node_b);

    // 组网收敛（打洞/握手），轮询 route 直至互见或超时
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut found = None;
    while std::time::Instant::now() < deadline {
        let out = tokio::process::Command::new(&cli_bin)
            .arg("--rpc-portal")
            .arg(node_a.rpc_portal().to_string())
            .arg("--output")
            .arg("json")
            .arg("route")
            .output()
            .await
            .expect("easytier-cli 运行");
        if out.status.success() {
            let entries = cli_json::parse_route_json(&String::from_utf8_lossy(&out.stdout))
                .expect("route JSON 可解析");
            if let Some(e) = entries.iter().find(|e| e.ipv4.is_some()) {
                found = e.ipv4.clone();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let peer_ip = found.expect("30s 内未见对端虚拟 IP（组网失败）");
    println!("对端虚拟 IP: {peer_ip}");
    assert!(node_a.is_running() && node_b.is_running(), "节点中途退出");
    drop(node_a);
    drop(node_b);
    let _ = Path::new(&work); // work_dir 留待临时目录清理
    let _ = std::fs::remove_dir_all(&work);
}
