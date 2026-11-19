//! t11 更新全链（手测清单 §5.1/5.2/5.3 自动化）。
//!
//! - `t11a_update_pack_library`：纯库直测——check（发现/防回滚/坏签名）、
//!   apply full/chunked、坏块注入、中断重试（暂存残留清理）、只读回退
//!   （icacls deny → 下载前失败）。全部在测试进程内完成（tempdir 模拟安装
//!   目录 + 本地最小 HTTP 发布端），不 spawn 子进程。
//! - `t11b_update_e2e_full` / `t11c_update_e2e_chunked`：e2e 全链——
//!   `RD_UPDATE_TEST_CONFIRM=1` 直通确认框 + `RD_UPDATE_TEST_AUTO=1` 自动
//!   触发（照 RD_TEST_CRASH 先例的 debug 钩子），Agent 子进程走「检查 →
//!   下载 → 拼装 → 对调 → spawn 新 exe → exit(0)」真实链路，断言新进程
//!   打印 RD_LISTEN（`--print-port` 透传）、marker 文件就位、无 `*.old`
//!   残留（新进程启动清理），chunked 额外断言下载字节数显著小于全包。
//!
//! 本模块用例自带安装目录（tempdir）、发布端（本地 HTTP）与 Agent 实例，
//! 不触碰共享 env（common::env()）与其他用例的进程资源。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rd_update_pack::{generate_keypair, sign_bytes, Chunker, Manifest};
use tempfile::TempDir;

use crate::common::{self, Client};

/// 假新版清单版本号（只须大于当前 0.3.0，触发「有可用更新」）。
const FAKE_VERSION: &str = "9.9.9";
/// 更新包内的哨兵文件名：对调后出现在安装目录 = 新版文件确实就位。
const MARKER_NAME: &str = "rd-update-e2e-marker.txt";
/// marker 内容：新版 `new` / 旧 zip（chunked 对比用）`old`。
const MARKER_NEW: &[u8] = b"marker-new";
const MARKER_OLD: &[u8] = b"marker-old";
/// 清单里 pack 文件名。
const PACK_NAME: &str = "t11.pack";
/// 本机旧 zip 文件名（updater::find_local_zip 的匹配模式）。
const LOCAL_ZIP_NAME: &str = "rd-agent-0.3.0-win64.zip";

// ---------------------------------------------------------------------------
// t11a 纯库直测
// ---------------------------------------------------------------------------

/// rd_update_pack / updater 纯库直测（§5.1/5.2/5.3 的库层语义）。
#[tokio::test(flavor = "multi_thread")]
async fn t11a_update_pack_library() -> Result<()> {
    t11a_check_scenarios().await?;
    println!("t11a ok: check 场景（发现/防回滚/坏签名）");
    t11a_apply_full().await?;
    println!("t11a ok: apply full（对调 + marker 就位 + .old 清理）");
    t11a_apply_chunked().await?;
    println!("t11a ok: apply chunked（Range 缺块下载 < 全包）");
    t11a_bad_chunk().await?;
    println!("t11a ok: 坏块注入（解块校验失败，安装目录未被污染）");
    t11a_staging_leftover().await?;
    println!("t11a ok: 中断重试（暂存残留清理后重装成功）");
    t11a_readonly_fallback().await?;
    println!("t11a ok: 只读回退（icacls deny → 下载前失败，文件未动）");
    Ok(())
}

async fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .context("构建测试 HTTP 客户端失败")
}

/// check 三场景：新版本可发现；同版本/低版本拒（防回滚）；坏签名 fail-closed。
async fn t11a_check_scenarios() -> Result<()> {
    let exe = fake_exe_bytes()?;
    let new_zip = build_zip(&[("rd-agent.exe", &exe), (MARKER_NAME, MARKER_NEW)])?;
    let release = make_release(FAKE_VERSION, &new_zip)?;
    let server = ReleaseServer::spawn(release.files());
    let http = http_client().await?;
    let settings = release.settings(server.addr, rd_agent::updater::UpdateMode::Full);

    // 有可用更新
    let m = rd_agent::updater::check(&http, &settings, "0.3.0")
        .await?
        .context("9.9.9 > 0.3.0 应报有可用更新")?;
    anyhow::ensure!(m.version == FAKE_VERSION, "清单版本应为 {FAKE_VERSION}");
    // 防回滚：同版本 / 本机更新
    anyhow::ensure!(
        rd_agent::updater::check(&http, &settings, FAKE_VERSION)
            .await?
            .is_none(),
        "同版本应报无可用更新"
    );

    // 低版本发布端（0.1.0 < 本机 0.3.0）→ 拒
    let old_zip = build_zip(&[("rd-agent.exe", &exe), (MARKER_NAME, MARKER_NEW)])?;
    let old_release = make_release("0.1.0", &old_zip)?;
    let old_server = ReleaseServer::spawn(old_release.files());
    let old_settings = old_release.settings(old_server.addr, rd_agent::updater::UpdateMode::Full);
    anyhow::ensure!(
        rd_agent::updater::check(&http, &old_settings, "0.3.0")
            .await?
            .is_none(),
        "远端 0.1.0 < 本机 0.3.0 应拒（防回滚）"
    );

    // 坏签名：.sig 用另一密钥生成 → fail-closed
    let (_other_seed, other_pub) = generate_keypair();
    let bad = FakeRelease {
        manifest: release.manifest.clone(),
        manifest_bytes: release.manifest_bytes.clone(),
        pack: release.pack.clone(),
        sig_bytes: sign_bytes(&other_pub, &release.manifest_bytes)?,
        pubkey_hex: release.pubkey_hex.clone(),
    };
    let bad_server = ReleaseServer::spawn(bad.files());
    let bad_settings = bad.settings(bad_server.addr, rd_agent::updater::UpdateMode::Full);
    let err = rd_agent::updater::check(&http, &bad_settings, "0.3.0")
        .await
        .err()
        .context("坏签名必须被拒")?;
    let msg = format!("{err:#}");
    anyhow::ensure!(
        msg.contains("签名验证失败"),
        "坏签名错误应含「签名验证失败」: {msg}"
    );
    Ok(())
}

/// apply full：下载全包 → 对调 → marker 就位 + `.old` 可被启动清理回收。
async fn t11a_apply_full() -> Result<()> {
    let base = TempDir::new().context("临时目录创建失败")?;
    let install = prepare_install(base.path()).await?;
    let exe = fake_exe_bytes()?;
    let new_zip = build_zip(&[("rd-agent.exe", &exe), (MARKER_NAME, MARKER_NEW)])?;
    let release = make_release(FAKE_VERSION, &new_zip)?;
    let server = ReleaseServer::spawn(release.files());
    let http = http_client().await?;
    let settings = release.settings(server.addr, rd_agent::updater::UpdateMode::Full);
    let manifest = release.manifest.clone();

    let plan = rd_agent::updater::apply_into(&http, &settings, &manifest, &install).await?;
    anyhow::ensure!(
        plan.new_exe == install.join("rd-agent.exe"),
        "RestartPlan.new_exe 应指向安装目录 exe"
    );
    assert_marker_installed(&install)?;
    // 对调留下的 .old（zip 内 exe 与安装目录同名 → 恰 1 个）由下次启动
    // cleanup_old_files 回收（updater 单测语义的真实目录演练）
    let cleaned = rd_agent::updater::cleanup_old_files(&install);
    anyhow::ensure!(
        cleaned >= 1,
        "应清理 1 个 .old（rd-agent.exe），实际 {cleaned}"
    );
    anyhow::ensure!(
        !install.join("rd-agent.exe.old").exists(),
        "cleanup 后不应残留 .old"
    );
    anyhow::ensure!(
        !install.join(".rd-update-staging").exists(),
        "staging 目录应已删除"
    );
    Ok(())
}

/// apply chunked：本机旧 zip 同法切块 → 只拉缺块段（Range 字节数 < 全包）。
async fn t11a_apply_chunked() -> Result<()> {
    let base = TempDir::new().context("临时目录创建失败")?;
    let install = prepare_install(base.path()).await?;
    let exe = fake_exe_bytes()?;
    let new_zip = build_zip(&[("rd-agent.exe", &exe), (MARKER_NAME, MARKER_NEW)])?;
    let release = make_release(FAKE_VERSION, &new_zip)?;
    let server = ReleaseServer::spawn(release.files());
    let http = http_client().await?;
    let settings = release.settings(server.addr, rd_agent::updater::UpdateMode::Chunked);
    let manifest = release.manifest.clone();

    // 本机旧 zip：exe 首块改 1 字节（其余内容与新 zip 相同 → 大部分块共享）
    let mut old_exe = exe.clone();
    old_exe[4096] ^= 0xFF;
    let old_zip = build_zip(&[("rd-agent.exe", &old_exe), (MARKER_NAME, MARKER_OLD)])?;
    std::fs::write(install.join(LOCAL_ZIP_NAME), &old_zip)
        .with_context(|| format!("写本机旧 zip 失败: {}", install.display()))?;

    let plan = rd_agent::updater::apply_into(&http, &settings, &manifest, &install).await?;
    anyhow::ensure!(plan.version == FAKE_VERSION);
    let served = server.served_pack_bytes.load(Ordering::SeqCst);
    let total = server.pack_len;
    anyhow::ensure!(
        served > 0 && served < total,
        "chunked 应只拉缺块（拉了 {served}/{total} 字节）"
    );
    println!(
        "[t11a] chunked 缺块下载 {served}/{total} 字节（{:.1}%）",
        served as f64 / total as f64 * 100.0
    );
    assert_marker_installed(&install)?;
    Ok(())
}

/// 坏块注入：pack 改 1 字节 → 解块校验失败，下载前阶段即放弃（目录未动）。
async fn t11a_bad_chunk() -> Result<()> {
    let base = TempDir::new().context("临时目录创建失败")?;
    let install = prepare_install(base.path()).await?;
    let exe = fake_exe_bytes()?;
    let new_zip = build_zip(&[("rd-agent.exe", &exe), (MARKER_NAME, MARKER_NEW)])?;
    let release = make_release(FAKE_VERSION, &new_zip)?;
    let mut files = release.files();
    let pack = files.get_mut(PACK_NAME).context("发布目录缺 pack")?;
    let mid = pack.len() / 2;
    pack[mid] ^= 0xFF;
    let server = ReleaseServer::spawn(files);
    let http = http_client().await?;
    let settings = release.settings(server.addr, rd_agent::updater::UpdateMode::Full);
    let manifest = release.manifest.clone();

    let err = rd_agent::updater::apply_into(&http, &settings, &manifest, &install)
        .await
        .err()
        .context("坏块必须导致 apply 失败")?;
    let msg = format!("{err:#}");
    anyhow::ensure!(
        msg.contains("下载段解块校验失败"),
        "错误应含「下载段解块校验失败」: {msg}"
    );
    anyhow::ensure!(
        !install.join(".rd-update-staging").exists() && !install.join(MARKER_NAME).exists(),
        "坏块路径不得写安装目录（半途污染）"
    );
    Ok(())
}

/// 中断重试：暂存目录残留（上次中断）→ apply 先清理再重装，成功。
async fn t11a_staging_leftover() -> Result<()> {
    let base = TempDir::new().context("临时目录创建失败")?;
    let install = prepare_install(base.path()).await?;
    let exe = fake_exe_bytes()?;
    let new_zip = build_zip(&[("rd-agent.exe", &exe), (MARKER_NAME, MARKER_NEW)])?;
    let release = make_release(FAKE_VERSION, &new_zip)?;
    let server = ReleaseServer::spawn(release.files());
    let http = http_client().await?;
    let settings = release.settings(server.addr, rd_agent::updater::UpdateMode::Full);
    let manifest = release.manifest.clone();

    // 模拟上次更新中断的残留：暂存目录 + 半截 payload
    let staging = install.join(".rd-update-staging");
    std::fs::create_dir(&staging).context("创建残留暂存目录失败")?;
    std::fs::write(staging.join("payload.zip"), b"interrupted-download")?;
    std::fs::write(staging.join("junk.txt"), b"junk")?;

    rd_agent::updater::apply_into(&http, &settings, &manifest, &install).await?;
    assert_marker_installed(&install)?;
    anyhow::ensure!(
        !staging.exists(),
        "重装成功后暂存残留应被清理（目录已删或已空）"
    );
    Ok(())
}

/// 只读回退：icacls deny 写权限 → apply 在**下载前**失败（先验后动）。
async fn t11a_readonly_fallback() -> Result<()> {
    let base = TempDir::new().context("临时目录创建失败")?;
    let install = base.path().join("install");
    std::fs::create_dir(&install)?;
    std::fs::write(install.join("rd-agent.exe"), b"old-exe")?;
    let before = std::fs::read(install.join("rd-agent.exe"))?;

    let user = std::env::var("USERNAME").context("取当前用户名失败")?;
    let status = Command::new("icacls")
        .arg(&install)
        .args(["/deny", &format!("{user}:(W)")])
        .status()
        .context("运行 icacls 失败")?;
    anyhow::ensure!(status.success(), "icacls /deny 失败: {status}");

    // ensure_writable 探针必须拒绝（apply 未下载任何字节就放弃）。
    // 本地无发布端可连：即便探针失效也不会真下载，用不存在的端点即可。
    let settings = rd_agent::updater::UpdateSettings {
        endpoint: "http://127.0.0.1:1".to_string(), // 不存在的服务
        pubkey_hex: "aa".repeat(32),
        mode: rd_agent::updater::UpdateMode::Full,
    };
    let http = reqwest::Client::new();
    let manifest = rd_update_pack::Manifest {
        format_version: 1,
        version: FAKE_VERSION.into(),
        pack_file: PACK_NAME.into(),
        pack_sha256: "bb".repeat(32),
        pack_size: 1,
        chunker: rd_update_pack::ChunkerParams {
            min_size: 64,
            avg_size: 256,
            max_size: 1024,
        },
        chunks: vec![],
    };
    let err = rd_agent::updater::apply_into(&http, &settings, &manifest, &install)
        .await
        .err()
        .context("只读目录必须被 ensure_writable 拒绝")?;
    let msg = format!("{err:#}");
    anyhow::ensure!(
        msg.contains("安装目录不可写"),
        "错误应含「安装目录不可写」: {msg}"
    );

    // 恢复权限（TempDir 清理依赖可写）；失败仅提示——残留在 %TEMP% 无害
    let status = Command::new("icacls")
        .arg(&install)
        .args(["/remove:d", &user])
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        println!("[t11a] WARN: icacls /remove:d 失败（%TEMP% 残留一个临时目录）");
    }
    let after = std::fs::read(install.join("rd-agent.exe"))?;
    anyhow::ensure!(before == after, "只读路径不得改动原文件");
    Ok(())
}

// ---------------------------------------------------------------------------
// t11b / t11c e2e 全链（spawn 对调重启）
// ---------------------------------------------------------------------------

/// e2e 全链（§5.1 步骤 5-8 + §5.3 full 对照）：
/// Agent 子进程（`RD_UPDATE_TEST_AUTO=1` + `RD_UPDATE_TEST_CONFIRM=1`）自动走
/// 「检查 → 确认（直通）→ 下载 → 对调 → 确认重启（直通）→ spawn 新 exe →
/// exit(0)」；断言安装目录 marker 就位、新进程打印 RD_LISTEN（透传
/// `--print-port`）、目录无 `*.old` 残留（新进程启动清理）、新进程可连接；
/// `chunked=true` 时断言下载字节数显著小于全包（§5.1 步骤 6）。
async fn run_update_e2e(chunked: bool) -> Result<()> {
    let base = TempDir::new().context("临时目录创建失败")?;
    let install = prepare_install(base.path()).await?;

    // 假新版发布端：zip(exe 原字节 + marker) → pack/manifest/sig 三件
    let exe = fake_exe_bytes()?;
    let new_zip = build_zip(&[("rd-agent.exe", &exe), (MARKER_NAME, MARKER_NEW)])?;
    let release = make_release(FAKE_VERSION, &new_zip)?;
    let server = ReleaseServer::spawn(release.files());

    // chunked：安装目录放本机旧 zip（大部分块与新版共享）
    if chunked {
        let mut old_exe = exe.clone();
        old_exe[4096] ^= 0xFF;
        let old_zip = build_zip(&[("rd-agent.exe", &old_exe), (MARKER_NAME, MARKER_OLD)])?;
        std::fs::write(install.join(LOCAL_ZIP_NAME), &old_zip).context("写旧 zip 失败")?;
    }

    // config-dir + Pro 授权 + 清理兜底
    let cfg = base.path().join("cfg");
    std::fs::create_dir(&cfg).context("创建 config-dir 失败")?;
    common::write_pro_license(&cfg)?;
    let _cleanup = common::AgentCleanup(cfg.clone());

    // 旧 Agent（从安装目录的拷贝 exe 运行——install_dir 即安装目录）；
    // NO_PROXY 覆盖 127.0.0.1：reqwest 默认吃系统代理，本机发布端不能绕代理
    let endpoint = format!("http://{}", server.addr);
    let envs = [
        ("RD_UPDATE_ENDPOINT", endpoint.as_str()),
        ("RD_UPDATE_SIGN_PUBKEY", release.pubkey_hex.as_str()),
        ("RD_UPDATE_MODE", if chunked { "chunked" } else { "full" }),
        ("RD_UPDATE_TEST_CONFIRM", "1"),
        ("RD_UPDATE_TEST_AUTO", "1"),
        ("NO_PROXY", "127.0.0.1"),
        ("no_proxy", "127.0.0.1"),
    ];
    let (mut agent, _addr1, listen_rx) = common::spawn_agent_custom(
        &install.join("rd-agent.exe"),
        &cfg,
        &envs,
        &["--no-tray", "--listen", "127.0.0.1:0", "--print-port"],
    )?;
    println!(
        "[t11{}] 旧 Agent 就绪（pid {}，endpoint {endpoint}）",
        if chunked { "c" } else { "b" },
        agent.child.id()
    );

    // 更新链自动触发（启动后 1.5s）→ 对调完成 = marker 出现
    let marker = install.join(MARKER_NAME);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if marker.is_file() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            common::dump_logs_in(&cfg);
            anyhow::bail!("60s 内 marker 未出现（更新链未完成对调）");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let marker_body = std::fs::read(&marker).context("读 marker 失败")?;
    anyhow::ensure!(
        marker_body == MARKER_NEW,
        "marker 内容应为新版，实际 {marker_body:?}"
    );

    // 旧进程 exit(0) → restart_now spawn 新 exe（继承 stdout 管道）→ 第二条
    // RD_LISTEN（`--print-port` 经 RestartPlan.args 透传）
    let line2 = listen_rx
        .recv_timeout(Duration::from_secs(30))
        .context("更新重启后 30s 内未收到新进程的 RD_LISTEN=（spawn 对调重启失败？）")?;
    let addr2: SocketAddr = line2
        .parse()
        .with_context(|| format!("新进程监听地址行解析失败: {line2:?}"))?;
    println!("[t11] 新进程就绪（{addr2}）");

    // 无 `*.old` 残留（新进程启动 cleanup_old_files；打印 RD_LISTEN 前已完成）
    let olds: Vec<String> = std::fs::read_dir(&install)?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".old"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    anyhow::ensure!(
        olds.is_empty(),
        "新进程启动后仍残留 .old（清理未执行）: {olds:?}"
    );

    // 新进程可连接（信任列表同 config-dir；首连自适应配对）
    let c = Client::connect_to(addr2).await?;
    anyhow::ensure!(
        !c.hello_ack.host_version.is_empty(),
        "新进程 HelloAck.host_version 为空（服务异常）"
    );
    println!(
        "[t11] 新进程 HelloAck ok（agent 版本 {}）",
        c.hello_ack.host_version
    );
    c.close().await;

    // 模式断言（服务端统计）：full = 全包一次；chunked = 只拉缺块
    let served = server.served_pack_bytes.load(Ordering::SeqCst);
    let total = server.pack_len;
    if chunked {
        anyhow::ensure!(
            served > 0 && served < total,
            "chunked 下载应小于全包: {served}/{total}"
        );
        println!(
            "t11c ok: chunked 全链（下载 {served}/{total} 字节，{:.1}%）",
            served as f64 / total as f64 * 100.0
        );
    } else {
        anyhow::ensure!(
            server.pack_requests.load(Ordering::SeqCst) == 1 && served == total,
            "full 应一次拉全包: {served}/{total}（{} 次请求）",
            server.pack_requests.load(Ordering::SeqCst)
        );
        println!("t11b ok: full 全链（全包 {total} 字节一次拉取）");
    }

    // 旧 Child 已 exit(0)；新进程由 Agent spawn（非测试进程 Child），Drop 兜底杀
    let _ = agent.child.wait();
    Ok(())
}

/// §5.1/5.3：full 模式（灰度起点）e2e 全链。
#[tokio::test(flavor = "multi_thread")]
async fn t11b_update_e2e_full() -> Result<()> {
    run_update_e2e(false).await
}

/// §5.1：chunked 缺块差分 e2e 全链（本机旧 zip → 只拉缺块段）。
#[tokio::test(flavor = "multi_thread")]
async fn t11c_update_e2e_chunked() -> Result<()> {
    run_update_e2e(true).await
}

// ---------------------------------------------------------------------------
// 假新版构造辅助
// ---------------------------------------------------------------------------

/// 假「新版」exe 内容：真 debug exe 字节（新 zip 里的 exe 必须能跑——对调后
/// restart_now spawn 的就是它）。测试进程内缓存（多个场景共用，~几十 MB）。
fn fake_exe_bytes() -> Result<Vec<u8>> {
    static EXE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let bytes = EXE.get_or_init(|| {
        std::fs::read(env!("CARGO_BIN_EXE_rd-agent"))
            .unwrap_or_else(|err| panic!("读 CARGO_BIN_EXE_rd-agent 失败: {err}"))
    });
    Ok(bytes.clone())
}

/// 安装目录：真 exe + ffmpeg DLL（本地有、git 无；缺失不阻断——encode caps
/// 探测会降级，不影响更新链路）。
async fn prepare_install(base: &Path) -> Result<PathBuf> {
    let install = base.join("install");
    std::fs::create_dir(&install).context("创建安装目录失败")?;
    std::fs::copy(env!("CARGO_BIN_EXE_rd-agent"), install.join("rd-agent.exe"))
        .context("拷贝 rd-agent.exe 到安装目录失败")?;
    let ffmpeg_bin =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/rd-controller/src-tauri/ffmpeg-bin");
    if ffmpeg_bin.is_dir() {
        for entry in std::fs::read_dir(&ffmpeg_bin)?.flatten() {
            let name = entry.file_name();
            if name
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".dll")
            {
                let _ = std::fs::copy(entry.path(), install.join(&name));
            }
        }
    } else {
        println!(
            "[t11] ffmpeg-bin 不存在（{}），安装目录不含 DLL",
            ffmpeg_bin.display()
        );
    }
    Ok(install)
}

/// 断言对调后安装目录状态：marker 就位 + exe/.old 结构符合预期。
fn assert_marker_installed(install: &Path) -> Result<()> {
    let body = std::fs::read(install.join(MARKER_NAME))
        .with_context(|| format!("marker 未就位: {}", install.join(MARKER_NAME).display()))?;
    anyhow::ensure!(body == MARKER_NEW, "marker 内容应为新版，实际 {body:?}");
    Ok(())
}

/// 固定时间戳打 zip（确定性）：新旧 zip 的同内容条目字节一致——chunked 缺块
/// 对比「大部分块共享」的前提（时间戳漂移会把整包变成全缺块）。条目用
/// Stored（不压缩）：测试 zip 无需压缩（pack 的 zstd 已按块压缩），且
/// Deflated 对几十 MB 的 debug exe 纯 Rust 压缩实现慢到拖垮用例。
fn build_zip(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let buf = std::io::Cursor::new(Vec::new());
    let mut w = zip::ZipWriter::new(buf);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(
            zip::DateTime::from_date_and_time(2026, 1, 1, 0, 0, 0)
                .map_err(|e| anyhow::anyhow!("zip 时间戳构造失败: {e:?}"))?,
        );
    for (name, data) in entries {
        w.start_file(*name, opts)
            .with_context(|| format!("zip 条目 {name} 开始失败"))?;
        w.write_all(data)
            .with_context(|| format!("zip 条目 {name} 写入失败"))?;
    }
    let file = w.finish().context("zip 收尾失败")?;
    Ok(file.into_inner())
}

/// 一次假发布：密钥对 + Manifest::make + 签名。seed 只存活在本结构内
/// （对齐手测清单「seed 只放 %TEMP%，别提交」的纪律——测试进程内存即弃，
/// 绝不写入安装目录或发布目录）。
struct FakeRelease {
    manifest: Manifest,
    manifest_bytes: Vec<u8>,
    pack: Vec<u8>,
    sig_bytes: String,
    pubkey_hex: String,
}

impl FakeRelease {
    /// 发布目录三件套：`update.json` / `update.json.sig` / pack。
    fn files(&self) -> HashMap<String, Vec<u8>> {
        HashMap::from([
            ("update.json".to_string(), self.manifest_bytes.clone()),
            (
                "update.json.sig".to_string(),
                self.sig_bytes.clone().into_bytes(),
            ),
            (self.manifest.pack_file.clone(), self.pack.clone()),
        ])
    }

    /// Agent 侧更新配置（端点指向本地发布端，公钥用本次发布的测试钥）。
    fn settings(
        &self,
        addr: SocketAddr,
        mode: rd_agent::updater::UpdateMode,
    ) -> rd_agent::updater::UpdateSettings {
        rd_agent::updater::UpdateSettings {
            endpoint: format!("http://{addr}"),
            pubkey_hex: self.pubkey_hex.clone(),
            mode,
        }
    }
}

fn make_release(version: &str, new_zip: &[u8]) -> Result<FakeRelease> {
    let (seed_hex, pubkey_hex) = generate_keypair();
    let (manifest, pack) = Manifest::make(version, PACK_NAME, new_zip, &Chunker::with_defaults());
    let manifest_bytes = serde_json::to_vec(&manifest).context("清单序列化失败")?;
    let sig_bytes = sign_bytes(&seed_hex, &manifest_bytes).context("清单签名失败")?;
    Ok(FakeRelease {
        manifest,
        manifest_bytes,
        pack,
        sig_bytes,
        pubkey_hex,
    })
}

/// 本地最小发布端：GET + Range（206）静态文件服务 + pack 下载字节统计。
///
/// 手写 HTTP/1.1（零新增依赖）：每请求读头、按路径取文件、带 Range 时回
/// 206 + Content-Range（Agent 的缺块下载只发 `bytes=start-end` 闭区间），
/// 响应带 `Connection: close`。accept 线程随测试进程退出回收。
struct ReleaseServer {
    addr: SocketAddr,
    /// pack 的 Range 下载累计字节数（206 响应体字节；chunked/full 断言用）。
    served_pack_bytes: Arc<AtomicU64>,
    /// pack 的 Range 请求数（full 应为 1）。
    pack_requests: Arc<AtomicUsize>,
    /// pack 总长（断言基准）。
    pack_len: u64,
}

impl ReleaseServer {
    fn spawn(files: HashMap<String, Vec<u8>>) -> Self {
        let pack_len = files
            .get(PACK_NAME)
            .map(|p| p.len() as u64)
            .unwrap_or_default();
        let listener = TcpListener::bind("127.0.0.1:0").expect("发布端 bind 127.0.0.1:0 失败");
        let addr = listener.local_addr().expect("取发布端地址失败");
        let files = Arc::new(files);
        let served = Arc::new(AtomicU64::new(0));
        let reqs = Arc::new(AtomicUsize::new(0));
        // 统计原件留在 ReleaseServer 断言用；accept 线程持有克隆
        let (t_served, t_reqs, t_files) =
            (Arc::clone(&served), Arc::clone(&reqs), Arc::clone(&files));
        std::thread::Builder::new()
            .name("rd-e2e-release".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let files = Arc::clone(&t_files);
                    let served = Arc::clone(&t_served);
                    let reqs = Arc::clone(&t_reqs);
                    std::thread::spawn(move || {
                        handle_release_conn(stream, &files, &served, &reqs);
                    });
                }
            })
            .expect("发布端 accept 线程启动失败");
        ReleaseServer {
            addr,
            served_pack_bytes: served,
            pack_requests: reqs,
            pack_len,
        }
    }
}

/// 处理一个连接：读请求行 + 头（只认 Range），按路径回 200/206/404。
fn handle_release_conn(
    stream: TcpStream,
    files: &HashMap<String, Vec<u8>>,
    served: &AtomicU64,
    pack_requests: &AtomicUsize,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut write_half = write_half;
    let mut reader = BufReader::new(stream);

    let mut req_line = String::new();
    match reader.read_line(&mut req_line) {
        Ok(n) if n > 0 => {}
        _ => return,
    }
    let mut parts = req_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").to_string();

    // 头部读到空行为止，只关心 Range（Agent 只发 bytes=start-end 闭区间）
    let mut range: Option<(u64, u64)> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let t = line.trim();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t.to_ascii_lowercase().strip_prefix("range:") {
            if let Some(spec) = v.trim().strip_prefix("bytes=") {
                if let Some((s, e)) = spec.split_once('-') {
                    if let (Ok(s), Ok(e)) = (s.trim().parse::<u64>(), e.trim().parse::<u64>()) {
                        range = Some((s, e));
                    }
                }
            }
        }
    }

    let key = path.trim_start_matches('/');
    let Some(content) = files.get(key) else {
        let _ = respond(&mut write_half, "404 Not Found", None, b"not found");
        return;
    };
    let total = content.len() as u64;
    match range {
        None => {
            let _ = respond(&mut write_half, "200 OK", None, content);
        }
        Some((s, e)) => {
            if s >= total {
                let _ = respond(&mut write_half, "416 Range Not Satisfiable", None, b"");
                return;
            }
            let e = e.min(total - 1);
            let n = (e - s + 1) as usize;
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {n}\r\nContent-Range: bytes {s}-{e}/{total}\r\nConnection: close\r\n\r\n"
            );
            if write_half.write_all(head.as_bytes()).is_ok()
                && write_half.write_all(&content[s as usize..][..n]).is_ok()
                && write_half.flush().is_ok()
            {
                served.fetch_add(n as u64, Ordering::SeqCst);
                pack_requests.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// 写一个完整响应（可选额外头 + body）。
fn respond(
    stream: &mut TcpStream,
    status: &str,
    extra: Option<&str>,
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(e) = extra {
        head.push_str(e);
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
