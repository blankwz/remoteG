//! 文件传输（T-031，v0.2，proto §"文件传输"）：控制端 ↔ 被控端文件收发。
//!
//! 流向（与 proto 注释一致，全部 `req_id=0`，按 `xfer_id` 路由）：
//! - 推送（pull=false）：控制端 [`FileXferOffer`] → 顺序 [`FileXferChunk`]
//!   （64KiB，QUIC 可靠流保序）→ 本端写 `Downloads\rd接收\<消毒名>`（重名
//!   `" (n)"` 去重）→ eof 后回 [`FileXferAck`]；校验失败立即回 ok=false。
//! - 拉取（pull=true）：控制端 [`FileXferOffer`]`{remote_path}` → 本端后台
//!   任务顺序发 [`FileXferChunk`]（取消令牌可中止）→ 发完即止，无 Ack；
//!   控制端写失败时发 [`FileCancel`]。
//!
//! 安全与审计纪律：
//! - 接收目录固定 `Downloads\rd接收` + 文件名消毒（防穿越/非法字符/保留名）；
//! - 列目录与终端同级（会话本有任意文件能力），不设白名单，仅 tracing；
//! - 每次传输完成落审计（`file_send_done` / `file_recv_done`，只记名字与
//!   字节数，不记内容）。
//!
//! 已知限制：推送分片在分发循环内落盘，千兆局域网大文件传输期间控制流
//! 其他消息（输入等）排队延迟上升——与 RustDesk 行为一致，弱影响，v0.3
//! 再拆独立流。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::io::Write as _;

use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use rd_proto::v0::envelope;
use rd_proto::v0::{
    FileCancel, FileEntry, FileListAck, FileListReq, FileXferAck, FileXferChunk, FileXferOffer,
};

use crate::session::{error_msg, SessionCtx};

/// 协议错误码：文件类错误（列目录失败等；proto §文件传输）。
pub const ERR_FILE: u32 = 55;
/// 分片大小：64KiB（传输效率与内存占用折中；proto §FileXferChunk）。
pub const CHUNK_SIZE: usize = 64 * 1024;
/// 文件名长度上限（防超长路径写失败）。
const NAME_MAX: usize = 200;

/// 会话级传输状态：同时只允许一路「接收中」推送（写盘串行、目录不乱）；
/// 拉取可多路并行（各持取消令牌）。断开时 [`on_disconnect`] 全量清理。
///
/// `outgoing` 用 std 锁：[`OutgoingGuard`] 要在任务任意退出路径（含 panic
/// 展开）的 `Drop` 里同步摘除表项，不能用跨 await 的 tokio 锁。
#[derive(Default)]
pub struct FileState {
    incoming: Mutex<Option<IncomingXfer>>,
    outgoing: std::sync::Mutex<HashMap<u64, Arc<CancellationToken>>>,
}

/// 拉取任务的 RAII 表项守卫：drop（正常结束/取消/panic 展开）时必摘表项。
struct OutgoingGuard {
    ctx: Arc<SessionCtx>,
    xfer_id: u64,
}

impl Drop for OutgoingGuard {
    fn drop(&mut self) {
        self.ctx
            .files
            .outgoing
            .lock()
            .expect("outgoing 锁中毒")
            .remove(&self.xfer_id);
    }
}

struct IncomingXfer {
    xfer_id: u64,
    file: std::fs::File,
    path: PathBuf,
    /// 期望的下一分片序号（从 0 起；乱序在可靠流上不应发生，防御性检查）。
    next_seq: u32,
    received: u64,
}

// ---------------------------------------------------------------------------
// 分发入口（session.rs dispatch_loop 调用）
// ---------------------------------------------------------------------------

/// `FileListReq`：列目录。空 path = 主目录；相对路径拒绝（防歧义）。
pub async fn list(ctx: &Arc<SessionCtx>, req_id: u32, m: FileListReq) -> anyhow::Result<()> {
    let target = if m.path.trim().is_empty() {
        match home_dir() {
            Some(h) => h,
            None => {
                ctx.send(
                    envelope::Msg::Error(error_msg(ERR_FILE, "无法确定主目录")),
                    req_id,
                )
                .await?;
                return Ok(());
            }
        }
    } else {
        PathBuf::from(&m.path)
    };
    if !target.is_absolute() {
        ctx.send(
            envelope::Msg::Error(error_msg(ERR_FILE, "请使用绝对路径（如 C:\\Users\\…）")),
            req_id,
        )
        .await?;
        return Ok(());
    }
    let entries = match list_dir(&target) {
        Ok(e) => e,
        Err(err) => {
            tracing::debug!(name = %ctx.peer_name, path = %target.display(), "列目录失败: {err:#}");
            ctx.send(
                envelope::Msg::Error(error_msg(ERR_FILE, &format!("列目录失败：{err:#}"))),
                req_id,
            )
            .await?;
            return Ok(());
        }
    };
    tracing::debug!(name = %ctx.peer_name, path = %target.display(), n = entries.len(), "列目录");
    ctx.send(
        envelope::Msg::FileListAck(FileListAck {
            entries,
            path: target.display().to_string(),
        }),
        req_id,
    )
    .await
}

/// `FileXferOffer`：推送建档 / 拉取起任务。
pub async fn on_offer(ctx: &Arc<SessionCtx>, m: FileXferOffer) -> anyhow::Result<()> {
    if m.pull {
        start_pull(ctx, m).await
    } else {
        start_push_recv(ctx, m).await
    }
}

/// `FileXferChunk`：推送分片落盘（仅当有匹配的接收中传输时）。
pub async fn on_chunk(ctx: &Arc<SessionCtx>, m: FileXferChunk) -> anyhow::Result<()> {
    let mut guard = ctx.files.incoming.lock().await;
    let Some(inc) = guard.as_mut() else {
        return Ok(()); // 无接收中传输（取消后残留分片）：静默丢弃
    };
    if inc.xfer_id != m.xfer_id {
        return Ok(()); // 其他传输的迟到分片：静默丢弃
    }
    if m.seq != inc.next_seq {
        let reason = format!("分片乱序（期望 {}，收到 {}）", inc.next_seq, m.seq);
        drop(guard);
        abort_incoming(ctx, m.xfer_id).await;
        send_ack(ctx, m.xfer_id, false, &reason).await?;
        return Ok(());
    }
    inc.next_seq += 1;
    inc.received += m.data.len() as u64;
    let write_result = inc.file.write_all(&m.data);
    if let Err(err) = write_result {
        let reason = format!("写入被控端磁盘失败：{err}");
        drop(guard);
        abort_incoming(ctx, m.xfer_id).await;
        send_ack(ctx, m.xfer_id, false, &reason).await?;
        return Ok(());
    }
    if m.eof {
        let inc = guard.take().expect("eof 分片时接收态必在");
        let saved = inc.path.display().to_string();
        tracing::info!(name = %ctx.peer_name, bytes = inc.received, path = %saved, "推送接收完成");
        crate::audit::log(
            "file_recv_done",
            &ctx.peer_name,
            &ctx.peer_fp,
            serde_json::json!({ "name": inc.path.file_name().and_then(|n| n.to_str()).unwrap_or("?"), "bytes": inc.received }),
        );
        send_ack(ctx, m.xfer_id, true, &saved).await?;
    }
    Ok(())
}

/// `FileCancel`：中止匹配方向的传输（接收态丢弃半成品；拉取任务取消令牌）。
pub async fn on_cancel(ctx: &Arc<SessionCtx>, m: FileCancel) -> anyhow::Result<()> {
    tracing::info!(name = %ctx.peer_name, xfer = m.xfer_id, reason = %m.reason, "对端取消传输");
    abort_incoming(ctx, m.xfer_id).await;
    if let Some(token) = ctx
        .files
        .outgoing
        .lock()
        .expect("outgoing 锁中毒")
        .remove(&m.xfer_id)
    {
        token.cancel();
    }
    Ok(())
}

/// 断开清理：半成品删除 + 拉取任务全停（会话断开时 session.rs 调用）。
pub async fn on_disconnect(ctx: &Arc<SessionCtx>) {
    if let Some(inc) = ctx.files.incoming.lock().await.take() {
        drop(inc.file);
        let _ = std::fs::remove_file(&inc.path);
        tracing::info!(path = %inc.path.display(), "会话断开，删除未完成的接收文件");
    }
    for (id, token) in ctx.files.outgoing.lock().expect("outgoing 锁中毒").drain() {
        token.cancel();
        tracing::debug!(xfer = id, "会话断开，拉取任务已取消");
    }
}

// ---------------------------------------------------------------------------
// 推送接收 / 拉取发送
// ---------------------------------------------------------------------------

async fn start_push_recv(ctx: &Arc<SessionCtx>, m: FileXferOffer) -> anyhow::Result<()> {
    let clean = sanitize_file_name(&m.name);
    let Some(name) = clean else {
        send_ack(ctx, m.xfer_id, false, "文件名不合法，已拒绝").await?;
        return Ok(());
    };
    let dir = match receive_dir() {
        Ok(d) => d,
        Err(err) => {
            send_ack(ctx, m.xfer_id, false, &format!("接收目录不可用：{err:#}")).await?;
            return Ok(());
        }
    };
    // 先查忙再建档（M1：被拒时不得留下 0 字节残留）；建档期间持锁，
    // 保证「查忙 + 占槽」原子（std File::create 是同步快操作，不跨 await）
    let mut guard = ctx.files.incoming.lock().await;
    if guard.is_some() {
        drop(guard);
        send_ack(ctx, m.xfer_id, false, "已有传输进行中，请稍后再试").await?;
        return Ok(());
    }
    let target = dedupe_target(&dir, &name);
    let file = match std::fs::File::create(&target) {
        Ok(f) => f,
        Err(err) => {
            drop(guard);
            send_ack(ctx, m.xfer_id, false, &format!("无法创建文件：{err}")).await?;
            return Ok(());
        }
    };
    tracing::info!(name = %ctx.peer_name, path = %target.display(), size = m.size, "开始接收推送");
    *guard = Some(IncomingXfer {
        xfer_id: m.xfer_id,
        file,
        path: target,
        next_seq: 0,
        received: 0,
    });
    Ok(())
}

async fn start_pull(ctx: &Arc<SessionCtx>, m: FileXferOffer) -> anyhow::Result<()> {
    let path = PathBuf::from(&m.remote_path);
    if !path.is_absolute() || !path.is_file() {
        send_cancel(ctx, m.xfer_id, "路径不存在或不是文件").await?;
        return Ok(());
    }
    let token = Arc::new(CancellationToken::new());
    ctx.files
        .outgoing
        .lock()
        .expect("outgoing 锁中毒")
        .insert(m.xfer_id, Arc::clone(&token));
    let ctx = Arc::clone(ctx);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    tokio::spawn(async move {
        // RAII 表项守卫：任意退出路径（含 panic 展开）必摘表项（M5）
        let _guard = OutgoingGuard {
            ctx: Arc::clone(&ctx),
            xfer_id: m.xfer_id,
        };
        let size = std::fs::metadata(&path).map(|md| md.len()).unwrap_or(0);
        tracing::info!(name = %ctx.peer_name, path = %path.display(), size, xfer = m.xfer_id, "开始发送拉取文件");
        let mut sent: u64 = 0;
        let mut seq: u32 = 0;
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(err) => {
                let _ = ctx
                    .send(
                        envelope::Msg::FileCancel(FileCancel {
                            xfer_id: m.xfer_id,
                            reason: format!("打开文件失败：{err}"),
                        }),
                        0,
                    )
                    .await;
                return;
            }
        };
        let mut reader = tokio::io::BufReader::with_capacity(CHUNK_SIZE, file);
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut io_error: Option<std::io::Error> = None;
        loop {
            let n = tokio::select! {
                _ = token.cancelled() => break,
                r = reader.read(&mut buf) => match r {
                    Ok(n) => n,
                    Err(err) => { io_error = Some(err); break; }
                },
            };
            let eof = n == 0;
            let chunk = envelope::Msg::FileXferChunk(FileXferChunk {
                xfer_id: m.xfer_id,
                seq,
                data: buf[..n].to_vec(),
                eof,
            });
            if ctx.send(chunk, 0).await.is_err() {
                break; // 连接已断
            }
            seq += 1;
            sent += n as u64;
            if eof {
                break;
            }
        }
        if let Some(err) = io_error {
            tracing::warn!(path = %path.display(), "拉取发送读文件失败: {err}");
            let _ = ctx
                .send(
                    envelope::Msg::FileCancel(FileCancel {
                        xfer_id: m.xfer_id,
                        reason: format!("读文件失败：{err}"),
                    }),
                    0,
                )
                .await;
            return;
        }
        if token.is_cancelled() {
            tracing::info!(xfer = m.xfer_id, "拉取发送已取消（已发 {sent} 字节）");
            return;
        }
        tracing::info!(name = %ctx.peer_name, bytes = sent, xfer = m.xfer_id, "拉取发送完成");
        crate::audit::log(
            "file_send_done",
            &ctx.peer_name,
            &ctx.peer_fp,
            serde_json::json!({ "name": name, "bytes": sent }),
        );
    });
    Ok(())
}

/// 丢弃接收态并删除半成品（乱序/写失败/对端取消共用出口）。
async fn abort_incoming(ctx: &SessionCtx, xfer_id: u64) {
    if let Some(inc) = ctx.files.incoming.lock().await.take() {
        if inc.xfer_id == xfer_id {
            drop(inc.file);
            let _ = std::fs::remove_file(&inc.path);
            tracing::info!(path = %inc.path.display(), "接收失败，删除半成品");
        } else {
            // 不同 id 的取消：恢复原接收态（防御分支，正常不达）
            *ctx.files.incoming.lock().await = Some(inc);
        }
    }
}

async fn send_ack(ctx: &SessionCtx, xfer_id: u64, ok: bool, message: &str) -> anyhow::Result<()> {
    ctx.send(
        envelope::Msg::FileXferAck(FileXferAck {
            xfer_id,
            ok,
            message: message.to_string(),
        }),
        0,
    )
    .await
}

async fn send_cancel(ctx: &SessionCtx, xfer_id: u64, reason: &str) -> anyhow::Result<()> {
    ctx.send(
        envelope::Msg::FileCancel(FileCancel {
            xfer_id,
            reason: reason.to_string(),
        }),
        0,
    )
    .await
}

// ---------------------------------------------------------------------------
// 纯函数（单测覆盖）
// ---------------------------------------------------------------------------

/// 接收目录：`<主目录>\Downloads\rd接收`（不存在则创建）。
pub fn receive_dir() -> anyhow::Result<PathBuf> {
    // 环境变量 RD_RECV_DIR 可重定向（e2e 测试注入临时目录用，生产不设），
    // 缺省落主目录 Downloads\rd接收
    let dir = match std::env::var("RD_RECV_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            let home = home_dir().ok_or_else(|| anyhow::anyhow!("无法确定主目录"))?;
            home.join("Downloads").join("rd接收")
        }
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("创建 {} 失败：{e}", dir.display()))?;
    Ok(dir)
}

/// 主目录（Windows USERPROFILE；类 POSIX HOME）。
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// 文件名消毒：只取最后一段路径组件；去控制字符；替换 Windows 非法字符
/// `< > : " | ? *` 为 `_`；拒绝空/`.`/`..`/保留设备名；限长 [`NAME_MAX`]。
/// 返回 None 表示整体拒绝（不猜用户意图）。
pub fn sanitize_file_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut out: String = last
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    out = out.trim().trim_end_matches('.').to_string();
    if out.is_empty() || out == "." || out == ".." {
        return None;
    }
    // Windows 保留设备名（含带扩展名形式：CON.txt 同样非法）
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        return None;
    }
    if out.chars().count() > NAME_MAX {
        out = out.chars().take(NAME_MAX).collect();
    }
    Some(out)
}

/// 重名去重：`report.pdf` → `report (1).pdf` → `report (2).pdf`（存在才递增）。
pub fn dedupe_target(dir: &Path, name: &str) -> PathBuf {
    let target = dir.join(name);
    if !target.exists() {
        return target;
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string();
    let ext = Path::new(name).extension().and_then(|s| s.to_str());
    for n in 1..1000u32 {
        let candidate = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(candidate);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} (重复)"))
}

/// 列目录：目录在前、名称不分大小写排序；条目元数据取不到的跳过（不整单失败）。
fn list_dir(target: &Path) -> anyhow::Result<Vec<FileEntry>> {
    let mut entries: Vec<FileEntry> = Vec::new();
    for entry in std::fs::read_dir(target)? {
        let Ok(entry) = entry else { continue };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = entry.metadata().map(|md| md.len()).unwrap_or(0);
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            dir: is_dir,
            size: if is_dir { 0 } else { size },
        });
    }
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal_and_empty() {
        assert_eq!(sanitize_file_name(""), None);
        assert_eq!(sanitize_file_name("."), None);
        assert_eq!(sanitize_file_name(".."), None);
        assert_eq!(sanitize_file_name("   "), None);
        // 穿越路径：消毒取末段，落点固定接收目录，穿越被中和（非拒绝）
        assert_eq!(sanitize_file_name("..\\..\\..\\boot"), Some("boot".into()));
    }

    #[test]
    fn sanitize_takes_last_component() {
        assert_eq!(
            sanitize_file_name("C:\\Users\\x\\a b.txt"),
            Some("a b.txt".into())
        );
        assert_eq!(sanitize_file_name("/tmp/evil.pdf"), Some("evil.pdf".into()));
    }

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(
            sanitize_file_name("a<b>:c?.txt"),
            Some("a_b__c_.txt".into())
        );
        // 控制字符剔除
        assert_eq!(
            sanitize_file_name("a\u{0}b\u{7}.txt"),
            Some("ab.txt".into())
        );
    }

    #[test]
    fn sanitize_rejects_reserved_device_names() {
        assert_eq!(sanitize_file_name("CON"), None);
        assert_eq!(sanitize_file_name("con.txt"), None);
        assert_eq!(sanitize_file_name("LPT9"), None);
        // 非保留名不受影响
        assert_eq!(
            sanitize_file_name("console.log"),
            Some("console.log".into())
        );
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "x".repeat(500);
        let got = sanitize_file_name(&long).unwrap();
        assert_eq!(got.chars().count(), NAME_MAX);
    }

    #[test]
    fn sanitize_trailing_dots_trimmed() {
        assert_eq!(sanitize_file_name("report..."), Some("report".into()));
    }

    #[test]
    fn dedupe_first_fit_then_increments() {
        let dir = std::env::temp_dir().join(format!("rd-file-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dedupe_target(&dir, "report.pdf");
        assert_eq!(p1.file_name().unwrap(), "report.pdf");
        std::fs::File::create(&p1).unwrap();
        let p2 = dedupe_target(&dir, "report.pdf");
        assert_eq!(p2.file_name().unwrap(), "report (1).pdf");
        std::fs::File::create(&p2).unwrap();
        let p3 = dedupe_target(&dir, "report.pdf");
        assert_eq!(p3.file_name().unwrap(), "report (2).pdf");
        // 无扩展名
        let q1 = dedupe_target(&dir, "notes");
        assert_eq!(q1.file_name().unwrap(), "notes");
        std::fs::File::create(&q1).unwrap();
        let q2 = dedupe_target(&dir, "notes");
        assert_eq!(q2.file_name().unwrap(), "notes (1)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_dir_sorts_dirs_first_and_counts_files() {
        let dir = std::env::temp_dir().join(format!("rd-file-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("zsub")).unwrap();
        std::fs::create_dir_all(dir.join("asub")).unwrap();
        std::fs::write(dir.join("b.txt"), b"12345").unwrap();
        std::fs::write(dir.join("a.txt"), b"1").unwrap();
        let entries = list_dir(&dir).unwrap();
        let dirs: Vec<&FileEntry> = entries.iter().filter(|e| e.dir).collect();
        let files: Vec<&FileEntry> = entries.iter().filter(|e| !e.dir).collect();
        assert_eq!(dirs.len(), 2);
        assert_eq!(files.len(), 2);
        // 目录在前
        assert!(entries.first().unwrap().dir);
        // 文件按名称不分大小写排序
        assert_eq!(files[0].name, "a.txt");
        assert_eq!(files[1].name, "b.txt");
        // size 只对文件有意义
        assert_eq!(files[0].size, 1);
        assert_eq!(dirs[0].size, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn receive_dir_env_override() {
        // L4：RD_RECV_DIR 可重定向（e2e 注入临时目录）；重定向与缺省分支
        // 都验一遍，不写真实用户目录之外的内容
        let tmp = std::env::temp_dir().join(format!("rd-recv-test-{}", std::process::id()));
        std::env::set_var("RD_RECV_DIR", &tmp);
        let dir = receive_dir().unwrap();
        assert_eq!(dir, tmp);
        assert!(dir.is_dir());
        std::env::remove_var("RD_RECV_DIR");
        std::fs::remove_dir_all(&tmp).unwrap();
        let dir = receive_dir().unwrap();
        assert!(dir.ends_with("Downloads\\rd接收") || dir.ends_with("Downloads/rd接收"));
        assert!(dir.is_dir());
    }
}
