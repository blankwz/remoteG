//! 文件传输命令（T-031，v0.2）：列目录 / 推送 / 拉取 / 取消。
//!
//! 与 proto §文件传输一致：分片与回执全部 `req_id=0`，按 `xfer_id` 路由到
//! 本模块命令任务（conn.rs read_loop → xfers 表 → mpsc）；命令任务阻塞消费
//! 直到 eof/Ack/Cancel，进度经 [`Channel`] 推给前端。
//!
//! 纪律：本地写盘同样做文件名消毒与重名去重（远端名字不可信，与被控端
//! file_handler 同规格）；每次传输结束都注销路由（连接断开时 read_loop
//! 感知发送端关闭自动清表）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tauri::State;
use tauri::ipc::Channel;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use rd_proto::v0::envelope;
use rd_proto::v0::{
    FileCancel, FileEntry, FileListAck, FileListReq, FileXferAck, FileXferChunk, FileXferOffer,
};

use crate::conn::{ConnManager, DeviceConn, RPC_TIMEOUT};
use std::sync::Arc;

/// 分片大小：与被控端 `file_handler::CHUNK_SIZE` 一致。
const CHUNK_SIZE: usize = 64 * 1024;
/// 文件名长度上限（与被控端 `NAME_MAX` 一致）。
const NAME_MAX: usize = 200;

/// xfer_id 全局单调分配（跨设备唯一，路由表按连接隔离、id 仅作配对键）。
static XFER_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_xfer_id() -> u64 {
    XFER_SEQ.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// 目录条目（file_list 返回）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntryDto {
    pub name: String,
    pub dir: bool,
    pub size: u64,
}

/// 传输进度事件（on_progress Channel 推送）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XferProgressDto {
    pub xfer_id: u64,
    pub name: String,
    pub bytes: u64,
    /// 推送时为已知文件大小；拉取时为 0（远端不先报大小，进度按字节展示）。
    pub total: u64,
    /// `sending` / `receiving` / `done` / `error`。
    pub phase: String,
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn get_conn(state: &ConnManager, device_id: &str) -> Result<Arc<DeviceConn>, String> {
    state
        .get_device(device_id)
        .ok_or_else(|| "设备未连接".to_string())
}

/// 对端错误信封 → 用户可读消息（与 session.rs 同语义）。
fn agent_error(e: &rd_proto::v0::Error) -> String {
    if e.message.is_empty() {
        format!("对端错误 {code}", code = e.code)
    } else {
        e.message.clone()
    }
}

/// 远端文件名消毒：只取末段路径组件、去控制字符、替换 Windows 非法字符、
/// 拒绝保留设备名、限长——与被控端 `file_handler::sanitize_file_name`
/// 同规格（本地写盘的名字来自远端，同样不可信）。None = 整体拒绝。
fn sanitize_file_name(raw: &str) -> Option<String> {
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
    // Windows 保留设备名（含带扩展名形式：CON.txt 同样非法）——与被控端
    // file_handler::tests 同表互指，改一处必改另一处
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

/// 重名去重：`a.txt` → `a (1).txt`（与被控端同规格）。
fn dedupe_target(dir: &Path, name: &str) -> PathBuf {
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
            Some(e) => dir.join(format!("{stem} ({n}).{e}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} (重复)"))
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 列目录结果：path 为被控端规范化后的绝对路径（空请求 = 主目录展开），
/// 前端以此为准拼接子路径——相对名在 Agent 侧会被拒绝（H1）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListResultDto {
    pub path: String,
    pub entries: Vec<FileEntryDto>,
}

/// 列被控端目录：path 为绝对路径，空 = 被控端主目录。
#[tauri::command]
pub async fn file_list(
    device_id: String,
    path: String,
    state: State<'_, ConnManager>,
) -> Result<FileListResultDto, String> {
    let conn = get_conn(&state, &device_id)?;
    let reply = tokio::time::timeout(
        RPC_TIMEOUT,
        conn.rpc(envelope::Msg::FileListReq(FileListReq { path })),
    )
    .await
    .map_err(|_| "列目录超时".to_string())?
    .map_err(|e| format!("列目录失败：{e:#}"))?;
    match reply.msg {
        Some(envelope::Msg::FileListAck(FileListAck { entries, path })) => Ok(FileListResultDto {
            path,
            entries: entries
                .into_iter()
                .map(|FileEntry { name, dir, size }| FileEntryDto { name, dir, size })
                .collect(),
        }),
        Some(envelope::Msg::Error(e)) => Err(agent_error(&e)),
        _ => Err("对端回复了意外的消息类型".into()),
    }
}

/// 推送本地文件到被控端（`Downloads\rd接收`）。paths 为本机绝对路径列表；
/// 进度经 Channel 推送；任一文件失败即中止并返回错误（已完成的不回滚）。
#[tauri::command]
pub async fn file_send(
    device_id: String,
    paths: Vec<String>,
    on_progress: Channel<XferProgressDto>,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    for local in paths {
        let path = PathBuf::from(&local);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("路径不含文件名：{local}"))?
            .to_string();
        let size = std::fs::metadata(&path)
            .map_err(|e| format!("读取文件信息失败（{local}）：{e}"))?
            .len();
        let xfer_id = next_xfer_id();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<rd_proto::v0::Envelope>(64);
        conn.register_xfer(xfer_id, tx);
        let result = push_one(&conn, xfer_id, &name, size, &path, &on_progress, &mut rx).await;
        conn.unregister_xfer(xfer_id);
        result?;
    }
    Ok(())
}

/// 单文件推送：offer → 分片 → 等 Ack。
async fn push_one(
    conn: &Arc<DeviceConn>,
    xfer_id: u64,
    name: &str,
    size: u64,
    path: &Path,
    on_progress: &Channel<XferProgressDto>,
    rx: &mut tokio::sync::mpsc::Receiver<rd_proto::v0::Envelope>,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("打开文件失败：{e}"))?;
    let offer = envelope::Msg::FileXferOffer(FileXferOffer {
        xfer_id,
        name: name.to_string(),
        size,
        pull: false,
        remote_path: String::new(),
    });
    conn.send(offer)
        .await
        .map_err(|e| format!("发送传输请求失败：{e:#}"))?;
    let _ = on_progress.send(XferProgressDto {
        xfer_id,
        name: name.to_string(),
        bytes: 0,
        total: size,
        phase: "sending".into(),
        message: None,
    });
    // 进度节流（M6）：GB 级文件逐片 IPC 会刷爆前端——字节增量超 1%（且至少
    // 1MiB）才发一条；eof/done/error 必发
    let throttle_step = (size / 100).max(1024 * 1024);
    let mut last_emitted: u64 = 0;
    let mut sent: u64 = 0;
    let mut seq: u32 = 0;
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取本地文件失败：{e}"))?;
        let eof = n == 0;
        let chunk = envelope::Msg::FileXferChunk(FileXferChunk {
            xfer_id,
            seq,
            data: buf[..n].to_vec(),
            eof,
        });
        if conn.send(chunk).await.is_err() {
            return Err("连接已断开，传输中止".into());
        }
        seq += 1;
        sent += n as u64;
        if eof || sent - last_emitted >= throttle_step {
            last_emitted = sent;
            let _ = on_progress.send(XferProgressDto {
                xfer_id,
                name: name.to_string(),
                bytes: sent,
                total: size,
                phase: "sending".into(),
                message: None,
            });
        }
        if eof {
            break;
        }
    }
    // 等 Ack：连接关闭（closed watch）或 Ack 信封（mpsc）。超时兜底防悬挂。
    let mut closed = conn.closed();
    let ack = tokio::select! {
        _ = closed.changed() => return Err("连接已断开，传输中止".into()),
        r = rx.recv() => r,
        _ = tokio::time::sleep(RPC_TIMEOUT) => {
            return Err("等待被控端确认超时".into());
        }
    };
    match ack.and_then(|env| env.msg) {
        Some(envelope::Msg::FileXferAck(FileXferAck { ok, message, .. })) => {
            if ok {
                let _ = on_progress.send(XferProgressDto {
                    xfer_id,
                    name: name.to_string(),
                    bytes: sent,
                    total: size,
                    phase: "done".into(),
                    message: Some(message),
                });
                Ok(())
            } else {
                let _ = on_progress.send(XferProgressDto {
                    xfer_id,
                    name: name.to_string(),
                    bytes: sent,
                    total: size,
                    phase: "error".into(),
                    message: Some(message.clone()),
                });
                Err(format!("被控端接收失败：{message}"))
            }
        }
        Some(envelope::Msg::Error(e)) => Err(agent_error(&e)),
        // L1：取消已注销路由 → recv 返回 None；此时分片已发完，按「已取消」
        // 报告而非误导性的「回执格式异常」
        None => Err("传输已取消".into()),
        _ => Err("被控端回执格式异常".into()),
    }
}

/// 拉取被控端文件到本机目录（dest_dir 为本机绝对路径，重名自动去重）。
/// 返回保存的完整路径。
#[tauri::command]
pub async fn file_pull(
    device_id: String,
    remote_path: String,
    dest_dir: String,
    on_progress: Channel<XferProgressDto>,
    state: State<'_, ConnManager>,
) -> Result<String, String> {
    let conn = get_conn(&state, &device_id)?;
    let name = sanitize_file_name(
        Path::new(&remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(""),
    )
    .ok_or_else(|| "远端路径不含合法文件名".to_string())?;
    let dest = PathBuf::from(&dest_dir);
    if !dest.is_absolute() {
        return Err("保存目录必须是绝对路径".into());
    }
    std::fs::create_dir_all(&dest).map_err(|e| format!("创建保存目录失败：{e}"))?;
    // dedupe + create_new 原子占位（M3）：同名并发拉取时第二路 create_new
    // 必失败，重跑 dedupe 换名，绝不截断第一路的产物
    let (target, file) = loop {
        let candidate = dedupe_target(&dest, &name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => break (candidate, f),
            Err(_e) if candidate.exists() => continue,
            Err(e) => return Err(format!("创建本地文件失败：{e}")),
        }
    };
    let xfer_id = next_xfer_id();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<rd_proto::v0::Envelope>(256);
    conn.register_xfer(xfer_id, tx);
    let result = pull_one(
        &conn,
        xfer_id,
        &remote_path,
        &target,
        file,
        &on_progress,
        &mut rx,
    )
    .await;
    conn.unregister_xfer(xfer_id);
    result
}

/// 拉取分片间隔上限：30 s 无分片视为对端异常（M5，防任务死亡后悬挂）。
const PULL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 单文件拉取：offer(pull) → 收分片落盘 → eof 完成。`file` 为已原子占位的
/// 目标文件句柄（M3）；写盘走 tokio::fs（M4：不阻塞 worker 拖慢控制流）。
async fn pull_one(
    conn: &Arc<DeviceConn>,
    xfer_id: u64,
    remote_path: &str,
    target: &Path,
    file: std::fs::File,
    on_progress: &Channel<XferProgressDto>,
    rx: &mut tokio::sync::mpsc::Receiver<rd_proto::v0::Envelope>,
) -> Result<String, String> {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let offer = envelope::Msg::FileXferOffer(FileXferOffer {
        xfer_id,
        name: String::new(),
        size: 0,
        pull: true,
        remote_path: remote_path.to_string(),
    });
    conn.send(offer)
        .await
        .map_err(|e| format!("发送拉取请求失败：{e:#}"))?;
    let mut file = tokio::fs::File::from_std(file);
    let _ = on_progress.send(XferProgressDto {
        xfer_id,
        name: name.clone(),
        bytes: 0,
        total: 0,
        phase: "receiving".into(),
        message: None,
    });
    let mut received: u64 = 0;
    // 进度节流（M6）：每 1MiB 字节增量发一条；eof/done 必发
    const THROTTLE_STEP: u64 = 1024 * 1024;
    let mut last_emitted: u64 = 0;
    let mut closed = conn.closed();
    loop {
        let env = tokio::select! {
            _ = closed.changed() => {
                drop(file);
                let _ = std::fs::remove_file(target);
                return Err("连接已断开，传输中止".into());
            }
            r = rx.recv() => r,
            // 对端任务意外死亡且连接未断时防悬挂（M5）
            _ = tokio::time::sleep(PULL_IDLE_TIMEOUT) => {
                drop(file);
                let _ = std::fs::remove_file(target);
                let _ = conn
                    .send(envelope::Msg::FileCancel(FileCancel {
                        xfer_id,
                        reason: "控制端等待分片超时".into(),
                    }))
                    .await;
                return Err("传输超时：被控端 30 秒未发送数据".into());
            }
        };
        let Some(env) = env else {
            // 路由表项被清（连接断开清理）：按断开处理
            drop(file);
            let _ = std::fs::remove_file(target);
            return Err("连接已断开，传输中止".into());
        };
        match env.msg {
            Some(envelope::Msg::FileXferChunk(FileXferChunk { data, eof, .. })) => {
                if let Err(e) = file.write_all(&data).await {
                    // 本地写失败：通知对端取消，清理半成品
                    let _ = conn
                        .send(envelope::Msg::FileCancel(FileCancel {
                            xfer_id,
                            reason: format!("控制端写盘失败：{e}"),
                        }))
                        .await;
                    let _ = std::fs::remove_file(target);
                    return Err(format!("写入本地磁盘失败：{e}"));
                }
                received += data.len() as u64;
                if eof || received - last_emitted >= THROTTLE_STEP {
                    last_emitted = received;
                    let _ = on_progress.send(XferProgressDto {
                        xfer_id,
                        name: target
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file")
                            .to_string(),
                        bytes: received,
                        total: 0,
                        phase: "receiving".into(),
                        message: None,
                    });
                }
                if eof {
                    break;
                }
            }
            Some(envelope::Msg::FileCancel(FileCancel { reason, .. })) => {
                drop(file);
                let _ = std::fs::remove_file(target);
                return Err(format!("被控端取消传输：{reason}"));
            }
            Some(envelope::Msg::FileXferAck(_)) => {} // 拉取无 Ack，忽略迟到回执
            _ => {}                                   // 其他消息不属本传输
        }
    }
    drop(file);
    let saved = target.display().to_string();
    let _ = on_progress.send(XferProgressDto {
        xfer_id,
        name,
        bytes: received,
        total: received,
        phase: "done".into(),
        message: Some(saved.clone()),
    });
    Ok(saved)
}

/// 取消进行中的传输（xfer_id 来自进度事件）。
#[tauri::command]
pub async fn file_cancel(
    device_id: String,
    xfer_id: u64,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::FileCancel(FileCancel {
        xfer_id,
        reason: "控制端取消".into(),
    }))
    .await
    .map_err(|e| format!("发送取消失败：{e:#}"))?;
    conn.unregister_xfer(xfer_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{dedupe_target, sanitize_file_name};

    #[test]
    fn 消毒取末段替换非法字符() {
        assert_eq!(sanitize_file_name("C:\\x\\a b.txt"), Some("a b.txt".into()));
        assert_eq!(
            sanitize_file_name("/tmp/evil:a?.pdf"),
            Some("evil_a_.pdf".into())
        );
        assert_eq!(sanitize_file_name(""), None);
        assert_eq!(sanitize_file_name(".."), None);
    }

    #[test]
    fn 去重_存在才递增() {
        let dir = std::env::temp_dir().join(format!("rd-ctl-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dedupe_target(&dir, "a.txt");
        assert_eq!(p1.file_name().unwrap(), "a.txt");
        std::fs::File::create(&p1).unwrap();
        let p2 = dedupe_target(&dir, "a.txt");
        assert_eq!(p2.file_name().unwrap(), "a (1).txt");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
