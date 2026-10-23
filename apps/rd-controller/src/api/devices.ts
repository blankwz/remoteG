/**
 * 设备命令封装（T-019）：列表 / 6 位码配对 / 连接 / 断开 / 探测 / 解码能力上报。
 *
 * DTO 字段为 camelCase（Rust 侧 `#[serde(rename_all = "camelCase")]`）。
 */
import { invoke } from "@tauri-apps/api/core";

/** Agent 默认监听端口（地址省略端口时由 Rust 侧补全）。 */
export const AGENT_DEFAULT_PORT = 21118;

/** 控制端自身版本（vite define 注入 package.json version）。 */
declare const __APP_VERSION__: string;
export const APP_VERSION = __APP_VERSION__;

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
  /** 被控端应用版本（v0.1，版本一致性排查用）。 */
  hostVersion: string;
}

// ---------------------------------------------------------------------------
// 被控端版本记录（连接成功时写入；设备卡/会话 HUD 展示，版本一致性排查）
// ---------------------------------------------------------------------------

const HOST_VERSION_PREFIX = "rd.hostVersion.";

/** 记录某设备的被控端版本（connect 成功时调用）。 */
export function rememberHostVersion(deviceId: string, version: string): void {
  try {
    window.localStorage.setItem(HOST_VERSION_PREFIX + deviceId, version);
  } catch {
    // 存储不可用：仅失去版本展示，不影响功能
  }
}

/** 读取某设备的被控端版本；未连接过返回 null。 */
export function getHostVersion(deviceId: string): string | null {
  try {
    return window.localStorage.getItem(HOST_VERSION_PREFIX + deviceId);
  } catch {
    return null;
  }
}

/** UDP 局域网发现的设备（未配对，仅提供"添加设备"建议地址）。 */
export interface DiscoveredDeviceDto {
  name: string;
  addr: string;
  fingerprintShortHex: string;
}

/** 本机解码能力（前端探测后上报，连接握手时填入 Hello）。 */
export interface DecodeCapsDto {
  h264Baseline420: boolean;
  h264High444: boolean;
}

// ---------------------------------------------------------------------------
// 最后连接的设备（导航栏「窗口列表 / 会话」入口指向它；localStorage 持久化）
// ---------------------------------------------------------------------------

const LAST_DEVICE_KEY = "rd.lastDeviceId";

/** 记住最后连接的设备 id（连接成功 / 窗口列表装载成功时调用）。 */
export function rememberLastDeviceId(id: string): void {
  try {
    window.localStorage.setItem(LAST_DEVICE_KEY, id);
  } catch {
    // 存储不可用（隐私模式等）：仅失去导航快捷入口，不影响功能
  }
}

/** 读取最后连接的设备 id；从未连接过返回 null。 */
export function getLastDeviceId(): string | null {
  try {
    const id = window.localStorage.getItem(LAST_DEVICE_KEY);
    return id !== null && id.length > 0 ? id : null;
  } catch {
    return null;
  }
}

/** 清除记忆（删除该设备时调用，避免导航指向不存在的设备）。 */
export function clearLastDeviceId(): void {
  try {
    window.localStorage.removeItem(LAST_DEVICE_KEY);
  } catch {
    // 同上，忽略
  }
}

export function listDevices(): Promise<DeviceDto[]> {
  return invoke("list_devices");
}

/** 广播扫描局域网内的 rd-agent（约 1.5s，发现≠信任）。 */
export function discoverDevices(): Promise<DiscoveredDeviceDto[]> {
  return invoke("discover_devices");
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
