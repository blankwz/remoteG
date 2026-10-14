/**
 * 终端命令封装（T-023）：打开/恢复终端、输入、尺寸上报。
 *
 * - `openTerminal`：Channel 先建随 invoke 传入；输出数据（裸 VT 字节，无帧
 *   前缀，与视频/瓦片不同）经 Rust `InvokeResponseBody::Raw` → onmessage 收到
 *   `ArrayBuffer`；`resumeId` 传 null 开新会话，非空则恢复（Agent 回放环形
 *   缓冲，恢复失败 Agent 回 Error{30}）；
 * - `termInput`：输入字节以 number[] 传后端（JSON invoke；键入/探测载荷小，
 *   不走 raw IPC）；传 [] 即存活探测（Agent 侧写 0 字节，无副作用）；
 * - `termResize`：单向上报（Agent 不回包），网络瞬断失败由下一次 resize 重试。
 */
import { Channel, invoke } from "@tauri-apps/api/core";

/** Agent 侧 shell 白名单（rd-term ALLOWED_SHELLS）。 */
export const TERM_SHELLS = [
  "pwsh.exe",
  "cmd.exe",
  "wsl.exe",
  "powershell.exe",
] as const;

/** “+ 终端”与 URL `?term=` 的缺省 shell。 */
export const DEFAULT_TERM_SHELL: string = TERM_SHELLS[0];

/** open_terminal 的 ack：termId 用于 resize / resume，streamId 用于输入路由。 */
export interface OpenTerminalAckDto {
  termId: string;
  streamId: number;
}

/**
 * 打开/恢复终端；输出数据异步到达 `onData`（裸 VT 字节）。
 *
 * Rust 侧先 RPC 拿 stream_id 再注册前端路由（conn.rs 对注册前输出缓存 2 s
 * 并按序补投），快照不会丢。
 */
export function openTerminal(
  deviceId: string,
  shell: string,
  cols: number,
  rows: number,
  resumeId: string | null,
  onData: (data: ArrayBuffer) => void,
): Promise<OpenTerminalAckDto> {
  const on_data = new Channel<ArrayBuffer>();
  on_data.onmessage = onData;
  return invoke("open_terminal", {
    deviceId,
    shell,
    cols,
    rows,
    resumeId,
    onData: on_data,
  });
}

/** 写终端输入字节（VT 序列原样透传；传 [] 作存活探测）。 */
export function termInput(
  deviceId: string,
  streamId: number,
  data: number[],
): Promise<void> {
  return invoke("term_input", { deviceId, streamId, data });
}

/** 上报终端尺寸（单向；失败由下一次 resize 重试，不致命）。 */
export function termResize(
  deviceId: string,
  termId: string,
  cols: number,
  rows: number,
): Promise<void> {
  return invoke("term_resize", { deviceId, termId, cols, rows });
}

/**
 * 解析 URL `?term=` / 用户输入的 shell 名为白名单值。
 *
 * 接受 `pwsh` / `pwsh.exe` 两种写法（大小写不敏感）；未知值回退缺省
 * pwsh.exe（Agent 侧白名单会拒绝范围外的值）。
 */
export function parseShell(raw: string): string {
  const name = raw.trim().toLowerCase();
  const withExt = name.endsWith(".exe") ? name : `${name}.exe`;
  return (TERM_SHELLS as readonly string[]).includes(withExt)
    ? withExt
    : DEFAULT_TERM_SHELL;
}
