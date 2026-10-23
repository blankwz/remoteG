//! 终端会话命令（T-023）：open_terminal / term_input / term_resize。
//!
//! - `open_terminal`：**先 RPC `OpenTerminal` 拿 `stream_id`，再 `register_term`**
//!   （任务卡步骤 1 顺序）。Agent 回 `TerminalOpened` 先于快照，但 conn.rs 的
//!   `register_term` 对未注册流有 2 s TTL 缓存兜底、注册时按序补投，时序安全；
//!   ack 的 `stream_id` 透传前端（term_input 按它路由到双向流发送端）。
//! - `term_input`：输入字节走 `term_write`（QUIC 双向流 `u32 len + raw`）；
//!   前端以空载荷做存活探测 —— 进程退出后 Agent 关流（conn.rs 路由移除），
//!   写失败即退出信号（协议无 TerminalExit 消息，见任务卡步骤 6 上报）。
//! - `term_resize`：`TerminalResize` 单向上报（Agent 侧 `ResizePseudoConsole`，
//!   不回包，与 ViewportInfo 同款 req_id=0 语义）。

use rd_proto::v0::{Error as PbError, OpenTerminal, TerminalOpened, TerminalResize, envelope};
use serde::Serialize;
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::conn::{ConnManager, DeviceConn};

// ---------------------------------------------------------------------------
// DTO（camelCase 对齐前端 api/term.ts）
// ---------------------------------------------------------------------------

/// open_terminal 的 ack：term_id 用于 resize / resume，stream_id 用于输入路由。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTerminalAckDto {
    pub term_id: String,
    pub stream_id: u32,
}

/// OpenTerminalAck → DTO（透传，无映射逻辑；保留测试锚点防字段漂移）。
pub fn opened_to_dto(opened: &TerminalOpened) -> OpenTerminalAckDto {
    OpenTerminalAckDto {
        term_id: opened.term_id.clone(),
        stream_id: opened.stream_id,
    }
}

fn get_conn(state: &ConnManager, device_id: &str) -> Result<std::sync::Arc<DeviceConn>, String> {
    state
        .get_device(device_id)
        .ok_or_else(|| "设备未连接".to_string())
}

/// 对端错误信封 → 用户可读消息（与会话命令同款格式）。
fn terminal_error(e: &PbError) -> String {
    if e.message.is_empty() {
        format!("对端错误 {}", e.code)
    } else {
        format!("对端错误 {}: {}", e.code, e.message)
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令（签名与 lib.rs 注册一致）
// ---------------------------------------------------------------------------

/// 打开/恢复终端会话：RPC `OpenTerminal` → `TerminalOpened{term_id, stream_id}`
/// → `register_term`（此后输出数据异步到达 `on_data`，裸 VT 字节无帧前缀）。
///
/// `resume_id` 非空表示恢复既有会话（Agent 回放环形缓冲）；恢复失败
/// Agent 回 `Error{30}`（不存在或已过期），由前端决定清 id 后新开。
#[tauri::command]
pub async fn open_terminal(
    device_id: String,
    shell: String,
    cols: u32,
    rows: u32,
    resume_id: Option<String>,
    on_data: Channel<InvokeResponseBody>,
    state: State<'_, ConnManager>,
) -> Result<OpenTerminalAckDto, String> {
    let conn = get_conn(&state, &device_id)?;
    let resumed = resume_id.is_some();
    let msg = envelope::Msg::OpenTerm(OpenTerminal {
        shell,
        cols,
        rows,
        resume_id: resume_id.unwrap_or_default(),
    });
    let reply = conn
        .rpc(msg)
        .await
        .map_err(|e| format!("打开终端失败：{e:#}"))?;
    match reply.msg {
        Some(envelope::Msg::TermOpened(opened)) => {
            // 先拿 stream_id 再注册前端路由（conn.rs 对注册前到达的输出缓存 2 s
            // 并在注册时按序补投，快照不会丢）
            conn.register_term(u64::from(opened.stream_id), on_data);
            tracing::info!(
                device = %device_id,
                term = %opened.term_id,
                stream = opened.stream_id,
                resumed,
                "终端会话已打开"
            );
            Ok(opened_to_dto(&opened))
        }
        Some(envelope::Msg::Error(e)) => Err(terminal_error(&e)),
        _ => Err("对端回复了意外的消息类型".into()),
    }
}

/// 写终端输入字节（`data` 为原始字节，含 VT 序列；空载荷作存活探测）。
///
/// 进程退出后 conn.rs 已移除路由，此处报"终端流 X 未打开"，前端据此显示
/// 退出遮罩；设备断开时报"设备未连接"。
#[tauri::command]
pub async fn term_input(
    device_id: String,
    stream_id: u32,
    data: Vec<u8>,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    conn.term_write(u64::from(stream_id), &data)
        .await
        .map_err(|e| format!("终端输入失败：{e:#}"))
}

/// 上报终端尺寸（单向 `TerminalResize`；Agent 侧 ResizePseudoConsole）。
///
/// 失败不致命（网络瞬断时下一次 resize 重试），返回错误仅供前端记录。
#[tauri::command]
pub async fn term_resize(
    device_id: String,
    term_id: String,
    cols: u32,
    rows: u32,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::TermResize(TerminalResize {
        term_id,
        cols,
        rows,
    }))
    .await
    .map_err(|e| format!("调整终端尺寸失败：{e:#}"))
}

// ---------------------------------------------------------------------------
// 单元测试（错误格式化与 ack DTO 序列化锚点）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_error_includes_code_and_message() {
        let e = PbError {
            code: 30,
            message: "终端会话不存在或已过期".into(),
        };
        assert_eq!(terminal_error(&e), "对端错误 30: 终端会话不存在或已过期");
        let e = PbError {
            code: 30,
            message: String::new(),
        };
        assert_eq!(terminal_error(&e), "对端错误 30");
    }

    #[test]
    fn opened_to_dto_serializes_camel_case() {
        let opened = TerminalOpened {
            term_id: "term-1".into(),
            stream_id: 7,
        };
        let dto = opened_to_dto(&opened);
        assert_eq!(dto.term_id, "term-1");
        assert_eq!(dto.stream_id, 7);
        let json = serde_json::to_value(&dto).expect("serialize");
        assert_eq!(json["termId"], "term-1");
        assert_eq!(json["streamId"], 7);
    }
}
