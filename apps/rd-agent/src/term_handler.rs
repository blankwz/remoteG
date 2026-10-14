//! 终端处理:OpenTerminal/Resize → rd-term 会话 ↔ QUIC 双向流(T-017 实现)。
//!
//! 本任务(T-015)只定义桩签名;T-017 实现时注意:
//! - **终端 VT 字节原样转发,勿拦截/改写 DSR**。rd-term 读线程已内置 ConPTY
//!   首启 DSR(`\x1b[6n`)应答逻辑(`reply_dsr`,以光标 (1,1) 应答);Agent 转
//!   发层只做 `u32 len + 原始字节` 的搬运,不做任何转义序列解析。
//! - `TerminalOpened.stream_id` 填 `Conn::accept_bidi` 返回的 QUIC stream id
//!   (rd-transport `DataTx::id()`),控制端按 id 路由(05_api 补充)。
//! - `state.config.terminal_enabled` 的总开关拦截已在 session.rs 分发处完成,
//!   这里收到消息即视为已放行。

use crate::session::SessionCtx;
use rd_proto::v0::{OpenTerminal, TerminalResize};

/// 终端类控制流消息(载荷由 T-017 消费,桩阶段无读者)。
#[derive(Debug)]
#[allow(dead_code)]
pub enum TermMsg {
    Open(OpenTerminal),
    Resize(TerminalResize),
}

/// 终端挂接:`term_id` → 本连接持有的终端会话与流任务。
/// T-017 实现;本任务为空壳(默认构造)。
#[derive(Debug, Default)]
pub struct TermAttachments;

/// 处理一条终端消息(T-017 实现;桩不回包)。
pub fn handle(ctx: &SessionCtx, req_id: u32, msg: TermMsg) -> anyhow::Result<()> {
    let _ = (ctx, req_id, msg);
    Ok(())
}

/// 会话断开:关闭本连接打开的终端会话(T-017 实现;桩为空)。
pub fn on_disconnect(ctx: &SessionCtx) {
    let _ = ctx;
}
