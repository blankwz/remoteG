/**
 * 设备命令封装（T-019 实现具体逻辑；当前骨架透传后端桩，均返回 not implemented）。
 *
 * 参数签名由 T-019 定义时补全。
 */
import { invoke } from "@tauri-apps/api/core";

export function listDevices(): Promise<never> {
  return invoke("list_devices");
}

export function pairDevice(): Promise<never> {
  return invoke("pair_device");
}

export function removeDevice(): Promise<never> {
  return invoke("remove_device");
}

export function connectDevice(): Promise<never> {
  return invoke("connect_device");
}

export function disconnectDevice(): Promise<never> {
  return invoke("disconnect_device");
}

export function probeDevice(): Promise<never> {
  return invoke("probe_device");
}

export function setDecodeCaps(): Promise<never> {
  return invoke("set_decode_caps");
}
