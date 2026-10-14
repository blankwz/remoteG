//! 窗口列表与窗口事件命令（T-020）。
//!
//! - `list_windows`：RPC `ListWindows` → `WindowList`（图标 PNG 转 base64
//!   data URL，前端 `<img src>` 直用）；
//! - `subscribe_window_events`：订阅 [`DeviceConn`] 广播，把**顶层** `WindowEvent`
//!   经 tauri `Channel` 推给前端；页面先订阅后拉列表，事件驱动增删改、无轮询。
//!
//! window_id 以十进制字符串进 DTO（u64 可能超出 JS number 安全整数范围，
//! URL 参数 `#/session/:id?open=<windowId>` 也天然是字符串）。

use rd_proto::v0::{envelope, window_event, Envelope, ListWindows, WindowInfo};
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::State;
use tokio::sync::broadcast;

use crate::conn::ConnManager;

// ---------------------------------------------------------------------------
// DTO（camelCase 对齐前端 api/windows.ts）
// ---------------------------------------------------------------------------

/// 可捕获窗口条目（列表页卡片）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowDto {
    /// 窗口句柄（hwnd）的十进制字符串。
    pub id: String,
    pub title: String,
    /// 进程名（如 explorer.exe）。
    pub process: String,
    pub pid: u32,
    /// 图标 PNG 的 `data:image/png;base64,...`；无图标为 None（前端显示占位）。
    pub icon: Option<String>,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub dpi: u32,
    pub resizable: bool,
    pub minimized: bool,
}

/// 顶层窗口事件（CREATED / DESTROYED / MOVED / TITLE / CHILD_*）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowEventDto {
    /// [`window_event::Kind`] 的 proto 枚举名（如 "CREATED"）。
    pub kind: String,
    /// 事件携带的最新窗口信息；DESTROYED 等事件可能缺省（None）。
    pub info: Option<WindowDto>,
    /// 窗口组根的窗口 id（十进制字符串；0 = 无组）。
    pub root_id: String,
}

impl WindowEventDto {
    fn kind(kind: i32, info: Option<WindowDto>, root_id: u64) -> Self {
        Self {
            kind: kind_name(kind).to_string(),
            info,
            root_id: root_id.to_string(),
        }
    }
}

/// Kind 枚举值 → proto 枚举名；未知值兜底 "UNKNOWN"（前端按无操作处理）。
fn kind_name(kind: i32) -> &'static str {
    window_event::Kind::try_from(kind)
        .map(|k| k.as_str_name())
        .unwrap_or("UNKNOWN")
}

fn to_dto(w: &WindowInfo) -> WindowDto {
    WindowDto {
        id: w.id.to_string(),
        title: w.title.clone(),
        process: w.process.clone(),
        pid: w.pid,
        icon: icon_data_url(&w.icon_png),
        x: w.x,
        y: w.y,
        w: w.w,
        h: w.h,
        dpi: w.dpi,
        resizable: w.resizable,
        minimized: w.minimized,
    }
}

/// PNG 字节 → data URL；空（Agent 未取到图标，proto3 缺省）为 None。
fn icon_data_url(png: &[u8]) -> Option<String> {
    if png.is_empty() {
        return None;
    }
    Some(format!("data:image/png;base64,{}", base64_encode(png)))
}

/// 标准 base64（RFC 4648，带 padding）。手写实现避免为本页新增依赖。
const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.as_chunks::<3>().0 {
        let n = u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]);
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 6) as usize & 63] as char);
        out.push(B64_ALPHABET[n as usize & 63] as char);
    }
    // 收尾：剩余 1/2 字节按 6 位组补零，再补 '=' 到 4 的倍数
    match data.len() % 3 {
        1 => {
            let b = u32::from(data[data.len() - 1]);
            out.push(B64_ALPHABET[(b >> 2) as usize] as char);
            out.push(B64_ALPHABET[((b & 0x03) << 4) as usize] as char);
            out.push_str("==");
        }
        2 => {
            let n = u32::from(data[data.len() - 2]) << 8 | u32::from(data[data.len() - 1]);
            out.push(B64_ALPHABET[(n >> 10) as usize] as char);
            out.push(B64_ALPHABET[(n >> 4) as usize & 63] as char);
            out.push(B64_ALPHABET[((n & 0x0f) << 2) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

// ---------------------------------------------------------------------------
// 顶层事件判定与广播转发
// ---------------------------------------------------------------------------

/// 信封 → 顶层窗口事件 DTO；非 WindowEvent 或子窗口事件返回 None。
///
/// # 顶层判定（T-020 约定，协议未显式区分）
/// Agent 侧窗口组语义：`root_id` 为该窗口所在组根的 hwnd，顶层窗口可能是
/// `0`（不属于任何组）或**自引用**（root_id == 自身 id，即自己是组根）。
/// 因此顶层 = `root_id == 0 || root_id == info.id`；其余（root_id 指向组根、
/// info 为组内子窗口）是 CHILD_ADDED / CHILD_REMOVED 类子窗口事件，
/// 列表页只展示顶层窗口，不消费。
fn top_level_window_event(env: &Envelope) -> Option<WindowEventDto> {
    let Some(envelope::Msg::WindowEvent(ev)) = &env.msg else {
        return None;
    };
    let info = ev.info.as_ref().map(to_dto);
    let is_top = ev.root_id == 0
        || info.as_ref().is_some_and(|w| w.id == ev.root_id.to_string());
    if !is_top {
        return None;
    }
    Some(WindowEventDto::kind(ev.kind, info, ev.root_id))
}

// ---------------------------------------------------------------------------
// Tauri 命令（签名与 lib.rs 注册一致）
// ---------------------------------------------------------------------------

/// 列出被控端可捕获窗口（顶层与窗口组根）。
#[tauri::command]
pub async fn list_windows(
    device_id: String,
    state: State<'_, ConnManager>,
) -> Result<Vec<WindowDto>, String> {
    let conn = state
        .get_device(&device_id)
        .ok_or_else(|| "设备未连接".to_string())?;
    let reply = conn
        .rpc(envelope::Msg::ListWindows(ListWindows {}))
        .await
        .map_err(|e| format!("获取窗口列表失败：{e:#}"))?;
    match reply.msg {
        Some(envelope::Msg::WindowList(list)) => Ok(list.windows.iter().map(to_dto).collect()),
        Some(envelope::Msg::Error(e)) => Err(format!("对端错误 {}: {}", e.code, e.message)),
        _ => Err("对端回复了意外的消息类型".to_string()),
    }
}

/// 订阅该设备的顶层窗口事件流；事件经 `on_event` 异步推给前端。
///
/// 前端 Channel 被丢弃（页面卸载）或连接结束时，转发任务自动退出；
/// 同一设备重复订阅由前端保证只保留最新一期的回调（事件应用幂等）。
#[tauri::command]
pub async fn subscribe_window_events(
    device_id: String,
    on_event: Channel<WindowEventDto>,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = state
        .get_device(&device_id)
        .ok_or_else(|| "设备未连接".to_string())?;
    let mut rx = conn.subscribe();
    let mut closed = conn.closed();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                // 连接结束（读循环退出置 closed）
                _ = closed.changed() => break,
                ev = rx.recv() => match ev {
                    Ok(env) => {
                        if let Some(dto) = top_level_window_event(&env) {
                            // 前端 Channel 已关闭 → 任务结束
                            if on_event.send(dto).is_err() {
                                break;
                            }
                        }
                    }
                    // 积压溢出：丢弃一段事件继续（前端可整页刷新兜底）
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            device = %conn.device_id(),
                            skipped,
                            "窗口事件订阅积压溢出，已跳过 {skipped} 条"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// 单元测试（任务卡：图标 base64、顶层事件过滤、DTO 映射）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rd_proto::v0::Error as PbError;

    /// 构造 WindowInfo（字段齐全，按需覆盖）。
    fn window(id: u64, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            title: title.into(),
            process: "explorer.exe".into(),
            pid: 4242,
            icon_png: Vec::new(),
            x: 10,
            y: 20,
            w: 800,
            h: 600,
            dpi: 96,
            resizable: true,
            minimized: false,
        }
    }

    /// 构造一条 WindowEvent 信封。
    fn event(kind: window_event::Kind, info: Option<WindowInfo>, root_id: u64) -> Envelope {
        Envelope {
            req_id: 0,
            msg: Some(envelope::Msg::WindowEvent(rd_proto::v0::WindowEvent {
                kind: kind as i32,
                info,
                root_id,
            })),
        }
    }

    // ---- base64（RFC 4648 §10 测试向量）----

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn icon_data_url_is_png_mime_with_base64_body() {
        assert_eq!(icon_data_url(&[]), None, "空图标应为 None（前端显示占位）");
        let url = icon_data_url(&[1, 2, 3]).expect("非空应有 data URL");
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        assert_eq!(url.rsplit(',').next(), Some("AQID"));
    }

    // ---- DTO 映射 ----

    #[test]
    fn to_dto_maps_fields_and_string_id() {
        let mut w = window(0x1234, "记事本");
        w.icon_png = vec![0xFF, 0xD8];
        w.minimized = true;
        let dto = to_dto(&w);
        assert_eq!(dto.id, "4660", "u64 hwnd 应为十进制字符串");
        assert_eq!(dto.title, "记事本");
        assert_eq!(dto.process, "explorer.exe");
        assert_eq!(dto.pid, 4242);
        assert_eq!(
            dto.icon.as_deref(),
            Some("data:image/png;base64,/9g="),
            "图标转 data URL"
        );
        assert_eq!((dto.x, dto.y, dto.w, dto.h), (10, 20, 800, 600));
        assert_eq!(dto.dpi, 96);
        assert!(dto.resizable);
        assert!(dto.minimized);
    }

    #[test]
    fn kind_name_maps_all_proto_kinds() {
        for (k, name) in [
            (window_event::Kind::Created, "CREATED"),
            (window_event::Kind::Destroyed, "DESTROYED"),
            (window_event::Kind::Moved, "MOVED"),
            (window_event::Kind::Title, "TITLE"),
            (window_event::Kind::ChildAdded, "CHILD_ADDED"),
            (window_event::Kind::ChildRemoved, "CHILD_REMOVED"),
        ] {
            assert_eq!(kind_name(k as i32), name);
        }
        assert_eq!(kind_name(99), "UNKNOWN", "未知枚举值兜底");
    }

    // ---- 顶层事件过滤 ----

    #[test]
    fn top_level_when_root_id_zero() {
        // 顶层窗口不属于任何组：root_id = 0
        let dto = top_level_window_event(&event(window_event::Kind::Created, Some(window(7, "A")), 0))
            .expect("root_id=0 应为顶层");
        assert_eq!(dto.kind, "CREATED");
        assert_eq!(dto.info.as_ref().expect("info").id, "7");
        assert_eq!(dto.root_id, "0");
    }

    #[test]
    fn top_level_when_root_id_self_referencing() {
        // 顶层窗口自身是组根：root_id == info.id
        let dto = top_level_window_event(&event(window_event::Kind::Title, Some(window(7, "A")), 7))
            .expect("自引用应为顶层");
        assert_eq!(dto.kind, "TITLE");
        assert_eq!(dto.root_id, "7");
    }

    #[test]
    fn child_window_event_is_filtered() {
        // root_id 指向组根 7，info 是子窗口 9 → 子窗口事件，不进列表
        assert!(
            top_level_window_event(&event(
                window_event::Kind::ChildAdded,
                Some(window(9, "child")),
                7
            ))
            .is_none()
        );
    }

    #[test]
    fn destroyed_with_empty_info_is_top_level_by_root_zero() {
        // proto3 缺省 info（None）且 root_id=0：仍转发，info 为 None（前端按 root_id 处理）
        let dto = top_level_window_event(&event(window_event::Kind::Destroyed, None, 0))
            .expect("应转发");
        assert_eq!(dto.kind, "DESTROYED");
        assert!(dto.info.is_none());
    }

    #[test]
    fn non_window_event_envelope_is_ignored() {
        let env = Envelope {
            req_id: 0,
            msg: Some(envelope::Msg::Error(PbError {
                code: 1,
                message: "boom".into(),
            })),
        };
        assert!(top_level_window_event(&env).is_none());
        let env = Envelope {
            req_id: 0,
            msg: None,
        };
        assert!(top_level_window_event(&env).is_none());
    }
}
