/**
 * 输入命令封装（T-022）：鼠标移动 / 点击 / 滚轮 / 键盘 / 文本。
 *
 * - `mouseMove` 走 QUIC datagram（fire-and-forget，`u8 type=1, u64 window_id,
 *   i32 x, i32 y, u32 seq` 小端，seq 由 Rust 侧 per-连接自增，前端不感知）；
 * - 其余（点击 / 滚轮 / 按键 / 文本，需可靠）走控制流单向 `send`
 *   （req_id=0，Agent 无应答，不走 RPC 空等）；
 * - window_id 沿用约定：十进制字符串（u64 保精度），Rust 侧解析；
 * - 调用方（InputMapper 接线层）自行 `.catch(() => undefined)` 吞错：
 *   输入丢失不弹错误，连接断开由会话层遮罩与重连兜底。
 */
import { invoke } from "@tauri-apps/api/core";

/** 鼠标移动（datagram，fire-and-forget）。坐标为远端窗口物理像素。 */
export function mouseMove(
  deviceId: string,
  windowId: string,
  x: number,
  y: number,
): Promise<void> {
  return invoke("mouse_move", { deviceId, windowId, x, y });
}

/** 鼠标按键（控制流可靠）。`button` 为协议编号：0 左 / 1 右 / 2 中 / 3 X1 / 4 X2。 */
export function mouseButton(
  deviceId: string,
  windowId: string,
  x: number,
  y: number,
  button: number,
  down: boolean,
): Promise<void> {
  return invoke("mouse_button", { deviceId, windowId, x, y, button, down });
}

/** 滚轮（控制流可靠）。`dx`/`dy` 为 ±120 的 wheel delta（Windows 约定符号）。 */
export function mouseWheel(
  deviceId: string,
  windowId: string,
  x: number,
  y: number,
  dx: number,
  dy: number,
): Promise<void> {
  return invoke("mouse_wheel", { deviceId, windowId, x, y, dx, dy });
}

/** 键盘事件（控制流可靠）。扫描码为 PS/2 Set 1 基本码 + extended 标志。 */
export function keyEvent(
  deviceId: string,
  scancode: number,
  extended: boolean,
  down: boolean,
  modifiers: number,
): Promise<void> {
  return invoke("key_event", { deviceId, scancode, extended, down, modifiers });
}

/** 文本输入（控制流可靠）。IME 组合结束的上屏文本。 */
export function textInput(deviceId: string, text: string): Promise<void> {
  return invoke("text_input", { deviceId, text });
}
