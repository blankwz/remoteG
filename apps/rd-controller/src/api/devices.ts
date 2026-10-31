/**
 * 设备命令封装（T-019）：列表 / 6 位码配对 / 连接 / 断开 / 探测 / 解码能力上报。
 *
 * DTO 字段为 camelCase（Rust 侧 `#[serde(rename_all = "camelCase")]`）。
 */
import { call } from "./bridge";

/** Agent 默认监听端口（地址省略端口时由 Rust 侧补全）。 */
export const AGENT_DEFAULT_PORT = 21118;

/** 控制端自身版本（vite define 注入 package.json version）。 */
declare const __APP_VERSION__: string;
export const APP_VERSION = __APP_VERSION__;

// ---------------------------------------------------------------------------
// 融合模式被控状态（D-013 包5 UI 化）
// ---------------------------------------------------------------------------

/** 被控端在线状态（本机=被控机）。 */
export interface HostStatusDto {
  listenAddrs: string[];
  peers: { name: string; sinceSecs: number }[];
  paused: boolean;
}

/** 配对码 + 建议地址。 */
export interface PairCodeDto {
  code: string;
  addrs: string[];
  paused: boolean;
}

export function hostStatus(): Promise<HostStatusDto> {
  return call("host_status");
}

export function generatePairCode(): Promise<PairCodeDto> {
  return call("generate_pair_code");
}

/** 编辑已配对设备（名称/地址）。 */
export function editDevice(
  id: string,
  name: string,
  addr: string,
): Promise<DeviceDto> {
  return call("edit_device", { id, name, addr });
}

/** Tailscale 链路测试（直达 or DERP 中继 + 逐跳延迟）。 */
export function tailscalePing(ip: string): Promise<string> {
  return call("tailscale_ping", { ip });
}

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
  return call("list_devices");
}

/** 广播扫描局域网内的 rd-agent（约 1.5s，发现≠信任）。 */
export function discoverDevices(): Promise<DiscoveredDeviceDto[]> {
  return call("discover_devices");
}

export function pairDevice(addr: string, code: string, name: string): Promise<DeviceDto> {
  return call("pair_device", { addr, code, name });
}

export function removeDevice(id: string): Promise<void> {
  return call("remove_device", { id });
}

export function connectDevice(id: string): Promise<HelloAckDto> {
  return call("connect_device", { id });
}

export function disconnectDevice(id: string): Promise<void> {
  return call("disconnect_device", { id });
}

export function probeDevice(id: string): Promise<boolean> {
  return call("probe_device", { id });
}

export function setDecodeCaps(caps: DecodeCapsDto): Promise<void> {
  return call("set_decode_caps", { caps });
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

// ---------------------------------------------------------------------------
// 连接历史（R12 设计#3：设备行「上次连接」时间感；localStorage 先例 lastDeviceId）
// ---------------------------------------------------------------------------

const CONNECT_LOG_KEY = "rd.connectLog";
const CONNECT_LOG_MAX = 50;

interface ConnectLogEntry {
  readonly id: string;
  readonly ts: number;
}

function readConnectLog(): readonly ConnectLogEntry[] {
  try {
    const raw = window.localStorage.getItem(CONNECT_LOG_KEY);
    if (raw === null) {
      return [];
    }
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as ConnectLogEntry[]) : [];
  } catch {
    return [];
  }
}

/** 记录一次成功连接（进入窗口页即记；重复连同一设备只更新时刻）。 */
export function recordConnect(deviceId: string): void {
  try {
    const next = [
      { id: deviceId, ts: Date.now() },
      ...readConnectLog().filter((e) => e.id !== deviceId),
    ].slice(0, CONNECT_LOG_MAX);
    window.localStorage.setItem(CONNECT_LOG_KEY, JSON.stringify(next));
  } catch {
    // 存储不可用仅失去时间感，不致命
  }
}

/** 上次成功连接时刻；无记录返回 null。 */
export function lastConnectedAt(deviceId: string): number | null {
  return readConnectLog().find((e) => e.id === deviceId)?.ts ?? null;
}

/** 相对时间人话（刚刚/n 分钟前/n 小时前/n 天前/更早）。 */
export function relativeTime(ts: number): string {
  const secs = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (secs < 60) {
    return "刚刚";
  }
  if (secs < 3600) {
    return `${Math.floor(secs / 60)} 分钟前`;
  }
  if (secs < 86400) {
    return `${Math.floor(secs / 3600)} 小时前`;
  }
  if (secs < 86400 * 30) {
    return `${Math.floor(secs / 86400)} 天前`;
  }
  return "更早";
}
