/**
 * 窗口命令封装（T-020）：列表 / 顶层窗口事件订阅。
 *
 * DTO 字段为 camelCase（Rust 侧 `#[serde(rename_all = "camelCase")]`）。
 * window_id 以十进制字符串传递（u64 可能超出 JS number 安全整数范围，
 * 且路由参数 `#/session/:id?open=<windowId>` 本就是字符串）。
 */
import { Channel } from "@tauri-apps/api/core";
import { call, isTauriRuntime } from "./bridge";

/** 可捕获窗口条目（列表页卡片）。 */
export interface WindowDto {
  /** 窗口句柄（hwnd）的十进制字符串。 */
  id: string;
  title: string;
  /** 进程名（如 explorer.exe）。 */
  process: string;
  pid: number;
  /** 图标 PNG 的 data URL；无图标为 null（前端显示占位）。 */
  icon: string | null;
  x: number;
  y: number;
  w: number;
  h: number;
  dpi: number;
  resizable: boolean;
  minimized: boolean;
  /** 窗口外框屏幕矩形（全局物理像素；块 2 轻点关闭核对用，前端不消费）。
   * 可选：mock/旧 fixture 无此字段。 */
  frameX?: number;
  frameY?: number;
  frameW?: number;
  frameH?: number;
  /** 标题栏关闭按钮屏幕矩形（全局；null/缺省 = 无按钮/不可定位）。 */
  closeButton?: { x: number; y: number; w: number; h: number } | null;
}

/** 顶层窗口事件种类（proto `WindowEvent.Kind` 枚举名）。 */
export type WindowEventKind =
  | "CREATED"
  | "DESTROYED"
  | "MOVED"
  | "TITLE"
  | "CHILD_ADDED"
  | "CHILD_REMOVED";

/** 顶层窗口事件（子窗口事件已在 Rust 侧过滤，不会到达前端）。 */
export interface WindowEventDto {
  kind: WindowEventKind;
  /** 事件携带的最新窗口信息；DESTROYED 等事件可能缺省（null）。 */
  info: WindowDto | null;
  /** 窗口组根的窗口 id（十进制字符串；"0" = 无组）。 */
  rootId: string;
}

/** 列出被控端可捕获窗口。 */
export function listWindows(deviceId: string): Promise<WindowDto[]> {
  return call("list_windows", { deviceId });
}

/**
 * 订阅该设备的顶层窗口事件流；事件异步到达 `onEvent`。
 *
 * 订阅任务在页面卸载（Channel 被回收）或设备断开时由 Rust 侧自动结束，
 * 前端无需显式取消。
 */
export function subscribeWindowEvents(
  deviceId: string,
  onEvent: (event: WindowEventDto) => void,
): Promise<() => void> {
  if (!isTauriRuntime()) {
    return Promise.resolve(() => undefined);
  }
  const on_event = new Channel<WindowEventDto>();
  on_event.onmessage = onEvent;
  return call("subscribe_window_events", { deviceId, onEvent: on_event });
}
