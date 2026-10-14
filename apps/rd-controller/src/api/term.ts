/**
 * 终端命令封装（T-023 实现具体逻辑；当前骨架透传后端桩）。
 */
import { invoke } from "@tauri-apps/api/core";

export function openTerminal(): Promise<never> {
  return invoke("open_terminal");
}

export function termInput(): Promise<never> {
  return invoke("term_input");
}

export function termResize(): Promise<never> {
  return invoke("term_resize");
}
