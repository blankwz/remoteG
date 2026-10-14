/**
 * 窗口命令封装（T-020 实现具体逻辑；当前骨架透传后端桩）。
 */
import { invoke } from "@tauri-apps/api/core";

export function listWindows(): Promise<never> {
  return invoke("list_windows");
}

export function subscribeWindowEvents(): Promise<never> {
  return invoke("subscribe_window_events");
}
