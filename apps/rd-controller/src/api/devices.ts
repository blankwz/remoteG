/**
 * 设备命令封装（T-019）：列表 / 6 位码配对 / 连接 / 断开 / 探测 / 解码能力上报。
 *
 * DTO 字段为 camelCase（Rust 侧 `#[serde(rename_all = "camelCase")]`）。
 */
import { invoke } from "@tauri-apps/api/core";

/** Agent 默认监听端口（地址省略端口时由 Rust 侧补全）。 */
export const AGENT_DEFAULT_PORT = 21118;

/** 已配对设备（含在线状态）。 */
export interface DeviceDto {
  id: string;
  name: string;
  addr: string;
  /** SHA-256(证书 DER) 的 64 字符小写 hex。 */
  fingerprintHex: string;
  /** 加入时间（Unix 秒）。 */
  addedAt: number;
  online: boolean;
}

/** connect_device 返回的对端能力。 */
export interface HelloAckDto {
  protoVer: number;
  hostName: string;
  encodeCaps: string[];
}

/** 本机解码能力（前端探测后上报，连接握手时填入 Hello）。 */
export interface DecodeCapsDto {
  h264Baseline420: boolean;
  h264High444: boolean;
}

export function listDevices(): Promise<DeviceDto[]> {
  return invoke("list_devices");
}

export function pairDevice(addr: string, code: string, name: string): Promise<DeviceDto> {
  return invoke("pair_device", { addr, code, name });
}

export function removeDevice(id: string): Promise<void> {
  return invoke("remove_device", { id });
}

export function connectDevice(id: string): Promise<HelloAckDto> {
  return invoke("connect_device", { id });
}

export function disconnectDevice(id: string): Promise<void> {
  return invoke("disconnect_device", { id });
}

export function probeDevice(id: string): Promise<boolean> {
  return invoke("probe_device", { id });
}

export function setDecodeCaps(caps: DecodeCapsDto): Promise<void> {
  return invoke("set_decode_caps", { caps });
}

/**
 * WebCodecs 解码能力探测（应用启动时调用一次）：
 * - H.264 Baseline 4:2:0：avc1.42E033（兜底能力，默认视为支持）；
 * - H.264 High 4:4:4：avc1.F40033（supports_444 探测）。
 */
export async function probeDecodeCaps(): Promise<DecodeCapsDto> {
  const [baseline, high444] = await Promise.all([
    VideoDecoder.isConfigSupported({ codec: "avc1.42E033" }),
    VideoDecoder.isConfigSupported({ codec: "avc1.F40033" }),
  ]);
  return {
    h264Baseline420: baseline.supported === true,
    h264High444: high444.supported === true,
  };
}
