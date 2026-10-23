//! 输入注入命令（T-022）：鼠标移动走 QUIC datagram（fire-and-forget，seq
//! per-设备自增），点击 / 滚轮 / 按键 / 文本走控制流单向 `send`（req_id=0，
//! Agent 无应答，不走 RPC 空等；可靠性由 QUIC 流保证）。
//!
//! - 鼠标 datagram 帧（05_api §3）：`u8 type=1, u64 window_id, i32 x, i32 y,
//!   u32 seq` 全小端。seq 在命令层维护：conn.rs 不可改，而同一设备同时只有
//!   一条连接（ConnManager::put_device 替换旧连接），per-设备自增等价
//!   per-连接自增（重连后 seq 续走仍单调，不影响顺序语义）；
//! - 坐标均为远端窗口**物理像素**（前端 InputMapper 已含 dpr/zoom 换算）；
//! - 按键扫描码为 PS/2 Set 1 基本码 + extended 标志（前端 scancodes.ts
//!   查表），modifiers 位图仅透传（Agent 侧信息参考，不换算）。
//!
//! window_id 沿用约定：前端十进制字符串，Rust 侧解析 u64（复用
//! session::parse_window_id）。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use bytes::BufMut;
use rd_proto::v0::{KeyEvent, MouseButton, MouseWheel, TextInput, envelope};
use tauri::State;

use crate::commands::session::parse_window_id;
use crate::conn::{ConnManager, DeviceConn};

/// datagram 帧类型：鼠标移动（05_api §3）。
const DATAGRAM_TYPE_MOUSE_MOVE: u8 = 1;

/// datagram 帧长：`u8 + u64 + i32 + i32 + u32`。
const MOVE_DATAGRAM_LEN: usize = 1 + 8 + 4 + 4 + 4;

/// per-设备（等价 per-连接，见模块注释）的鼠标移动 datagram seq 自增器。
static MOVE_SEQ: LazyLock<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicU32>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// 纯函数（编码与 seq，单测覆盖）
// ---------------------------------------------------------------------------

/// 编码 `type=1` 鼠标移动 datagram：`[u8 1][u64 window_id][i32 x][i32 y][u32 seq]`
/// （全小端，05_api §3；Agent 侧 `parse_mouse_move` 消费）。
pub fn encode_mouse_move(window_id: u64, x: i32, y: i32, seq: u32) -> Vec<u8> {
    let mut buf = bytes::BytesMut::with_capacity(MOVE_DATAGRAM_LEN);
    buf.put_u8(DATAGRAM_TYPE_MOUSE_MOVE);
    buf.put_u64_le(window_id);
    buf.put_i32_le(x);
    buf.put_i32_le(y);
    buf.put_u32_le(seq);
    buf.freeze().to_vec()
}

/// 取该设备下一个 seq（自 1 起单调递增；溢出回绕由 u32 语义兜底）。
pub fn next_seq(device_id: &str) -> u32 {
    let mut map = MOVE_SEQ.lock().expect("move seq 表");
    let counter = Arc::clone(map.entry(device_id.to_string()).or_default());
    counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .wrapping_add(1)
}

fn get_conn(state: &ConnManager, device_id: &str) -> Result<Arc<DeviceConn>, String> {
    state
        .get_device(device_id)
        .ok_or_else(|| "设备未连接".to_string())
}

// ---------------------------------------------------------------------------
// Tauri 命令（签名与 lib.rs 注册一致）
// ---------------------------------------------------------------------------

/// 鼠标移动：QUIC datagram，fire-and-forget（发送失败仅报错给前端吞掉，
/// 下一帧移动立即覆盖，无需重试）。
#[tauri::command]
pub async fn mouse_move(
    device_id: String,
    window_id: String,
    x: i32,
    y: i32,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let win = parse_window_id(&window_id)?;
    let conn = get_conn(&state, &device_id)?;
    let seq = next_seq(&device_id);
    conn.send_datagram(encode_mouse_move(win, x, y, seq).into())
        .map_err(|e| format!("发送鼠标移动失败：{e:#}"))
}

/// 鼠标按键（0 左 / 1 右 / 2 中 / 3 X1 / 4 X2）：控制流可靠单向。
#[tauri::command]
pub async fn mouse_button(
    device_id: String,
    window_id: String,
    x: i32,
    y: i32,
    button: u32,
    down: bool,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let win = parse_window_id(&window_id)?;
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::MouseBtn(MouseButton {
        window_id: win,
        x,
        y,
        button,
        down,
    }))
    .await
    .map_err(|e| format!("发送鼠标按键失败：{e:#}"))
}

/// 滚轮：`dx`/`dy` 为 ±120 的 wheel delta（Windows 约定符号，前端归一化）。
#[tauri::command]
pub async fn mouse_wheel(
    device_id: String,
    window_id: String,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let win = parse_window_id(&window_id)?;
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::Wheel(MouseWheel {
        window_id: win,
        x,
        y,
        dx,
        dy,
    }))
    .await
    .map_err(|e| format!("发送滚轮失败：{e:#}"))
}

/// 键盘事件：PS/2 Set 1 基本扫描码 + extended 标志 + modifiers 位图透传。
#[tauri::command]
pub async fn key_event(
    device_id: String,
    scancode: u32,
    extended: bool,
    down: bool,
    modifiers: u32,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::Key(KeyEvent {
        scancode,
        extended,
        down,
        modifiers,
    }))
    .await
    .map_err(|e| format!("发送按键失败：{e:#}"))
}

/// 文本输入：IME 组合结束的上屏文本（Agent 按 UTF-16 码元逐个注入）。
#[tauri::command]
pub async fn text_input(
    device_id: String,
    text: String,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::Text(TextInput { text }))
        .await
        .map_err(|e| format!("发送文本失败：{e:#}"))
}

// ---------------------------------------------------------------------------
// 单元测试（任务卡：datagram 编码布局 / seq per-设备自增；输入命令的消息
// 构造依赖 DeviceConn（QUIC 连接），端到端联调覆盖，本处不 mock）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- encode_mouse_move（05_api §3 帧布局）----

    #[test]
    fn mouse_move_datagram_layout_is_little_endian() {
        let buf = encode_mouse_move(0x0123456789ABCDEF, -7, 300, 42);
        assert_eq!(buf.len(), MOVE_DATAGRAM_LEN);
        assert_eq!(buf[0], 1, "type=1");
        let mut u64le = [0u8; 8];
        u64le.copy_from_slice(&buf[1..9]);
        assert_eq!(u64::from_le_bytes(u64le), 0x0123456789ABCDEF);
        let mut i32le = [0u8; 4];
        i32le.copy_from_slice(&buf[9..13]);
        assert_eq!(i32::from_le_bytes(i32le), -7);
        i32le.copy_from_slice(&buf[13..17]);
        assert_eq!(i32::from_le_bytes(i32le), 300);
        let mut u32le = [0u8; 4];
        u32le.copy_from_slice(&buf[17..21]);
        assert_eq!(u32::from_le_bytes(u32le), 42);
    }

    #[test]
    fn mouse_move_datagram_zero_values_match_reference_bytes() {
        // 参考字节：type=1 + window_id=1 + (0,0) + seq=1
        assert_eq!(
            encode_mouse_move(1, 0, 0, 1),
            vec![
                1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0
            ]
        );
    }

    #[test]
    fn mouse_move_datagram_negative_coords_encode_signed_le() {
        let buf = encode_mouse_move(2, -1, -2147483648, u32::MAX);
        let x = i32::from_le_bytes(buf[9..13].try_into().expect("x"));
        let y = i32::from_le_bytes(buf[13..17].try_into().expect("y"));
        let seq = u32::from_le_bytes(buf[17..21].try_into().expect("seq"));
        assert_eq!((x, y, seq), (-1, -2147483648, u32::MAX));
    }

    // ---- next_seq（per-设备自增）----

    #[test]
    fn seq_increments_per_device_independently() {
        // 唯一设备名避免与其他测试并行共享条目
        let a = "seq-test-device-a-022";
        let b = "seq-test-device-b-022";
        let a1 = next_seq(a);
        let a2 = next_seq(a);
        let b1 = next_seq(b);
        assert_eq!(a2, a1 + 1, "同设备单调自增");
        assert_eq!(b1, 1, "新设备从 1 起");
    }

    #[test]
    fn seq_first_value_is_one() {
        let c = "seq-test-device-c-022";
        assert_eq!(next_seq(c), 1);
        assert_eq!(next_seq(c), 2);
    }
}
