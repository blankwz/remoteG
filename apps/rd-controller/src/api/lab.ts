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

export type RemotePlanMode =
  | "direct"
  | "punch"
  | "relay"
  | "easytier"
  | "tailscale"
  | "zerotier"
  | "cf_worker"
  | "frp"
  | "derp";

export interface RealTestPlanDto {
  planId: string;
  planName: string;
  mode: RemotePlanMode;
  targetAddr: string;
  relayAddr: string;
  rounds: number;
}

export interface RealPlanProbeResultDto {
  planId: string;
  planName: string;
  mode: string;
  resolvedAddr: string;
  ok: boolean;
  reachableCount: number;
  totalProbes: number;
  lossPct: number;
  minRttMs: number | null;
  avgRttMs: number | null;
  maxRttMs: number | null;
  jitterMs: number | null;
  nodeRttMs: number | null;
  targetRttMs: number | null;
  peerFingerprint: string | null;
  routeDetail: string;
  logs: string[];
}

export function labProbeTargetPlan(
  plan: RealTestPlanDto,
): Promise<RealPlanProbeResultDto> {
  return call("lab_probe_target_plan", { plan });
}

export function labSaveProfile(profile: LabProfile): Promise<LabProfile[]> {
  return call("lab_save_profile", { profile });
}

export function labDeleteProfile(id: string): Promise<LabProfile[]> {
  return call("lab_delete_profile", { id });
}

export type PoolNodeKind = "stun" | "easytier" | "relay" | "cf_worker" | "frp" | "derp";

export interface PublicNodeItemDto {
  name: string;
  kind: PoolNodeKind;
  addr: string;
  region: string;
  source: string;
  note: string;
}

export interface FetchPublicNodesResultDto {
  nodes: PublicNodeItemDto[];
  fetchedOnlineCount: number;
  logs: string[];
}

export interface NodeProbeResultDto {
  addr: string;
  kind: string;
  ok: boolean;
  rttMs: number | null;
  mappedAddr: string | null;
  detail: string;
}

export function labFetchPublicNodes(
  customUrl?: string,
): Promise<FetchPublicNodesResultDto> {
  return call("lab_fetch_public_nodes", { customUrl: customUrl?.trim() || null });
}

export function labProbeNode(
  kind: PoolNodeKind | "punch",
  addr: string,
): Promise<NodeProbeResultDto> {
  return call("lab_probe_node", { kind, addr });
}

export interface NatDiagnosisDto {
  natType: "cone" | "symmetric" | "unknown" | "blocked";
  label: string;
  advice: string;
  mappedA: string | null;
  mappedB: string | null;
  serverA: string;
  serverB: string;
  rttAMs: number | null;
  rttBMs: number | null;
}

export function labDiagnoseNat(stunAddrs: string[]): Promise<NatDiagnosisDto> {
  return call("lab_diagnose_nat", { stunAddrs });
}

export interface P2pHostOpenDto {
  ok: boolean;
  token: string;
  mappedAddr: string;
  natType: string;
  natLabel: string;
  pairCode: string;
  stunUsed: string;
  candidates: string[];
  advice: string;
}

export interface P2pConnectDeviceSummary {
  id: string;
  name: string;
  addr: string;
  fingerprintHex: string;
  addedAt: number;
  online: boolean;
  macAddrs: string[];
  relay: string | null;
}

export interface P2pConnectResultDto {
  ok: boolean;
  needReversePunch: boolean;
  needPairCode: boolean;
  reverseCode: string;
  device: P2pConnectDeviceSummary | null;
  routeTag: string;
  detail: string;
  ctrlNatType?: string;
  hostNatType?: string;
  txPulses?: number;
  rxPulses?: number;
  attemptCount?: number;
}

export interface P2pPunchStatusDto {
  active: boolean;
  reverseCode: string;
  ctrlNatType: string;
  txPulses: number;
  rxPulses: number;
  punchedPeer: string | null;
}

export interface MeshPeerItemDto {
  source: string;
  name: string;
  addr: string;
  latencyMs: number | null;
  cost: string;
}

export function labP2pHostOpen(stunAddr?: string): Promise<P2pHostOpenDto> {
  return call("lab_p2p_host_open", { stunAddr: stunAddr?.trim() || null });
}

export function labP2pHostPunchPeer(peerReceipt: string): Promise<string> {
  return call("lab_p2p_host_punch_peer", { peerReceipt: peerReceipt.trim() });
}

export function labP2pConnectToken(
  tokenOrAddr: string,
  code?: string,
  stunAddr?: string,
): Promise<P2pConnectResultDto> {
  return call("lab_p2p_connect_token", {
    tokenOrAddr: tokenOrAddr.trim(),
    code: code?.trim() || null,
    stunAddr: stunAddr?.trim() || null,
  });
}

export function labP2pCheckStatus(): Promise<P2pPunchStatusDto> {
  return call("lab_p2p_check_status", {});
}

export function labExportDiagnosticLog(): Promise<string> {
  return call("lab_export_diagnostic_log", {});
}

export function labScanMeshPeers(): Promise<MeshPeerItemDto[]> {
  return call("lab_scan_mesh_peers", {});
}
