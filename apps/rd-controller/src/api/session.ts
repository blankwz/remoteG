/**
 * 会话命令封装（T-021）：窗口打开/关闭、视口上报、窗口组事件订阅、
 * 调试 PNG 导出（写入下载目录）。
 *
 * - window_id 沿用 T-020 约定：十进制字符串（u64 保精度）；
 * - 帧经 `Channel` 到达：Rust 侧 `InvokeResponseBody::Raw` → onmessage 收到
 *   `ArrayBuffer`，首字节为帧类型（1=视频 2=瓦片 3=失效，见 TileLayer.ts）；
 * - PNG 导出直接调用 tauri-plugin-fs 的 `write_file`（capabilities 已授权
 *   `fs:allow-download-write-recursive`），原始字节走 IPC raw body，
 *   不经 JSON 数组展开；下载目录经 core path 插件解析。
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import { downloadDir } from "@tauri-apps/api/path";

/** 适配模式（proto FitMode 的字符串形式）。 */
export type FitMode = "FIT" | "SCALE" | "NATIVE";

/** 可用缩放倍率（任务卡步骤 6：100% / 150% / 200%）。 */
export const ZOOM_LEVELS = [1, 1.5, 2] as const;

/** open_window 的 ack。流 id 仅透传展示（帧按帧头 window_id 路由）。 */
export interface OpenWindowAckDto {
  /** 窗口句柄的十进制字符串。 */
  windowId: string;
  /** RenderLevel 枚举名（M1 恒 L0_PIXEL）。 */
  level: string;
  /** Codec 枚举名（HUD 显示）。 */
  codec: string;
  w: number;
  h: number;
  videoStreamId: number;
  tileStreamId: number;
}

/** Agent 码率自适应统计（StatsReport，连接级）。 */
export interface StatsDto {
  fps: number;
  rttMs: number;
  kbps: number;
  decodeMs: number;
}

/** 窗口组内的窗口条目（子窗口或根）。 */
export interface GroupWindowDto {
  id: string;
  title: string;
  process: string;
  pid: number;
  /** 屏幕坐标（前端相对根原点换算偏移）。 */
  x: number;
  y: number;
  w: number;
  h: number;
  dpi: number;
  resizable: boolean;
}

/** 窗口组事件种类。 */
export type GroupEventKind =
  | "CHILD_ADDED"
  | "CHILD_REMOVED"
  | "MOVED"
  | "TITLE"
  | "DESTROYED"
  | "FIT_DEGRADED"
  | "STATS";

/** 窗口组事件。 */
export interface GroupEventDto {
  kind: GroupEventKind;
  /** 事件携带的窗口信息；FIT_DEGRADED / STATS 为 null。 */
  info: GroupWindowDto | null;
  /** FIT_DEGRADED 的提示文案。 */
  message: string | null;
  /** STATS 的统计值。 */
  stats: StatsDto | null;
  /** 事件所属窗口组根（十进制字符串）。 */
  rootId: string;
}

/**
 * 打开窗口会话（先注册帧路由再 RPC OpenWindow，Rust 侧保证顺序）。
 *
 * `maxFps` 传 0 表示未指定（Agent 用配置默认值）；帧异步到达 `onFrame`。
 */
export function openWindow(
  deviceId: string,
  windowId: string,
  fit: FitMode,
  maxFps: number,
  onFrame: (frame: ArrayBuffer) => void,
): Promise<OpenWindowAckDto> {
  const on_frame = new Channel<ArrayBuffer>();
  on_frame.onmessage = onFrame;
  return invoke("open_window", { deviceId, windowId, fit, maxFps, onFrame: on_frame });
}

/** 关闭窗口会话（撤销帧路由 + 单向 CloseWindow；重复关闭安全）。 */
export function closeWindow(deviceId: string, windowId: string): Promise<void> {
  return invoke("close_window", { deviceId, windowId });
}

/** 上报视口（Fit 1:1 适配；`dpr` 传 devicePixelRatio / zoom）。 */
export function setViewport(
  deviceId: string,
  windowId: string,
  cssW: number,
  cssH: number,
  dpr: number,
): Promise<void> {
  return invoke("set_viewport", { deviceId, windowId, cssW, cssH, dpr });
}

/**
 * 订阅窗口组事件流；事件异步到达 `onEvent`。
 *
 * 订阅任务在 Channel 被回收或设备断开时由 Rust 侧自动结束。
 */
export function subscribeGroupEvents(
  deviceId: string,
  rootId: string,
  onEvent: (event: GroupEventDto) => void,
): Promise<void> {
  const on_event = new Channel<GroupEventDto>();
  on_event.onmessage = onEvent;
  return invoke("subscribe_group_events", { deviceId, rootId, onEvent: on_event });
}

/**
 * 把 PNG 字节写入系统下载目录，返回完整路径（调试导出 Ctrl+Shift+S 用）。
 *
 * 直接调用 `plugin:fs|write_file`：payload 为 raw 字节，path/options 走
 * IPC headers（与 tauri-plugin-fs 官方 JS 封装的调用形态一致，避免为导出
 * 单帧 PNG 引入 plugin-fs npm 依赖）。
 */
export async function savePngToDownloads(fileName: string, png: Uint8Array): Promise<string> {
  const dir = await downloadDir();
  const path = `${dir}/${fileName}`;
  await invoke("plugin:fs|write_file", png, {
    headers: {
      path: encodeURIComponent(path),
      options: JSON.stringify({}),
    },
  });
  return path;
}
