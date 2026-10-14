/**
 * 输入命令封装（T-022 实现具体逻辑；当前骨架透传后端桩）。
 */
import { invoke } from "@tauri-apps/api/core";

export function mouseMove(): Promise<never> {
  return invoke("mouse_move");
}

export function mouseButton(): Promise<never> {
  return invoke("mouse_button");
}

export function mouseWheel(): Promise<never> {
  return invoke("mouse_wheel");
}

export function keyEvent(): Promise<never> {
  return invoke("key_event");
}

export function textInput(): Promise<never> {
  return invoke("text_input");
}
