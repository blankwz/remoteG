/**
 * 方案实验室（RD Geek Lab v0.4）前端 API 封装。
 *
 * 所有调用统一经 `bridge.call` 收口（遵守 INV-bridge 门禁纪律），
 * 支持在线操控真实设备与离线驱动本机真协议栈沙盒。
 */
import { Channel } from "@tauri-apps/api/core";
import { call, isTauriRuntime } from "./bridge";

export interface LabTransportConfig {
  /** "auto" | "direct" | "punch" | "relay" | "easytier" */
  routeMode: string;
  /** "bbr" | "cubic" */
  congestion: string;
  /** true = QUIC Datagram + RS-FEC; false = QUIC Stream 可靠流 */
  mediaFec: boolean;
  /** RS-FEC 冗余比例 (5 ~ 100 %) */
  fecOverheadPct: number;
}

export interface LabCodecConfig {
  manualLock: boolean;
  bitrateKbps: number;
  maxFps: number;
  preferHw: boolean;
  request444: boolean;
  tileEnabled: boolean;
  tileStillMs: number;
  tileBudgetPct: number;
}

export interface LabProfile {
  id: string;
  name: string;
  boundDeviceId: string | null;
  transport: LabTransportConfig;
  codec: LabCodecConfig;
}

export interface NetemConfigDto {
  enabled: boolean;
  delayMs: number;
  jitterMs: number;
  lossPct: number;
  burstLen: number;
  rateKbps: number;
}

export interface LabLiveStateDto {
  activeDeviceId: string | null;
  activeAddr: string | null;
  routeTag: string;
  congestion: string;
  mediaFec: boolean;
  fecOverheadPct: number;
  fecRecovered: number;
  fecLost: number;
  rttMs: number;
  quicCwndKb: number;
  quicSentPkts: number;
  quicLostPkts: number;
  encoderName: string;
  hardwareEnc: boolean;
  is444: boolean;
  targetKbps: number;
  videoKbps: number;
  tileKbps: number;
  fpsCap: number;
  tileStillMs: number;
  tileBudgetPct: number;
  tilesSent: number;
  idrCount: number;
  netem: NetemConfigDto;
  netemDropped: number;
  netemDelayed: number;
  sandboxRunning: boolean;
  sandboxPsnrDb: number;
  sandboxTextFidelityPct: number;
}

export interface LabAbMetricsDto {
  label: string;
  encoderName: string;
  avgFps: number;
  p99FrameMs: number;
  videoKbps: number;
  tileKbps: number;
  fecRecovered: number;
  framesDropped: number;
  psnrDb: number;
  textFidelityPct: number;
}

export interface LabAbResultDto {
  netemSummary: string;
  runA: LabAbMetricsDto;
  runB: LabAbMetricsDto;
  winner: string;
  verdict: string;
}

export function labGetState(deviceId?: string | null): Promise<LabLiveStateDto> {
  return call("lab_get_state", { deviceId: deviceId ?? null });
}

export function labApplyTuning(
  deviceId: string | null,
  codec: LabCodecConfig,
  fecOverheadPct: number,
  forceIdr: boolean,
): Promise<void> {
  return call("lab_apply_tuning", {
    deviceId,
    codec,
    fecOverheadPct,
    forceIdr,
  });
}

export function labApplyTransport(
  deviceId: string | null,
  transport: LabTransportConfig,
): Promise<string> {
  return call("lab_apply_transport", { deviceId, transport });
}

export function labSetNetem(config: NetemConfigDto): Promise<NetemConfigDto> {
  return call("lab_set_netem", { config });
}

export function labPanicReset(deviceId?: string | null): Promise<void> {
  return call("lab_panic_reset", { deviceId: deviceId ?? null });
}

export function labStartSandbox(
  onFrame: (frame: ArrayBuffer) => void,
): Promise<void> {
  if (!isTauriRuntime() || !(window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__) {
    void onFrame;
    return Promise.resolve();
  }
  const ch = new Channel<ArrayBuffer>();
  ch.onmessage = onFrame;
  return call("lab_start_sandbox", { onFrame: ch });
}

export function labStopSandbox(): Promise<void> {
  return call("lab_stop_sandbox", {});
}

export interface LabRankedRouteDto {
  profileId: string;
  routeMode: string;
  score: number;
  metrics: LabAbMetricsDto;
}

export interface LabAllRoutesBenchDto {
  netemSummary: string;
  ranked: LabRankedRouteDto[];
  bestProfileId: string;
  bestName: string;
  verdict: string;
}

export function labRunAbBench(
  profileA: LabProfile,
  profileB: LabProfile,
): Promise<LabAbResultDto> {
  return call("lab_run_ab_bench", { profileA, profileB });
}

export function labBenchmarkAllRoutes(
  profiles: LabProfile[],
): Promise<LabAllRoutesBenchDto> {
  return call("lab_benchmark_all_routes", { profiles });
}

export function labListProfiles(): Promise<LabProfile[]> {
  return call("lab_list_profiles", {});
}

export function labSaveProfile(profile: LabProfile): Promise<LabProfile[]> {
  return call("lab_save_profile", { profile });
}

export function labDeleteProfile(id: string): Promise<LabProfile[]> {
  return call("lab_delete_profile", { id });
}
