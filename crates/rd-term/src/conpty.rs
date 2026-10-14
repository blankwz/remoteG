//! 伪终端抽象：Windows 上是 ConPTY，macOS/Linux 上是 openpty，由 portable-pty
//! 统一（03_architecture/架构方案.md §3.5，T-014）。

use std::io::{Read, Write};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

/// 注入子进程的 `TERM` 值，保证 vim/htop 等 TUI 程序输出完整 256 色序列。
pub const TERM_ENV_VALUE: &str = "xterm-256color";

/// 一次成功 spawn 产出的全部句柄。
pub struct PtyHandles {
    /// 终端输出读取端（阻塞读）。
    pub reader: Box<dyn Read + Send>,
    /// 终端输入写入端。
    pub writer: Box<dyn Write + Send>,
    /// 主端：resize 用。
    pub master: Box<dyn MasterPty + Send>,
    /// 子进程：wait/kill 用。
    pub child: Box<dyn Child + Send + Sync>,
}

/// 启动伪终端并在其中执行 `shell`（`cols` x `rows`，TERM=xterm-256color）。
///
/// 所有失败均携带协议错误码 30 语义（05_api/协议契约.md §4：终端启动失败）。
pub fn spawn_pty(shell: &str, cols: u16, rows: u16) -> Result<PtyHandles> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .with_context(|| format!("[30] openpty({cols}x{rows}) failed"))?;

    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", TERM_ENV_VALUE);

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("[30] failed to spawn shell `{shell}`"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .context("[30] failed to clone pty reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("[30] failed to take pty writer")?;
    Ok(PtyHandles {
        reader,
        writer,
        master: pair.master,
        child,
    })
}
