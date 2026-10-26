//! 终端处理:OpenTerminal/Resize → rd-term 会话 ↔ QUIC 双向流(T-017)。
//!
//! 数据面(05_api §3):终端流为双向流,帧格式 `u32 len + 原始字节`。
//! **终端 VT 字节原样转发,勿拦截/改写 DSR** —— rd-term 读线程已内置
//! ConPTY 首启 DSR(`\x1b[6n`)应答逻辑(`reply_dsr`,以光标 (1,1) 应答),
//! Agent 转发层只做 `u32 len + 原始字节` 的搬运,不做任何转义序列解析。
//!
//! 打开流程(任务卡步骤 4):
//! 1. `resume_id` 非空 → `attach` 既有会话(失败回 Error 30,不静默新开);
//!    否则 `open` 新 shell 后 `attach`(`terminal_enabled` 总开关已在
//!    session.rs 分发处拦截,走到这里即视为放行);
//! 2. `open_bidi(Terminal)` → 先回 `TerminalOpened{term_id, stream_id}`
//!    (`stream_id` 填 `DataTx::id()`,控制端按 id 路由),再写回放快照;
//!    **回包先于快照**:快照可达 1 MiB,若对端等回包才开始读流,QUIC 流控
//!    窗口会被快照占满、双方互相等待死锁;TerminalOpened 走控制流,与数据
//!    流无顺序约束,不影响流内「快照在广播输出之前」的顺序;
//! 3. 输出方向:rd-term 广播 → `send_raw` 持续转发;`Exit`/会话关闭 → 发
//!    FIN 关流;输入方向:控制端 → `terms.write`。
//!
//! 断开(任务卡步骤 6):**只 detach** —— 取消本连接挂接令牌,两个方向任务
//! 随即退出;rd-term 会话与回放缓冲保留,空闲超时后回收,期间同 `resume_id`
//! 重连可看到之前输出(DoD)。
//!
//! 日志只记录 term_id/stream_id 等元数据,**绝不记录终端内容与输入字节**(DoD)。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use rd_proto::v0::{envelope, Error, OpenTerminal, TerminalOpened, TerminalResize};
use rd_term::TermOutput;
use rd_transport::{DataFrame, DataRx, DataTx, StreamKind};
use tokio_util::sync::CancellationToken;

use crate::session::{AgentState, SessionCtx};

/// 协议错误码 30:终端启动失败(05_api §4;附加失败/恢复失败沿用同一码)。
const ERR_TERMINAL: u32 = rd_term::session::ERR_TERMINAL_SPAWN;

/// `OpenTerminal` 未携带尺寸时(proto3 零值)的默认值。
const DEFAULT_COLS: u32 = 80;
const DEFAULT_ROWS: u32 = 24;

/// 本连接的终端挂接停止令牌。`TermAttachments` 为 unit 结构(session.rs
/// 构造固定),挂接状态放进程级静态、按连接分键(`Arc<Conn>` 指针地址;
/// ctx 存活期间连接必然存活,指针不会复用)。
static ATTACHMENTS: LazyLock<Mutex<HashMap<usize, Vec<CancellationToken>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 终端类控制流消息(session.rs 分发时按类型归组)。
#[derive(Debug)]
pub enum TermMsg {
    Open(OpenTerminal),
    Resize(TerminalResize),
}

/// 终端挂接标记:每个连接一个实例(session.rs 构造)。实际挂接状态在模块级
/// [`ATTACHMENTS`] 表中,按连接键隔离;本类型保留分发签名所需的占位。
#[derive(Debug, Default)]
pub struct TermAttachments;

/// 处理一条终端消息。`Open` 内部含 async 操作(开 QUIC 流、回包),而分发
/// 签名为同步(session.rs 固定),故以 `block_in_place + block_on` 桥接:
/// rd-agent 为多线程 runtime(main.rs),block_in_place 把当前 worker 转为
/// 阻塞线程并交出调度核心,不会卡死其它任务;期间仅本连接控制流分发暂缓。
pub fn handle(ctx: &SessionCtx, req_id: u32, msg: TermMsg) -> anyhow::Result<()> {
    match msg {
        TermMsg::Open(m) => {
            // 审计（合规留痕）：远端开了 shell（敏感度最高的一类操作）
            crate::audit::log(
                "terminal_open",
                &ctx.peer_name,
                &ctx.peer_fp,
                serde_json::json!({ "shell": m.shell }),
            );
            // 共享暂停时拒绝新终端(D-013 包3):暂停语义 = 一切控制通道停止
            if ctx.state.paused.load(std::sync::atomic::Ordering::SeqCst) {
                let rt = tokio::runtime::Handle::current();
                return tokio::task::block_in_place(|| {
                    rt.block_on(send_error(ctx, req_id, "共享已暂停,请对方先在托盘取消暂停"))
                });
            }
            let rt = tokio::runtime::Handle::current();
            tokio::task::block_in_place(|| rt.block_on(open_terminal(ctx, req_id, m)))
        }
        // resize 为纯同步 rd-term 调用(内部持锁),直接执行;尺寸超界收敛到
        // u16。失败返回 Err,由分发层(handle_soft)记录,不打断会话。
        TermMsg::Resize(m) => {
            ctx.state
                .terms
                .resize(&m.term_id, size_u16(m.cols), size_u16(m.rows))
        }
    }
}

/// 会话断开:取消本连接全部挂接令牌(只 detach,见模块注释)。
pub fn on_disconnect(ctx: &SessionCtx) {
    let tokens = ATTACHMENTS
        .lock()
        .expect("attachments 锁中毒")
        .remove(&conn_key(ctx));
    if let Some(tokens) = tokens {
        for token in tokens {
            token.cancel();
        }
        tracing::debug!(name = %ctx.peer_name, "终端挂接已全部脱离(会话交由 rd-term 空闲回收)");
    }
}

/// 「暂停共享」时调用(D-013 包3/E4 评审 P1):此前暂停只覆盖画面与注入,
/// 已挂接的活 shell 仍双向收发且断开后保留 10 分钟可 resume——家人按了
/// 暂停仍有一个可用通道。现在取消全部连接的挂接并杀掉全部 rd-term 会话,
/// 暂停语义完整;恢复共享后由控制端重新打开终端。
pub fn pause_all(state: &AgentState) {
    let all: Vec<CancellationToken> = {
        let mut map = ATTACHMENTS.lock().expect("attachments 锁中毒");
        map.drain().flat_map(|(_, v)| v).collect()
    };
    for token in all {
        token.cancel();
    }
    state.terms.close_all();
    tracing::info!("共享已暂停:终端挂接与全部会话已终止");
}

// ---------------------------------------------------------------------------
// 打开/恢复
// ---------------------------------------------------------------------------

/// 打开/恢复终端:attach(或 open+attach)→ 开双向流 → 回 TerminalOpened →
/// 启动两个方向的任务。
async fn open_terminal(ctx: &SessionCtx, req_id: u32, m: OpenTerminal) -> anyhow::Result<()> {
    let attached = if m.resume_id.is_empty() {
        open_and_attach(ctx, req_id, &m).await
    } else {
        attach_existing(ctx, req_id, &m.resume_id).await
    };
    // Ok(None) = 已回 Error 30,本条消息处理完毕
    let Some((term_id, snapshot, out_rx)) = attached? else {
        return Ok(());
    };

    // 开终端双向流;stream id 立即回给控制端(顺序理由见模块注释)
    let (tx, rx) = ctx.conn.open_bidi(StreamKind::Terminal).await?;
    let stream_id = u32::try_from(tx.id())
        .map_err(|_| anyhow::anyhow!("QUIC stream id {} 超出 u32", tx.id()))?;
    ctx.send(
        envelope::Msg::TermOpened(TerminalOpened {
            term_id: term_id.clone(),
            stream_id,
        }),
        req_id,
    )
    .await?;

    // 挂接生命周期:一个令牌联动两方向;进程静态表登记,断开时统一取消
    let stop = CancellationToken::new();
    register(ctx, stop.clone());
    tokio::spawn(pump_input(
        Arc::clone(&ctx.state),
        term_id.clone(),
        rx,
        stop.clone(),
    ));
    tokio::spawn(forward_output(tx, out_rx, snapshot, stop));

    tracing::info!(
        name = %ctx.peer_name,
        term = %term_id,
        stream_id,
        resumed = !m.resume_id.is_empty(),
        "终端已挂接"
    );
    Ok(())
}

/// 新开 shell 并附加。失败回 `Error{code:30}`(消息不含内部细节),
/// 返回 `Ok(None)` 表示已应答、无需继续。
async fn open_and_attach(
    ctx: &SessionCtx,
    req_id: u32,
    m: &OpenTerminal,
) -> anyhow::Result<
    Option<(
        String,
        Vec<u8>,
        tokio::sync::broadcast::Receiver<TermOutput>,
    )>,
> {
    let cols = if m.cols == 0 {
        DEFAULT_COLS as u16
    } else {
        size_u16(m.cols)
    };
    let rows = if m.rows == 0 {
        DEFAULT_ROWS as u16
    } else {
        size_u16(m.rows)
    };
    match ctx.state.terms.open(&m.shell, cols, rows) {
        Ok(term_id) => {
            let (snapshot, rx) = ctx.state.terms.attach(&term_id)?;
            Ok(Some((term_id, snapshot, rx)))
        }
        Err(err) => {
            // shell 不在白名单 / ConPTY 启动失败;日志不含码值与终端内容
            tracing::warn!(name = %ctx.peer_name, shell = %m.shell, "打开终端失败: {err:#}");
            send_error(ctx, req_id, "终端启动失败").await?;
            Ok(None)
        }
    }
}

/// 附加既有会话(断线重连)。会话不存在/已回收 → `Error{code:30}`,
/// 不静默改开新会话(控制端语义上期望恢复历史输出,由其决定是否新开)。
async fn attach_existing(
    ctx: &SessionCtx,
    req_id: u32,
    resume_id: &str,
) -> anyhow::Result<
    Option<(
        String,
        Vec<u8>,
        tokio::sync::broadcast::Receiver<TermOutput>,
    )>,
> {
    match ctx.state.terms.attach(resume_id) {
        Ok((snapshot, rx)) => Ok(Some((resume_id.to_string(), snapshot, rx))),
        Err(err) => {
            tracing::warn!(name = %ctx.peer_name, "附加终端会话失败(不存在或已回收): {err:#}");
            send_error(ctx, req_id, "终端会话不存在或已过期").await?;
            Ok(None)
        }
    }
}

/// 回协议 `Error{code:30}`。
async fn send_error(ctx: &SessionCtx, req_id: u32, message: &str) -> anyhow::Result<()> {
    ctx.send(
        envelope::Msg::Error(Error {
            code: ERR_TERMINAL,
            message: message.to_string(),
        }),
        req_id,
    )
    .await
}

/// u32 → u16 尺寸收敛(防御 proto3 零值与超界值)。
fn size_u16(v: u32) -> u16 {
    v.clamp(1, u32::from(u16::MAX)) as u16
}

// ---------------------------------------------------------------------------
// 挂接表
// ---------------------------------------------------------------------------

/// 连接键:`Arc<Conn>` 指针地址(ctx 存活期间连接必然存活)。
fn conn_key(ctx: &SessionCtx) -> usize {
    Arc::as_ptr(&ctx.conn) as usize
}

/// 登记挂接令牌(on_disconnect 统一取消)。
fn register(ctx: &SessionCtx, stop: CancellationToken) {
    ATTACHMENTS
        .lock()
        .expect("attachments 锁中毒")
        .entry(conn_key(ctx))
        .or_default()
        .push(stop);
}

// ---------------------------------------------------------------------------
// 双向转发
// ---------------------------------------------------------------------------

/// 输出方向:先写回放快照(重连可见历史输出),再持续转发 rd-term 广播。
/// `Exit`(子进程退出)或会话关闭时发送 FIN 关流,并通知输入方向退出。
async fn forward_output(
    mut tx: DataTx,
    mut out_rx: tokio::sync::broadcast::Receiver<TermOutput>,
    snapshot: Vec<u8>,
    stop: CancellationToken,
) {
    if !snapshot.is_empty() {
        if let Err(err) = tx.send_raw(&snapshot).await {
            tracing::debug!("终端快照发送失败,停止转发: {err:#}");
            stop.cancel();
            return;
        }
    }
    loop {
        let out = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            out = out_rx.recv() => match out {
                Ok(out) => out,
                // 广播积压溢出:历史无法回补,终端自会全量重绘,继续转发新数据
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("终端输出广播落后 {n} 条,部分输出已丢失");
                    continue;
                }
                // 会话已被 close/回收
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        };
        match out {
            // VT 字节(含 DSR 查询)原样转发,不解析不改写
            TermOutput::Data(data) => {
                if tx.send_raw(&data).await.is_err() {
                    break; // 对端/连接已不可用
                }
            }
            TermOutput::Exit(_) => break, // 子进程退出:关流
        }
    }
    let _ = tx.finish();
    stop.cancel();
}

/// 输入方向:控制端 → `terms.write`。流结束(对端 FIN)、连接断开(quinn
/// 使流立即出错)或令牌取消即退出,并通知输出方向停止。
async fn pump_input(
    state: Arc<AgentState>,
    term_id: String,
    mut rx: DataRx,
    stop: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            biased;
            _ = stop.cancelled() => None,
            next = rx.recv() => match next {
                Ok(next) => next,
                Err(err) => {
                    tracing::debug!("终端输入流结束: {err:#}");
                    None
                }
            },
        };
        let Some(frame) = next else { break };
        match frame {
            DataFrame::Raw(bytes) => {
                // 日志禁止记录输入内容:失败只记错误,不记字节
                if let Err(err) = state.terms.write(&term_id, &bytes) {
                    tracing::debug!("终端输入写入失败(会话可能已回收): {err:#}");
                    break;
                }
            }
            // 终端流只承载 Raw 帧;不打印帧内容
            DataFrame::Video(..) | DataFrame::Tile(..) => {
                tracing::debug!("终端流收到非预期帧类型,忽略");
            }
        }
    }
    stop.cancel();
}
