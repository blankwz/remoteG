//! 会话命令（T-021）：窗口打开/关闭、视口上报、窗口组事件订阅。
//!
//! - `open_window`：**先 `register_window`**（帧路由就位，不丢首帧）再 RPC
//!   `OpenWindow`；失败时撤销注册。ack 的 `video_stream_id`/`tile_stream_id`
//!   仅透传展示（帧路由按帧头 `window_id`，见 conn.rs）。
//! - `close_window`：先撤销帧路由再发 `CloseWindow`。Agent 对 CloseWindow
//!   **不回包**（T-016 handle_close 无应答），故走 req_id=0 单向消息，
//!   避免 RPC 空等 5 s。
//! - `set_viewport`：`ViewportInfo` 单向上报（Agent 侧 150 ms 去抖）。
//! - `subscribe_group_events`：把广播中 root 命中的 `WindowEvent`
//!   （CHILD_ADDED/REMOVED、子窗口与根的 MOVED/TITLE、DESTROYED）、
//!   `Error{12}`（Fit 退化，非致命）与 `StatsReport`（连接级，无 window_id，
//!   转发给该设备全部组订阅者）推给前端；CHILD_ADDED 时顺带维护
//!   子窗口→root 帧映射（conn.rs `map_child_window`）。
//!
//! window_id 沿用 T-020 约定：前端十进制字符串，Rust 侧解析 u64。

use rd_proto::v0::{
    CloseWindow, Envelope, FitMode, OpenWindow, OpenWindowAck, QualityPreset, RenderLevel,
    SetDisplayWidth, SetPrivacy, SetQuality, StatsReport, ViewportInfo, WindowInfo, envelope,
    window_event,
};
use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::sync::broadcast;

use crate::conn::{ConnManager, DeviceConn};

/// 协议错误码 12：无法调整尺寸（Agent 已退化，控制端转本地缩放；05_api §4）。
const ERR_FIT_DEGRADED: u32 = 12;

// ---------------------------------------------------------------------------
// DTO（camelCase 对齐前端 api/session.ts）
// ---------------------------------------------------------------------------

/// open_window 的 Fit 参数（proto FitMode 的字符串形式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FitDto {
    Fit,
    Scale,
    Native,
}

impl FitDto {
    fn proto(self) -> FitMode {
        match self {
            FitDto::Fit => FitMode::Fit,
            FitDto::Scale => FitMode::Scale,
            FitDto::Native => FitMode::Native,
        }
    }
}

/// OpenWindowAck（前端展示用；流 id 仅透传，帧路由不依赖）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenWindowAckDto {
    /// 窗口句柄的十进制字符串。
    pub window_id: String,
    /// RenderLevel 枚举名（M1 恒 L0_PIXEL）。
    pub level: String,
    /// Codec 枚举名（HUD 显示）。
    pub codec: String,
    pub w: u32,
    pub h: u32,
    pub video_stream_id: u32,
    pub tile_stream_id: u32,
}

/// Agent 码率自适应统计（StatsReport，连接级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsDto {
    pub fps: u32,
    pub rtt_ms: u32,
    pub kbps: u32,
    pub decode_ms: u32,
}

/// 窗口组内的窗口条目（子窗口或根）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupWindowDto {
    /// 窗口句柄的十进制字符串。
    pub id: String,
    pub title: String,
    pub process: String,
    pub pid: u32,
    /// 屏幕坐标（前端相对根原点换算偏移）。
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub dpi: u32,
    pub resizable: bool,
}

/// 窗口组事件：CHILD_ADDED / CHILD_REMOVED / MOVED / TITLE / DESTROYED
/// （WindowEvent，root 命中）、FIT_DEGRADED（Error{12}）、STATS（StatsReport）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupEventDto {
    pub kind: String,
    /// 事件携带的窗口信息；FIT_DEGRADED / STATS 为 None。
    pub info: Option<GroupWindowDto>,
    /// FIT_DEGRADED 的提示文案。
    pub message: Option<String>,
    /// STATS 的统计值。
    pub stats: Option<StatsDto>,
    /// 事件所属窗口组根（十进制字符串）。
    pub root_id: String,
}

// ---------------------------------------------------------------------------
// 纯函数（映射与分类，单测覆盖）
// ---------------------------------------------------------------------------

/// 解析十进制窗口 id 字符串（前端传入；非法时给出用户可读错误）。
pub fn parse_window_id(id: &str) -> Result<u64, String> {
    id.trim()
        .parse::<u64>()
        .map_err(|_| format!("窗口 ID 无效：{id:?}（应为十进制数字）"))
}

/// `WindowInfo` → 组窗口 DTO（id 转十进制字符串）。
pub fn group_window_dto(w: &WindowInfo) -> GroupWindowDto {
    GroupWindowDto {
        id: w.id.to_string(),
        title: w.title.clone(),
        process: w.process.clone(),
        pid: w.pid,
        x: w.x,
        y: w.y,
        w: w.w,
        h: w.h,
        dpi: w.dpi,
        resizable: w.resizable,
    }
}

/// WindowEvent.kind → proto 枚举名；未知值兜底 "UNKNOWN"。
fn window_event_kind_name(kind: i32) -> String {
    window_event::Kind::try_from(kind)
        .map(|k| k.as_str_name().to_string())
        .unwrap_or_else(|_| "UNKNOWN".into())
}

/// 广播信封 → 组事件 DTO（root 命中才转发；None = 不属于该组/不消费）。
///
/// - `WindowEvent`：`root_id` 与订阅的组根一致即转发（含根自身的
///   MOVED/TITLE/DESTROYED 与子窗口 CHILD_*/MOVED）；
/// - `Error{12}`：Fit 退化（Agent 广播，req_id=0，无 window_id 字段，
///   转发给该设备全部组订阅者，前端按当前激活标签处理）；
/// - `StatsReport`：连接级统计，同上转发。
pub fn group_event(env: &Envelope, root: u64) -> Option<GroupEventDto> {
    match &env.msg {
        Some(envelope::Msg::WindowEvent(ev)) => {
            if ev.root_id != root {
                return None;
            }
            Some(GroupEventDto {
                kind: window_event_kind_name(ev.kind),
                info: ev.info.as_ref().map(group_window_dto),
                message: None,
                stats: None,
                root_id: ev.root_id.to_string(),
            })
        }
        Some(envelope::Msg::Error(e)) if e.code == ERR_FIT_DEGRADED => Some(GroupEventDto {
            kind: "FIT_DEGRADED".into(),
            info: None,
            message: if e.message.is_empty() {
                None
            } else {
                Some(e.message.clone())
            },
            stats: None,
            root_id: root.to_string(),
        }),
        Some(envelope::Msg::Stats(s)) => Some(group_stats_event(*s, root)),
        _ => None,
    }
}

fn group_stats_event(s: StatsReport, root: u64) -> GroupEventDto {
    GroupEventDto {
        kind: "STATS".into(),
        info: None,
        message: None,
        stats: Some(StatsDto {
            fps: s.fps,
            rtt_ms: s.rtt_ms,
            kbps: s.kbps,
            decode_ms: s.decode_ms,
        }),
        root_id: root.to_string(),
    }
}

/// OpenWindowAck → DTO。
pub fn ack_to_dto(ack: &OpenWindowAck) -> OpenWindowAckDto {
    OpenWindowAckDto {
        window_id: ack.window_id.to_string(),
        level: match RenderLevel::try_from(ack.level) {
            Ok(RenderLevel::L0Pixel) => "L0_PIXEL".into(),
            Ok(RenderLevel::L1Hybrid) => "L1_HYBRID".into(),
            Ok(RenderLevel::L2Semantic) => "L2_SEMANTIC".into(),
            _ => format!("UNKNOWN({})", ack.level),
        },
        codec: match rd_proto::v0::Codec::try_from(ack.codec) {
            Ok(rd_proto::v0::Codec::H264420) => "H264_420".into(),
            Ok(rd_proto::v0::Codec::H264444) => "H264_444".into(),
            Ok(rd_proto::v0::Codec::Hevc444) => "HEVC_444".into(),
            Ok(rd_proto::v0::Codec::Av1) => "AV1".into(),
            _ => format!("unknown({})", ack.codec),
        },
        w: ack.w,
        h: ack.h,
        video_stream_id: ack.video_stream_id,
        tile_stream_id: ack.tile_stream_id,
    }
}

fn get_conn(state: &ConnManager, device_id: &str) -> Result<std::sync::Arc<DeviceConn>, String> {
    state
        .get_device(device_id)
        .ok_or_else(|| "设备未连接".to_string())
}

/// 对端错误信封 → 用户可读消息。
fn agent_error(e: &rd_proto::v0::Error) -> String {
    if e.message.is_empty() {
        format!("对端错误 {}", e.code)
    } else {
        format!("对端错误 {}: {}", e.code, e.message)
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令（签名与 lib.rs 注册一致）
// ---------------------------------------------------------------------------

/// 打开窗口会话：注册帧路由 → RPC `OpenWindow` → ack。
///
/// `max_fps` 传 0 表示未指定（Agent 用配置默认值）。
#[tauri::command]
pub async fn open_window(
    device_id: String,
    window_id: String,
    fit: FitDto,
    max_fps: u32,
    view_only: bool,
    on_frame: Channel<InvokeResponseBody>,
    state: State<'_, ConnManager>,
) -> Result<OpenWindowAckDto, String> {
    let root = parse_window_id(&window_id)?;
    let conn = get_conn(&state, &device_id)?;

    // 先注册帧路由再发 OpenWindow（任务卡步骤 1 顺序）：ack 之后视频/瓦片
    // 数据流随即开始推送，注册先行保证不丢首帧。
    conn.register_window(root, on_frame);
    let msg = envelope::Msg::OpenWindow(OpenWindow {
        window_id: root,
        fit: fit.proto() as i32,
        // M1 只做像素级渲染（L0），能力协商上限即 L0
        max_level: RenderLevel::L0Pixel as i32,
        max_fps,
        // 只看不控（v0.1/D-013 包4）：Agent 侧注入门按此位拦截
        view_only,
    });
    let reply = match conn.rpc(msg).await {
        Ok(reply) => reply,
        Err(e) => {
            conn.unregister_window(root);
            return Err(format!("打开窗口失败：{e:#}"));
        }
    };
    match reply.msg {
        Some(envelope::Msg::OpenWindowAck(ack)) => {
            tracing::info!(
                device = %device_id,
                window = format!("{root:#x}"),
                fit = ?fit,
                streams = format!("{}/{}", ack.video_stream_id, ack.tile_stream_id),
                size = format!("{}x{}", ack.w, ack.h),
                codec = %ack.codec,
                "窗口会话已打开"
            );
            Ok(ack_to_dto(&ack))
        }
        Some(envelope::Msg::Error(e)) => {
            conn.unregister_window(root);
            Err(agent_error(&e))
        }
        _ => {
            conn.unregister_window(root);
            Err("对端回复了意外的消息类型".into())
        }
    }
}

/// 关闭窗口会话：撤销帧路由 → 单向 `CloseWindow`（Agent 不回包）。
///
/// 窗口未打开时 Agent 静默忽略，因此重复关闭安全；连接已断时报错但不影响
/// 本地清理（路由已撤销）。
#[tauri::command]
pub async fn close_window(
    device_id: String,
    window_id: String,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let root = parse_window_id(&window_id)?;
    let conn = get_conn(&state, &device_id)?;
    conn.unregister_window(root);
    conn.send(envelope::Msg::CloseWindow(CloseWindow { window_id: root }))
        .await
        .map_err(|e| format!("关闭窗口失败：{e:#}"))
}

/// 上报视口（Fit 模式 1:1 适配；Agent 侧 150 ms 去抖后调整远端窗口）。
///
/// `dpr` 传 `devicePixelRatio / zoom`：zoom > 1 时远端窗口变小、本地放大显示。
#[tauri::command]
pub async fn set_viewport(
    device_id: String,
    window_id: String,
    css_w: u32,
    css_h: u32,
    dpr: f32,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let root = parse_window_id(&window_id)?;
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::Viewport(ViewportInfo {
        window_id: root,
        css_w,
        css_h,
        dpr,
    }))
    .await
    .map_err(|e| format!("上报视口失败：{e:#}"))
}

/// 画质档位（T-028 F5）：RPC `SetQuality`，Agent 成功时原样 echo。
///
/// `window_id` 传 `None`/空串 = 会话内全部窗口；`preset` 为
/// `"auto"|"high"|"medium"|"low"`（前端工具栏字符串直传）。
#[tauri::command]
pub async fn set_quality(
    device_id: String,
    window_id: Option<String>,
    preset: String,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let proto_preset = match preset.as_str() {
        "auto" => QualityPreset::Auto,
        "high" => QualityPreset::High,
        "medium" => QualityPreset::Medium,
        "low" => QualityPreset::Low,
        other => return Err(format!("未知画质档位：{other}")),
    };
    let root = match window_id.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => parse_window_id(s)?,
        None => 0, // 全部窗口
    };
    let conn = get_conn(&state, &device_id)?;
    let msg = envelope::Msg::SetQuality(SetQuality {
        window_id: root,
        preset: proto_preset as i32,
    });
    let reply = conn
        .rpc(msg)
        .await
        .map_err(|e| format!("设置画质失败：{e:#}"))?;
    match reply.msg {
        // Agent 原样 echo = 已生效
        Some(envelope::Msg::SetQuality(echo)) => {
            tracing::info!(device = %device_id, window = format!("{root:#x}"), ?echo.preset, "画质档位已生效");
            Ok(())
        }
        Some(envelope::Msg::Error(e)) => Err(agent_error(&e)),
        _ => Err("对端回复了意外的消息类型".into()),
    }
}

/// 隐私屏开关（T-028 F9，Pro 功能）：RPC `SetPrivacy`，Agent 原样 echo。
///
/// `on=true` 时被控端先弹知情提示再黑屏；会话断开自动解除。
#[tauri::command]
pub async fn set_privacy(
    device_id: String,
    on: bool,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    let msg = envelope::Msg::SetPrivacy(SetPrivacy { window_id: 0, on });
    let reply = conn
        .rpc(msg)
        .await
        .map_err(|e| format!("设置隐私屏失败：{e:#}"))?;
    match reply.msg {
        Some(envelope::Msg::SetPrivacy(echo)) => {
            tracing::info!(device = %device_id, on = echo.on, "隐私屏已切换");
            Ok(())
        }
        Some(envelope::Msg::Error(e)) => Err(agent_error(&e)),
        _ => Err("对端回复了意外的消息类型".into()),
    }
}

/// 订阅窗口组事件流（该设备的广播中 root 命中的子集）。
///
/// CHILD_ADDED 时顺带登记子窗口→root 帧映射，使子窗口视频/瓦片帧路由到
/// 本组 Channel；连接结束或前端 Channel 关闭时转发任务自动退出。
/// 注：CHILD_REMOVED 的映射解除依赖 `unregister_window` 整体清理（conn.rs
/// 未暴露 unmap_child），残留映射只影响已注销子窗口的帧投递，无副作用。
#[tauri::command]
pub async fn subscribe_group_events(
    device_id: String,
    root_id: String,
    on_event: Channel<GroupEventDto>,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let root = parse_window_id(&root_id)?;
    let conn = get_conn(&state, &device_id)?;
    let mut rx = conn.subscribe();
    let mut closed = conn.closed();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                // 连接结束（读循环退出置 closed）
                _ = closed.changed() => break,
                ev = rx.recv() => match ev {
                    Ok(env) => {
                        if let Some(dto) = group_event(&env, root) {
                            if dto.kind == "CHILD_ADDED"
                                && let Some(info) = &dto.info
                                && let Ok(child) = info.id.parse::<u64>()
                            {
                                conn.map_child_window(child, root);
                            }
                            // 前端 Channel 已关闭（标签关闭/页面卸载）→ 任务结束
                            if on_event.send(dto).is_err() {
                                break;
                            }
                        }
                    }
                    // 积压溢出：丢弃一段事件继续（瓦片/视频流不经过此路径）
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            device = %device_id,
                            root = root,
                            skipped,
                            "组事件订阅积压溢出，已跳过 {skipped} 条"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
    Ok(())
}

/// 会话级状态订阅（v0.1/D-013 包4）：把 Agent 的 SessionState（锁屏/暂停/
/// Agent 退出等）转发给前端；连接结束或前端 Channel 关闭时任务退出。
#[tauri::command]
pub async fn subscribe_session_state(
    device_id: String,
    on_state: Channel<SessionStateDto>,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    let mut rx = conn.subscribe();
    let mut closed = conn.closed();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = closed.changed() => break,
                ev = rx.recv() => match ev {
                    Ok(env) => {
                        let Some(envelope::Msg::SessionState(s)) = &env.msg else {
                            continue;
                        };
                        let dto = SessionStateDto {
                            kind: s.kind,
                            reason: s.reason.clone(),
                        };
                        if on_state.send(dto).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!("SessionState 订阅积压溢出，跳过 {skipped} 条");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
    Ok(())
}

/// SessionState 前端 DTO（kind = proto 枚举原始 i32）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateDto {
    pub kind: i32,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// 虚拟屏（T-044 一期控制端接线）：状态订阅 + 换档 RPC
// ---------------------------------------------------------------------------

/// Agent 虚拟屏状态通知的 DTO（`VirtualDisplayState`，05_api §9.1）。
#[derive(Debug, Clone, Serialize)]
pub struct VdisplayStateDto {
    /// 0 CREATED / 1 SWITCHED / 2 DESTROYED / 3 FAILED
    pub kind: i32,
    pub width: u32,
    pub height: u32,
    pub message: String,
}

/// 订阅 Agent 虚拟屏状态（建屏/换档/撤屏/失败）。断开或前端 Channel 关闭
/// 时任务自动退出（与 `subscribe_session_state` 同款模式）。
#[tauri::command]
pub async fn subscribe_vdisplay_state(
    device_id: String,
    on_state: Channel<VdisplayStateDto>,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    let mut rx = conn.subscribe();
    let mut closed = conn.closed();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = closed.changed() => break,
                ev = rx.recv() => match ev {
                    Ok(env) => {
                        let Some(envelope::Msg::VdisplayState(s)) = &env.msg else {
                            continue;
                        };
                        let dto = VdisplayStateDto {
                            kind: s.kind,
                            width: s.width,
                            height: s.height,
                            message: s.message.clone(),
                        };
                        if on_state.send(dto).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "虚拟屏状态订阅积压溢出，跳过 {skipped} 条");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
    Ok(())
}

/// 虚拟屏换档（`SetDisplayWidth` RPC，tag 53）：成功回 echo（同 req_id），
/// 尺寸另由 `VirtualDisplayState{SWITCHED}` 通知；非法档位/未建屏回
/// `Error{60}`。档位范围 540~1080 步进 10（05_api §9.1）。
#[tauri::command]
pub async fn set_display_width(
    device_id: String,
    width: u32,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    let reply = conn
        .rpc(envelope::Msg::SetDisplayWidth(SetDisplayWidth { width }))
        .await
        .map_err(|e| format!("发送换档请求失败：{e:#}"))?;
    match reply.msg {
        // echo：RPC 回执到位；新尺寸等 SWITCHED 通知（订阅路径）
        Some(envelope::Msg::SetDisplayWidth(_)) => Ok(()),
        Some(envelope::Msg::Error(e)) => Err(agent_error(&e)),
        _ => Err("换档响应异常（非 echo/错误）".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 单元测试（任务卡：window_id 解析、Fit 映射、组事件分类、ack/窗口 DTO 映射）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rd_proto::v0::{Error as PbError, StatsReport, WindowEvent};

    /// 构造 WindowInfo。
    fn window(id: u64, x: i32, y: i32) -> WindowInfo {
        WindowInfo {
            id,
            title: format!("窗口{id}"),
            process: "notepad.exe".into(),
            pid: 4242,
            icon_png: Vec::new(),
            x,
            y,
            w: 800,
            h: 600,
            dpi: 96,
            resizable: true,
            minimized: false,
            ..WindowInfo::default()
        }
    }

    fn event(kind: window_event::Kind, info: Option<WindowInfo>, root_id: u64) -> Envelope {
        Envelope {
            req_id: 0,
            msg: Some(envelope::Msg::WindowEvent(WindowEvent {
                kind: kind as i32,
                info,
                root_id,
            })),
        }
    }

    // ---- parse_window_id ----

    #[test]
    fn parse_window_id_accepts_decimal_and_strips_space() {
        assert_eq!(parse_window_id("4660"), Ok(0x1234));
        assert_eq!(parse_window_id(" 42 "), Ok(42));
        assert_eq!(
            parse_window_id("18446744073709551615"),
            Ok(u64::MAX),
            "u64 全范围可解析（前端字符串保精度）"
        );
    }

    #[test]
    fn parse_window_id_rejects_garbage_and_negative() {
        assert!(parse_window_id("").is_err());
        assert!(parse_window_id("0x10").is_err());
        assert!(parse_window_id("-1").is_err());
        assert!(parse_window_id("abc").is_err());
        assert!(parse_window_id("99999999999999999999").is_err());
    }

    // ---- FitDto → proto ----

    #[test]
    fn fit_dto_maps_to_proto_values() {
        assert_eq!(FitDto::Fit.proto(), FitMode::Fit);
        assert_eq!(FitDto::Scale.proto(), FitMode::Scale);
        assert_eq!(FitDto::Native.proto(), FitMode::Native);
        // proto 数值与 05_api §2 一致
        assert_eq!(FitMode::Fit as i32, 0);
        assert_eq!(FitMode::Scale as i32, 1);
        assert_eq!(FitMode::Native as i32, 2);
    }

    #[test]
    fn fit_dto_deserializes_uppercase_json() {
        assert_eq!(
            serde_json::from_str::<FitDto>("\"FIT\"").unwrap(),
            FitDto::Fit
        );
        assert_eq!(
            serde_json::from_str::<FitDto>("\"SCALE\"").unwrap(),
            FitDto::Scale
        );
        assert_eq!(
            serde_json::from_str::<FitDto>("\"NATIVE\"").unwrap(),
            FitDto::Native
        );
        assert!(serde_json::from_str::<FitDto>("\"fit\"").is_err());
    }

    // ---- ack_to_dto ----

    #[test]
    fn ack_to_dto_maps_fields_and_string_id() {
        let ack = OpenWindowAck {
            window_id: u64::MAX,
            level: RenderLevel::L0Pixel as i32,
            video_stream_id: 3,
            tile_stream_id: 5,
            codec: rd_proto::v0::Codec::H264420 as i32,
            w: 1920,
            h: 1080,
        };
        let dto = ack_to_dto(&ack);
        assert_eq!(dto.window_id, "18446744073709551615");
        assert_eq!(dto.level, "L0_PIXEL");
        assert_eq!(dto.codec, "H264_420");
        assert_eq!(dto.w, 1920);
        assert_eq!(dto.h, 1080);
        assert_eq!(dto.video_stream_id, 3);
        assert_eq!(dto.tile_stream_id, 5);
    }

    #[test]
    fn ack_to_dto_tolerates_unknown_enum_values() {
        let ack = OpenWindowAck {
            level: 99,
            codec: 99,
            ..OpenWindowAck::default()
        };
        let dto = ack_to_dto(&ack);
        assert!(dto.level.starts_with("UNKNOWN"), "{}", dto.level);
        assert!(dto.codec.starts_with("unknown"), "{}", dto.codec);
    }

    // ---- group_window_dto ----

    #[test]
    fn group_window_dto_maps_id_to_string() {
        let dto = group_window_dto(&window(0xDEADBEEF, -5, 7));
        assert_eq!(dto.id, "3735928559");
        assert_eq!(dto.x, -5);
        assert_eq!(dto.y, 7);
        assert_eq!(dto.title, "窗口3735928559");
        assert!(dto.resizable);
    }

    // ---- group_event 分类 ----

    #[test]
    fn child_events_forwarded_when_root_matches() {
        for (kind, name) in [
            (window_event::Kind::ChildAdded, "CHILD_ADDED"),
            (window_event::Kind::ChildRemoved, "CHILD_REMOVED"),
            (window_event::Kind::Moved, "MOVED"),
            (window_event::Kind::Title, "TITLE"),
            (window_event::Kind::Destroyed, "DESTROYED"),
            (window_event::Kind::Created, "CREATED"),
        ] {
            let dto = group_event(&event(kind, Some(window(9, 1, 2)), 7), 7)
                .unwrap_or_else(|| panic!("{name} 应转发"));
            assert_eq!(dto.kind, name);
        }
    }

    #[test]
    fn child_added_event_carries_child_info() {
        let dto = group_event(
            &event(window_event::Kind::ChildAdded, Some(window(9, 3, 4)), 7),
            7,
        )
        .expect("应转发");
        assert_eq!(dto.kind, "CHILD_ADDED");
        let info = dto.info.expect("info");
        assert_eq!(info.id, "9");
        assert_eq!(info.x, 3);
        assert_eq!(dto.root_id, "7");
    }

    #[test]
    fn events_of_other_roots_are_dropped() {
        assert!(
            group_event(
                &event(window_event::Kind::ChildAdded, Some(window(9, 0, 0)), 999),
                7
            )
            .is_none()
        );
        // 顶层事件（root_id=0）由 T-020 的列表页订阅，组订阅不消费
        assert!(
            group_event(
                &event(window_event::Kind::Created, Some(window(7, 0, 0)), 0),
                7
            )
            .is_none()
        );
    }

    #[test]
    fn fit_degraded_error_maps_to_event() {
        let env = Envelope {
            req_id: 0,
            msg: Some(envelope::Msg::Error(PbError {
                code: ERR_FIT_DEGRADED,
                message: "窗口不可调整尺寸".into(),
            })),
        };
        let dto = group_event(&env, 7).expect("Error12 应转发");
        assert_eq!(dto.kind, "FIT_DEGRADED");
        assert_eq!(dto.message.as_deref(), Some("窗口不可调整尺寸"));
        assert_eq!(dto.root_id, "7");
    }

    #[test]
    fn other_errors_are_not_forwarded() {
        for code in [1u32, 2, 10, 11, 13] {
            let env = Envelope {
                req_id: 0,
                msg: Some(envelope::Msg::Error(PbError {
                    code,
                    message: "boom".into(),
                })),
            };
            assert!(group_event(&env, 7).is_none(), "code={code} 不应转发");
        }
    }

    #[test]
    fn stats_report_forwarded_without_window_id() {
        let env = Envelope {
            req_id: 0,
            msg: Some(envelope::Msg::Stats(StatsReport {
                fps: 58,
                rtt_ms: 12,
                kbps: 7800,
                decode_ms: 3,
            })),
        };
        let dto = group_event(&env, 7).expect("Stats 应转发");
        assert_eq!(dto.kind, "STATS");
        let stats = dto.stats.expect("stats");
        assert_eq!(
            stats,
            StatsDto {
                fps: 58,
                rtt_ms: 12,
                kbps: 7800,
                decode_ms: 3
            }
        );
    }

    #[test]
    fn non_group_messages_are_dropped() {
        let env = Envelope {
            req_id: 1,
            msg: Some(envelope::Msg::OpenWindowAck(OpenWindowAck::default())),
        };
        assert!(group_event(&env, 7).is_none());
        let env = Envelope {
            req_id: 0,
            msg: None,
        };
        assert!(group_event(&env, 7).is_none());
    }

    #[test]
    fn empty_fit_degraded_message_is_none() {
        let env = Envelope {
            req_id: 0,
            msg: Some(envelope::Msg::Error(PbError {
                code: ERR_FIT_DEGRADED,
                message: String::new(),
            })),
        };
        let dto = group_event(&env, 7).expect("应转发");
        assert!(dto.message.is_none(), "空文案应交由前端默认提示");
    }

    // ---- agent_error ----

    #[test]
    fn agent_error_includes_code_and_message() {
        let e = PbError {
            code: 11,
            message: "窗口处于最小化".into(),
        };
        assert_eq!(agent_error(&e), "对端错误 11: 窗口处于最小化");
        let e = PbError {
            code: 10,
            message: String::new(),
        };
        assert_eq!(agent_error(&e), "对端错误 10");
    }
}
