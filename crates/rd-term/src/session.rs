//! 终端会话管理：伪终端生命周期、输出广播、环形回放、空闲回收
//! （03_architecture/架构方案.md §3.5，T-014）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::MutexGuard;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use portable_pty::{Child, MasterPty, PtySize};
use tokio::sync::broadcast;

use crate::conpty::{self, PtyHandles};
use crate::ring::RingBuffer;

/// 协议错误码 30：终端启动失败（05_api/协议契约.md §4）。
pub const ERR_TERMINAL_SPAWN: u32 = 30;

/// broadcast 通道容量：约 256 个读块（4 KiB/块）的瞬时窗口。
const BROADCAST_CAP: usize = 256;
/// 读线程单次读取块大小。
const READ_BUF: usize = 4096;
/// 空闲看门狗轮询间隔上下限。
const IDLE_POLL_MIN: Duration = Duration::from_millis(100);
const IDLE_POLL_MAX: Duration = Duration::from_secs(1);

/// 终端输出消息：`Data` 为原始 VT 字节；子进程退出时广播一次 `Exit`。
#[derive(Debug, Clone)]
pub enum TermOutput {
    /// 原始 VT 输出字节。
    Data(Bytes),
    /// 子进程退出码。
    Exit(i32),
}

/// 终端会话管理器：`term_id`（128-bit 随机 hex）→ 会话。
pub struct TermManager {
    sessions: Arc<Mutex<HashMap<String, Arc<TermSession>>>>,
    idle_timeout: Duration,
}

struct TermSession {
    tx: broadcast::Sender<TermOutput>,
    ring: Mutex<RingBuffer>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    /// 子进程已确认退出（读线程 EOF 或 close），看门狗据此立即回收。
    exited: AtomicBool,
}

impl TermManager {
    /// 创建管理器；`idle_timeout` 为无订阅者会话的保留时长
    /// （生产 10 分钟，测试可调小）。
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            idle_timeout,
        }
    }

    /// 启动伪终端 shell，返回 `term_id`。
    ///
    /// shell 必须在白名单内（Windows：pwsh.exe/powershell.exe/cmd.exe/wsl.exe；
    /// 其他平台：`$SHELL`），否则返回协议错误码 30 语义的错误。
    pub fn open(&self, shell: &str, cols: u16, rows: u16) -> Result<String> {
        if !is_shell_allowed(shell) {
            return Err(anyhow!(
                "[{ERR_TERMINAL_SPAWN}] shell `{shell}` is not in the allowlist"
            ));
        }
        let term_id = new_term_id();
        let PtyHandles {
            reader,
            writer,
            master,
            child,
        } = conpty::spawn_pty(shell, cols, rows)?;
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        let session = Arc::new(TermSession {
            tx,
            ring: Mutex::new(RingBuffer::new()),
            writer: Mutex::new(writer),
            master: Mutex::new(master),
            child: Mutex::new(child),
            exited: AtomicBool::new(false),
        });
        self.lock_sessions()
            .insert(term_id.clone(), session.clone());
        spawn_read_thread(session.clone(), reader);
        spawn_idle_watchdog(
            self.idle_timeout,
            term_id.clone(),
            self.sessions.clone(),
            session,
        );
        Ok(term_id)
    }

    /// 取回放快照并订阅后续输出；两步在同一把 ring 锁内完成，
    /// 且读线程在锁内先 push 后 send，保证不丢不重。
    pub fn attach(&self, term_id: &str) -> Result<(Vec<u8>, broadcast::Receiver<TermOutput>)> {
        let session = self.session(term_id)?;
        let (snapshot, rx) = {
            let ring = session.ring.lock().unwrap();
            let snapshot = ring.snapshot();
            let rx = session.tx.subscribe();
            (snapshot, rx)
        };
        Ok((snapshot, rx))
    }

    /// 向终端写入输入字节。
    pub fn write(&self, term_id: &str, data: &[u8]) -> Result<()> {
        let session = self.session(term_id)?;
        let mut writer = session.writer.lock().unwrap();
        writer
            .write_all(data)
            .context("write to pty stdin failed")?;
        writer.flush().context("flush pty stdin failed")
    }

    /// 调整终端尺寸（ResizePseudoConsole）。
    pub fn resize(&self, term_id: &str, cols: u16, rows: u16) -> Result<()> {
        let session = self.session(term_id)?;
        let result = session
            .master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty failed");
        result
    }

    /// 关闭并移除会话，杀掉子进程。
    pub fn close(&self, term_id: &str) {
        if let Some(s) = self.lock_sessions().remove(term_id) {
            s.exited.store(true, Ordering::SeqCst);
            let _ = s.child.lock().unwrap().kill();
        }
    }

    /// 关闭并移除全部会话，杀掉全部子进程（被控端「暂停共享」时调用，
    /// 保证暂停后不存在仍可交互的活 shell——D-013 包3/E4 评审）。
    pub fn close_all(&self) {
        let sessions: Vec<_> = {
            let mut map = self.lock_sessions();
            map.drain().map(|(_, s)| s).collect()
        };
        for s in sessions {
            s.exited.store(true, Ordering::SeqCst);
            let _ = s.child.lock().unwrap().kill();
        }
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<TermSession>>> {
        self.sessions.lock().unwrap()
    }

    fn session(&self, term_id: &str) -> Result<Arc<TermSession>> {
        self.lock_sessions()
            .get(term_id)
            .cloned()
            .ok_or_else(|| anyhow!("terminal session `{term_id}` not found"))
    }
}

/// ConPTY 以 INHERIT_CURSOR 创建（portable-pty 默认）时，conhost 启动会发送
/// DSR 查询 `\x1b[6n` 并阻塞初始化，直到终端应答光标位置；不应答约 1 秒后
/// conhost 退出、输出管道关闭。Agent 不维护光标状态，以 (1,1) 应答放行。
/// unix openpty 不注入 DSR，无需此逻辑。
#[cfg(windows)]
const REPLY_DSR: bool = true;
#[cfg(not(windows))]
const REPLY_DSR: bool = false;

/// DSR 查询 `\x1b[6n`（ESC [ 6 n）。
const DSR_QUERY: [u8; 4] = [0x1b, b'[', b'6', b'n'];

/// 每会话一个读线程：阻塞读（无数据时挂起，CPU 不增长），
/// 在 ring 锁内先写入环形缓冲再广播，与 attach 互斥保证不丢不重。
fn spawn_read_thread(session: Arc<TermSession>, mut reader: Box<dyn Read + Send>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_BUF];
        let mut dsr_replied = false;
        // DSR 查询可能被拆到两次 read：暂存可能是其前缀的尾部字节。
        let mut dsr_tail: Vec<u8> = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if REPLY_DSR && !dsr_replied {
                        let mut scan = std::mem::take(&mut dsr_tail);
                        scan.extend_from_slice(&buf[..n]);
                        if scan.windows(DSR_QUERY.len()).any(|w| w == DSR_QUERY) {
                            reply_dsr(&session);
                            dsr_replied = true;
                        } else {
                            dsr_tail = dsr_partial_suffix(&scan);
                        }
                    }
                    let data = Bytes::copy_from_slice(&buf[..n]);
                    let mut ring = session.ring.lock().unwrap();
                    ring.push(&data);
                    let _ = session.tx.send(TermOutput::Data(data));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let code = {
            let mut child = session.child.lock().unwrap();
            child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1)
        };
        session.exited.store(true, Ordering::SeqCst);
        let _ = session.tx.send(TermOutput::Exit(code));
    });
}

/// 以光标 (1,1) 应答 conhost 的 DSR 查询；写入失败（会话已关）则静默忽略。
fn reply_dsr(session: &TermSession) {
    let mut writer = session.writer.lock().unwrap();
    let _ = writer.write_all(b"\x1b[1;1R").and_then(|_| writer.flush());
}

/// 返回 `scan` 尾部可能是 `DSR_QUERY` 前缀的最长后缀（最多 3 字节）。
fn dsr_partial_suffix(scan: &[u8]) -> Vec<u8> {
    for k in (1..DSR_QUERY.len()).rev() {
        if scan.ends_with(&DSR_QUERY[..k]) {
            return scan[scan.len() - k..].to_vec();
        }
    }
    Vec::new()
}

/// 每会话一个看门狗：无订阅者超过 `idle_timeout`（或子进程已退出且无订阅者）
/// 时杀进程并从管理器移除会话。
fn spawn_idle_watchdog(
    idle_timeout: Duration,
    term_id: String,
    sessions: Arc<Mutex<HashMap<String, Arc<TermSession>>>>,
    session: Arc<TermSession>,
) {
    let poll = (idle_timeout / 8).clamp(IDLE_POLL_MIN, IDLE_POLL_MAX);
    std::thread::spawn(move || {
        let mut last_active = Instant::now();
        loop {
            std::thread::sleep(poll);
            if session.tx.receiver_count() > 0 {
                last_active = Instant::now();
                continue;
            }
            let exited = session.exited.load(Ordering::SeqCst);
            if exited || last_active.elapsed() >= idle_timeout {
                let _ = session.child.lock().unwrap().kill();
                sessions.lock().unwrap().remove(&term_id);
                return;
            }
        }
    });
}

/// Windows 白名单 shell。
#[cfg(windows)]
const ALLOWED_SHELLS: [&str; 4] = ["pwsh.exe", "powershell.exe", "cmd.exe", "wsl.exe"];

fn is_shell_allowed(shell: &str) -> bool {
    is_allowed_name(&file_name_of(shell))
}

#[cfg(windows)]
fn is_allowed_name(name: &str) -> bool {
    ALLOWED_SHELLS.iter().any(|s| s.eq_ignore_ascii_case(name))
}

#[cfg(not(windows))]
fn is_allowed_name(name: &str) -> bool {
    let allowed = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    name == file_name_of(&allowed)
}

fn file_name_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

/// 生成 128-bit 随机 hex `term_id`；OS 熵不可用时用纳秒时钟兜底。
fn new_term_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0u64, |d| d.as_nanos() as u64);
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
        bytes[8..].copy_from_slice(&(!nanos).to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
