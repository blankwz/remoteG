/**
 * 会话命令封装（T-021 实现具体逻辑；当前骨架透传后端桩）。
 */
import { invoke } from "@tauri-apps/api/core";

export function openWindow(): Promise<never> {
  return invoke("open_window");
}

export function closeWindow(): Promise<never> {
  return invoke("close_window");
}

export function setViewport(): Promise<never> {
  return invoke("set_viewport");
}

export function subscribeGroupEvents(): Promise<never> {
  return invoke("subscribe_group_events");
}
