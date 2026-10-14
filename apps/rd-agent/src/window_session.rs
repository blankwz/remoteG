//! 窗口会话管线:capture → encode → QUIC 数据流(T-016/T-027 实现)。
//!
//! 本任务(T-015)只定义分发用消息枚举与桩签名;捕获/编码/流建立逻辑由
//! T-016 填充,`SessionCtx.windows` 即其挂接点。

use crate::session::SessionCtx;
use rd_proto::v0::{CloseWindow, ListWindows, OpenWindow, ViewportInfo};

/// 窗口类控制流消息(session.rs 分发时按类型归组;载荷由 T-016 消费)。
#[derive(Debug)]
#[allow(dead_code)]
pub enum WindowMsg {
    List(ListWindows),
    Open(OpenWindow),
    Close(CloseWindow),
    Viewport(ViewportInfo),
}

/// 窗口子会话集合:`window_id` → (捕获, 编码, 发送任务)。
/// T-016 实现;本任务为空壳(默认构造)。
#[derive(Debug, Default)]
pub struct WindowSessions;

impl WindowSessions {
    /// 指定窗口是否已打开捕获会话(T-016 实现;桩恒 false)。
    #[allow(dead_code)]
    pub fn is_open(&self, id: u64) -> bool {
        let _ = id;
        false
    }
}

/// 处理一条窗口消息。业务错误应回 `Error{code:10/11/12}` 并返回 Ok;
/// 返回 Err 仅用于协议级致命错误(T-016 实现;桩为空)。
pub fn handle(ctx: &SessionCtx, req_id: u32, msg: WindowMsg) -> anyhow::Result<()> {
    let _ = (ctx, req_id, msg);
    Ok(())
}

/// 会话断开:停止全部窗口捕获与编码任务(T-016 实现;桩为空)。
pub fn on_disconnect(ctx: &SessionCtx) {
    let _ = ctx;
}
