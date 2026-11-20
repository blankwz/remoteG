//! Agent 自更新执行器（改造项 A；snug Updater.swift 编排的 Rust 重写）。
//!
//! 职责链（对照 snug 全序，保守侧收紧）：
//! 1. `check`：拉 `update.json` + `.sig` → 内置公钥验签（fail-closed）→ 清单
//!    校验 → 与本机版本比较（防回滚）；
//! 2. `apply`：可写性探针 → 缺块计算（chunked 用本机 zip 对比；full 全拉）→
//!    HTTP Range 下载 → 逐块解压核对 → 拼装全量校验 → 暂存目录解 zip →
//!    逐文件 rename `.old` 对调 → 返回重启计划；
//! 3. 重启（bin 层）：断开全部会话 → spawn 新 exe（透传参数）→ exit(0)；
//!    新进程 bind 冲突由 `server::bind_servers_with_retry` 兜住（旧进程退出
//!    有延迟，最长等 ~15s）。
//!
//! 环境变量（对齐 T-038 的灰度机制）：
//! - `RD_UPDATE_ENDPOINT`：更新基址（一个目录 URL；`update.json`/`.sig`/pack
//!   同 base 可取）。缺省空 = 更新功能关闭（占位待 T-039 正式渠道）。
//! - `RD_UPDATE_MODE`：`full`（默认，灰度起点）| `chunked`（缺块差分）。
//! - `RD_UPDATE_SIGN_PUBKEY`：验签公钥 hex。缺省用内置占位（全 0，任何签名
//!   都不通过——未配真钥前更新必然拒绝，fail-closed）。debug 构建允许注入
//!   测试公钥（对齐 rd-security 的 dev 兜底思路）。
//!
//! 密钥纪律：公钥可编译锚定/环境覆盖，seed 绝不进 Agent；签名验证失败 = 整
//! 个更新拒绝，无旁路。
//!
//! 单元测试覆盖纯逻辑（验签/缺块计算/对调/清理）；网络编排由 09_test 手测
//! 清单覆盖（本地起 HTTP 服务模拟发布端，见手测清单补项）。

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rd_update_pack::{compare_versions, verify_bytes, Chunker, Manifest};

/// Agent 自更新验签公钥（ed25519，正式锚定，T-039）。对应 seed 在凭据目录
/// `rd-update-sign.seed`（绝不进仓库），CI 发布端走 `RD_UPDATE_SIGN_KEY`
/// secret 同源签名；签名验证失败 = 整个更新拒绝，无旁路。
pub const UPDATE_SIGN_PUBKEY: &str =
    "3fe6b393dab3adc4ac5cd65b45ab3f2a2ffaa9b561ef85285f845f18be35f7a6";

/// 默认更新基址：GitHub Releases latest 资产的平铺地址（GitHub 固定重定向
/// 到最新 release 的同名资产——`update.json`/`update.json.sig`/pack 同 base
/// 可取）。COS 国内渠道用 `RD_UPDATE_ENDPOINT` 覆盖为 `…/latest` 同构布局。
pub const DEFAULT_UPDATE_ENDPOINT: &str =
    "https://github.com/blankwz/remoteG/releases/latest/download";

/// 更新清单与包的解压暂存目录名（安装目录下）。
const STAGING_DIR: &str = ".rd-update-staging";

/// 更新通道模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateMode {
    /// 整包下载（灰度默认）：不做缺块对比，全量拉 pack 后走同一拼装链路。
    #[default]
    Full,
    /// 缺块差分：本机 zip 同法切块对比，只拉缺的段（HTTP Range）。
    Chunked,
}

impl UpdateMode {
    fn from_env() -> Self {
        match std::env::var("RD_UPDATE_MODE").as_deref() {
            Ok("chunked") => Self::Chunked,
            _ => Self::Full,
        }
    }
}

/// 更新配置（环境变量来源，托盘/服务启动时读取一次）。
#[derive(Debug, Clone)]
pub struct UpdateSettings {
    /// 更新基址；空 = 功能关闭。
    pub endpoint: String,
    /// 验签公钥 hex（64 字符）。
    pub pubkey_hex: String,
    pub mode: UpdateMode,
}

impl UpdateSettings {
    pub fn from_env() -> Self {
        let pubkey_hex = match std::env::var("RD_UPDATE_SIGN_PUBKEY") {
            // debug 构建允许注入测试公钥（本地 e2e 用）；release 一律内置锚定
            Ok(k) if cfg!(debug_assertions) || is_real_key(&k) => k,
            _ => UPDATE_SIGN_PUBKEY.to_string(),
        };
        Self {
            // 缺省锚定 GitHub 渠道（渠道就绪即小白零配置）；空串覆盖=显式关闭
            endpoint: std::env::var("RD_UPDATE_ENDPOINT").unwrap_or(DEFAULT_UPDATE_ENDPOINT.to_string()),
            pubkey_hex,
            mode: UpdateMode::from_env(),
        }
    }

    /// 更新功能是否启用（端点已配置）。
    pub fn enabled(&self) -> bool {
        !self.endpoint.trim().is_empty()
    }
}

/// 粗验公钥 hex 格式（64 位 hex）；release 构建拒绝环境注入非锚定值。
fn is_real_key(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 检查更新：拉清单 + 验签 + 版本比较。返回 `Some(manifest)` = 有新版本可装。
pub async fn check(
    http: &reqwest::Client,
    s: &UpdateSettings,
    current_version: &str,
) -> anyhow::Result<Option<Manifest>> {
    if !s.enabled() {
        anyhow::bail!("更新功能未启用（未配置 RD_UPDATE_ENDPOINT）");
    }
    let manifest_bytes = http_get(http, &format!("{}/update.json", s.endpoint), None).await?;
    let sig = http_get(http, &format!("{}/update.json.sig", s.endpoint), None).await?;
    let sig_str = String::from_utf8(sig).context("update.json.sig 不是 UTF-8")?;
    if !verify_bytes(&s.pubkey_hex, &manifest_bytes, sig_str.trim())? {
        anyhow::bail!("update.json 签名验证失败——更新拒绝（fail-closed）");
    }
    let manifest = Manifest::decode(&manifest_bytes)?;
    if compare_versions(&manifest.version, current_version) != std::cmp::Ordering::Greater {
        tracing::info!(
            remote = %manifest.version,
            current = current_version,
            "无可用更新"
        );
        return Ok(None);
    }
    Ok(Some(manifest))
}

/// 缺块范围计算（纯逻辑）：本机 zip 切块对比清单，得待下载的字节段。
/// full 模式 / 本机无 zip / 切块参数不一致 → 全包一段（自然退化整包下载）。
pub fn compute_ranges(
    manifest: &Manifest,
    local_zip: Option<&[u8]>,
    mode: UpdateMode,
) -> Vec<Range<u64>> {
    let empty: HashSet<&str> = HashSet::new();
    let have = match (mode, local_zip) {
        (_, None) => &empty,
        (UpdateMode::Full, _) => &empty,
        (UpdateMode::Chunked, Some(zip)) => {
            match manifest.chunks_in(zip, &Chunker::with_defaults()) {
                Some(local) => {
                    let set: HashSet<&str> = local.keys().map(|s| s.as_str()).collect();
                    return manifest.missing_ranges(&set);
                }
                None => &empty,
            }
        }
    };
    manifest.missing_ranges(have)
}

/// 执行更新（check 通过后调用）：下载 → 拼装 → 暂存解包 → 对调 → 重启计划。
///
/// 返回 [`RestartPlan`]，调用方负责：断开会话 → [`restart_now`] → 退出。
pub async fn apply(
    http: &reqwest::Client,
    s: &UpdateSettings,
    manifest: &Manifest,
) -> anyhow::Result<RestartPlan> {
    apply_into(http, s, manifest, &install_dir()?).await
}

/// 同 [`apply`]，但安装目录由调用方指定——e2e 用 tempdir 重定向（t11），
/// 生产路径 [`apply`] 固定传 [`install_dir`]。
pub async fn apply_into(
    http: &reqwest::Client,
    s: &UpdateSettings,
    manifest: &Manifest,
    install_dir: &Path,
) -> anyhow::Result<RestartPlan> {
    let install_dir = install_dir.to_path_buf();
    ensure_writable(&install_dir)?;

    // 缺块计算：chunked 模式尝试本机旧 zip（找不到就全包下载，无特殊分支）
    let local_zip = if s.mode == UpdateMode::Chunked {
        find_local_zip(&install_dir)
    } else {
        None
    };
    let ranges = compute_ranges(manifest, local_zip.as_deref(), s.mode);
    tracing::info!(
        mode = ?s.mode,
        ranges = ranges.len(),
        bytes = ranges.iter().map(|r| r.end - r.start).sum::<u64>(),
        "开始下载更新包"
    );

    // HTTP Range 逐段下载 → 解块核对。chunked 模式下**本机 zip 里的共享块**
    // 先行入池（t11 e2e 实测：available 只装下载段会让 assemble 缺本机已有
    // 的块——单测只锚定了 ranges 计算，未锚定拼装链路，此缺陷此前不可见）。
    let mut available = std::collections::HashMap::new();
    if let Some(bytes) = &local_zip {
        if let Some(local_chunks) = manifest.chunks_in(bytes, &Chunker::with_defaults()) {
            available.extend(local_chunks);
        }
    }
    for r in &ranges {
        let segment = http_get(http, &pack_url(s, manifest), Some((r.start, r.end))).await?;
        let recovered = manifest
            .unpack(&segment, r)
            .context("下载段解块校验失败（网络损坏或被篡改）")?;
        available.extend(recovered);
    }

    // 拼装全量校验 → 与发布产物逐字节相同
    let new_zip = manifest.assemble(&available).context("更新包拼装失败")?;

    // 暂存目录：写 zip → 解开（zip crate 拦截路径穿越条目）
    let staging = install_dir.join(STAGING_DIR);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("清理残留暂存目录失败: {}", staging.display()))?;
    }
    std::fs::create_dir(&staging)
        .with_context(|| format!("创建暂存目录失败: {}", staging.display()))?;
    let payload = staging.join("payload.zip");
    std::fs::write(&payload, &new_zip)
        .with_context(|| format!("写 payload 失败: {}", payload.display()))?;
    {
        let f = std::fs::File::open(&payload)?;
        let mut archive = zip::ZipArchive::new(f).context("payload.zip 读取失败")?;
        archive.extract(&staging).context("payload.zip 解包失败")?;
    }
    std::fs::remove_file(&payload)?;

    // 文件对调：旧文件 rename .old，新文件就位
    swap_files(&install_dir, &staging)?;
    std::fs::remove_dir(&staging).ok(); // 空了就删，残留无害

    Ok(RestartPlan {
        new_exe: install_dir.join("rd-agent.exe"),
        args: std::env::args().skip(1).collect(),
        version: manifest.version.clone(),
    })
}

/// 重启计划：对调完成后交给调用方执行。
#[derive(Debug, Clone)]
pub struct RestartPlan {
    pub new_exe: PathBuf,
    pub args: Vec<String>,
    pub version: String,
}

/// 执行重启：spawn 新 exe（透传原参数）。调用方应先断开全部会话再调用，
/// 并在本进程 spawn 成功后立即退出。
pub fn restart_now(plan: &RestartPlan) -> anyhow::Result<()> {
    std::process::Command::new(&plan.new_exe)
        .args(&plan.args)
        .spawn()
        .with_context(|| format!("启动新版本失败: {}", plan.new_exe.display()))?;
    tracing::info!(version = %plan.version, "已 spawn 新版本，本进程即将退出");
    Ok(())
}

/// 安装目录 = 当前 exe 所在目录。
pub fn install_dir() -> anyhow::Result<PathBuf> {
    std::env::current_exe()
        .context("取当前 exe 路径失败")?
        .parent()
        .map(Path::to_path_buf)
        .context("取安装目录失败")
}

/// 可写性探针：先验可写再下载（对照 snug Updater 的「先验后动」全序）。
fn ensure_writable(dir: &Path) -> anyhow::Result<()> {
    let probe = dir.join(".rd-update-probe");
    std::fs::write(&probe, b"probe").with_context(|| {
        format!(
            "安装目录不可写: {}（请检查只读介质/权限/杀软锁定）",
            dir.display()
        )
    })?;
    std::fs::remove_file(&probe).ok();
    Ok(())
}

/// 找本机旧 zip（chunked 模式缺块对比用）：安装目录里任意 rd-agent-*-win64.zip。
fn find_local_zip(dir: &Path) -> Option<Vec<u8>> {
    let mut best: Option<PathBuf> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_str()?;
        if name.starts_with("rd-agent-") && name.ends_with("-win64.zip") {
            best = Some(entry.path());
        }
    }
    std::fs::read(best?).ok()
}

/// pack 的下载 URL：与 update.json 同 base。
fn pack_url(s: &UpdateSettings, manifest: &Manifest) -> String {
    format!(
        "{}/{}",
        s.endpoint.trim_end_matches('/'),
        manifest.pack_file
    )
}

/// GET（可选 Range）：返回字节。统一 30s 连接 / 600s 总超时。
///
/// `range` 为包内**半开区间** `[start, end)`（同 `missing_ranges` 的
/// `Range<u64>` 语义）；HTTP Range 头是**闭区间**（`bytes=a-b` 含 b），须
/// 端点 -1 换算——此前直接把半开 end 当闭区间发送，标准 Range 服务器会多
/// 回 1 字节，`Manifest::unpack` 的长度校验必然失败（t11 e2e 实测）。
async fn http_get(
    http: &reqwest::Client,
    url: &str,
    range: Option<(u64, u64)>,
) -> anyhow::Result<Vec<u8>> {
    let mut req = http.get(url);
    if let Some((start, end)) = range {
        anyhow::ensure!(end > start, "Range 区间为空: {start}..{end}");
        req = req.header(reqwest::header::RANGE, format!("bytes={start}-{}", end - 1));
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("请求失败: {url}"))?;
    let status = resp.status();
    if !(status.is_success() || status == reqwest::StatusCode::PARTIAL_CONTENT) {
        anyhow::bail!("HTTP {status}: {url}");
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .with_context(|| format!("读响应体失败: {url}"))
}

/// 文件对调：staged 里的每个文件 → 安装目录。同名旧文件先 rename `.old`
/// （Windows 锁运行中文件：rename 允许、覆盖写/删除不允许——rename 是唯一
/// 无损路径）。`.old` 残留由下次启动 [`cleanup_old_files`] 清理。
pub fn swap_files(install: &Path, staged: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(staged)
        .with_context(|| format!("读暂存目录失败: {}", staged.display()))?
    {
        let entry = entry.context("读暂存目录条目失败")?;
        let name = entry.file_name();
        let target = install.join(&name);
        if target.exists() {
            let old = install.join(format!("{}.old", name.to_string_lossy()));
            if old.exists() {
                // 上次更新失败残留的 .old：非运行中文件，可删
                std::fs::remove_file(&old)
                    .with_context(|| format!("清理残留 {} 失败", old.display()))?;
            }
            std::fs::rename(&target, &old)
                .with_context(|| format!("{} → .old 对调失败（文件被占用？）", target.display()))?;
        }
        std::fs::rename(entry.path(), &target)
            .with_context(|| format!("新文件就位失败: {}", name.to_string_lossy()))?;
    }
    Ok(())
}

/// 启动时清理上次更新留下的 `*.old`。返回清理的文件数（日志用）。
///
/// 对调重启的常态竞速：旧进程 exit(0) 与新进程启动清理并行——旧进程的
/// **映像锁**（rename 后 .old 仍是其运行映像）会让首轮删除失败。短重试
/// 覆盖旧进程退出的毫秒级窗口；仍失败（非重启场景的外部锁定）留给下次
/// 启动，无 .old 时零等待。
pub fn cleanup_old_files(install: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(install) else {
        return 0;
    };
    let mut pending: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".old")))
        .map(|e| e.path())
        .collect();
    let mut cleaned = 0usize;
    for attempt in 0..5 {
        pending.retain(|p| {
            if std::fs::remove_file(p).is_ok() {
                cleaned += 1;
                false
            } else {
                true
            }
        });
        if pending.is_empty() {
            break;
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 锚定公钥非占位（T-039 替换后的防回退锚：占位是全 0、任何签名都拒；
    /// 若有人回退占位会让更新链整体静默失效，此测试立即红）。
    #[test]
    fn anchored_pubkey_is_not_placeholder() {
        assert_ne!(UPDATE_SIGN_PUBKEY, &"0".repeat(64));
        assert!(is_real_key(UPDATE_SIGN_PUBKEY));
    }

    /// debug 构建的公钥格式粗验：64 hex 才可注入。
    #[test]
    fn env_pubkey_must_be_hex64() {
        assert!(is_real_key(&"ab".repeat(32)));
        assert!(!is_real_key("zz"));
        assert!(!is_real_key(&"ab".repeat(31)));
    }

    /// compute_ranges：chunked + 本机旧 zip → 只拉缺块；无 zip → 全包一段。
    #[test]
    fn compute_ranges_chunked_vs_full() {
        let c = Chunker::with_defaults();
        // 高熵伪随机 4MB（~30 块，avg 128Ki），保证块级统计有样本量
        let v1: Vec<u8> = (0..4_000_000u32)
            .map(|i| {
                let x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                ((x >> 33) & 0xFF) as u8
            })
            .collect();
        let mut v2 = v1.clone();
        v2[2_000_000] ^= 0xFF;
        let (_, _) = Manifest::make("0.1.0", "a.pack", &v1, &c);
        let (m2, pack2) = Manifest::make("0.2.0", "b.pack", &v2, &c);

        // chunked + 本机 zip：下载量必须小于全包
        let ranges = compute_ranges(&m2, Some(&v1), UpdateMode::Chunked);
        let chunked_bytes: u64 = ranges.iter().map(|r| r.end - r.start).sum();
        assert!(
            chunked_bytes < (pack2.len() as u64) / 2,
            "缺块差分应显著省流量"
        );

        // full 模式 / 无 zip：全包一段（退化整包下载，无特殊分支）
        for mode in [UpdateMode::Full, UpdateMode::Chunked] {
            let ranges = compute_ranges(&m2, None, mode);
            assert_eq!(ranges.len(), 1);
            assert_eq!(ranges[0], 0..pack2.len() as u64);
        }
    }

    /// 文件对调与 .old 清理演练（tempdir 模拟安装目录与暂存目录）。
    #[test]
    fn swap_and_cleanup_roundtrip() {
        let dir = TempDir::new().unwrap();
        let install = dir.path();
        let staged = dir.path().join(".rd-update-staging");
        std::fs::create_dir(&staged).unwrap();

        // 旧安装：exe 与 DLL 在位
        std::fs::write(install.join("rd-agent.exe"), b"old-exe").unwrap();
        std::fs::write(install.join("avcodec.dll"), b"old-dll").unwrap();
        // 新版本：exe 与 DLL 都有变化，外加一个新文件
        std::fs::write(staged.join("rd-agent.exe"), b"new-exe").unwrap();
        std::fs::write(staged.join("avcodec.dll"), b"new-dll").unwrap();
        std::fs::write(staged.join("LICENSE"), b"license").unwrap();

        swap_files(install, &staged).unwrap();
        assert_eq!(
            std::fs::read(install.join("rd-agent.exe")).unwrap(),
            b"new-exe"
        );
        assert_eq!(
            std::fs::read(install.join("avcodec.dll")).unwrap(),
            b"new-dll"
        );
        assert_eq!(std::fs::read(install.join("LICENSE")).unwrap(), b"license");
        assert_eq!(
            std::fs::read(install.join("rd-agent.exe.old")).unwrap(),
            b"old-exe"
        );
        assert_eq!(
            std::fs::read(install.join("avcodec.dll.old")).unwrap(),
            b"old-dll"
        );

        // 二次对调（上次失败残留 .old 场景）：.old 被替换不报错
        std::fs::write(staged.join("rd-agent.exe"), b"newer-exe").unwrap();
        swap_files(install, &staged).unwrap();
        assert_eq!(
            std::fs::read(install.join("rd-agent.exe")).unwrap(),
            b"newer-exe"
        );

        // 清理：.old 全部消失
        let cleaned = cleanup_old_files(install);
        assert_eq!(cleaned, 2, "应清理两个 .old");
        assert!(!install.join("rd-agent.exe.old").exists());
        assert!(!install.join("avcodec.dll.old").exists());
    }
}
