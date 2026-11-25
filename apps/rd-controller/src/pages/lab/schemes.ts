/**
 * 远程连接全方案模型（5 大方案家族 · 9 种具体通道模式）。
 *
 * 智能 + 自定义双轨共用同一份元信息：
 * 1. 局域网直连（LAN QUIC · 4K@144Hz）
 * 2. P2P 打洞（STUN / NAT 穿透）
 * 3. 自建中转（rd-relay UDP / WS+TLS + SSH 一键部署）
 * 4. 虚拟组网（EasyTier Mesh / Tailscale / ZeroTier）
 * 5. 轻量/无服务器穿透（Cloudflare Workers / frp / DERP）
 */
import type { PoolNodeKind, RealTestPlanDto, RemotePlanMode } from "../../api/lab";

export type SchemeFamilyId = "lan" | "p2p" | "relay" | "mesh" | "tunnel";

export interface SchemeFamilyMeta {
  id: SchemeFamilyId;
  title: string;
  subtitle: string;
  modes: readonly RemotePlanMode[];
  autoCapable: boolean;
}

export const SCHEME_FAMILIES: readonly SchemeFamilyMeta[] = [
  {
    id: "lan",
    title: "局域网直连",
    subtitle: "LAN QUIC · 4K@144Hz 零中转",
    modes: ["direct"],
    autoCapable: true,
  },
  {
    id: "p2p",
    title: "P2P 打洞",
    subtitle: "STUN / NAT 穿透直连",
    modes: ["punch"],
    autoCapable: true,
  },
  {
    id: "relay",
    title: "自建中转",
    subtitle: "rd-relay · 9位ID · SSH一键建",
    modes: ["relay"],
    autoCapable: true,
  },
  {
    id: "mesh",
    title: "虚拟组网",
    subtitle: "EasyTier / Tailscale / ZeroTier",
    modes: ["easytier", "tailscale", "zerotier"],
    autoCapable: true,
  },
  {
    id: "tunnel",
    title: "轻量/无服务器",
    subtitle: "CF Workers / frp / DERP",
    modes: ["cf_worker", "frp", "derp"],
    autoCapable: true,
  },
] as const;

export interface ModeMeta {
  mode: RemotePlanMode;
  family: SchemeFamilyId;
  label: string;
  shortLabel: string;
  needsRelayAddr: boolean;
  placeholder: string;
  poolKind?: PoolNodeKind;
}

export const MODE_META: Record<RemotePlanMode, ModeMeta> = {
  direct: {
    mode: "direct",
    family: "lan",
    label: "局域网 / 直连 (QUIC)",
    shortLabel: "LAN 直连",
    needsRelayAddr: false,
    placeholder: "目标局域网/公网 IP（如 192.168.1.10，免填端口）",
  },
  punch: {
    mode: "punch",
    family: "p2p",
    label: "STUN / P2P 打洞",
    shortLabel: "P2P 打洞",
    needsRelayAddr: true,
    placeholder: "STUN 地址（如 stun.chat.bilibili.com:3478）",
    poolKind: "stun",
  },
  relay: {
    mode: "relay",
    family: "relay",
    label: "自建中转 (rd-relay)",
    shortLabel: "自建中转",
    needsRelayAddr: true,
    placeholder: "中转 VPS 地址（如 1.2.3.4:21118 或 wss://relay.example.com）",
    poolKind: "relay",
  },
  easytier: {
    mode: "easytier",
    family: "mesh",
    label: "EasyTier 虚拟组网",
    shortLabel: "EasyTier",
    needsRelayAddr: false,
    placeholder: "对方 EasyTier 虚拟 IP（如 10.126.126.2）",
    poolKind: "easytier",
  },
  tailscale: {
    mode: "tailscale",
    family: "mesh",
    label: "Tailscale 跨网直连",
    shortLabel: "Tailscale",
    needsRelayAddr: false,
    placeholder: "对方 Tailscale IP（如 100.81.205.10）",
  },
  zerotier: {
    mode: "zerotier",
    family: "mesh",
    label: "ZeroTier 虚拟网段",
    shortLabel: "ZeroTier",
    needsRelayAddr: false,
    placeholder: "对方 ZeroTier 虚拟 IP（如 10.147.17.25）",
  },
  cf_worker: {
    mode: "cf_worker",
    family: "tunnel",
    label: "Cloudflare Workers 穿透",
    shortLabel: "CF Workers",
    needsRelayAddr: true,
    placeholder: "Workers 域名（如 https://rd-relay.xxx.workers.dev）",
    poolKind: "cf_worker",
  },
  frp: {
    mode: "frp",
    family: "tunnel",
    label: "frp 端口映射穿透",
    shortLabel: "frp 隧道",
    needsRelayAddr: true,
    placeholder: "frps 公网映射地址（如 frp.example.com:7000）",
    poolKind: "frp",
  },
  derp: {
    mode: "derp",
    family: "tunnel",
    label: "DERP 中继节点",
    shortLabel: "DERP",
    needsRelayAddr: true,
    placeholder: "DERP 地址（如 https://derp20b.tailscale.com）",
    poolKind: "derp",
  },
};

export interface RelayNodeItem {
  id: string;
  name: string;
  addr: string;
  kind?: PoolNodeKind;
  region?: string;
  source?: string;
  favorite?: boolean;
  lastRttMs: number | null;
  mappedAddr?: string | null;
  status: "idle" | "ok" | "fail";
}

export function inferNodeKind(node: Pick<RelayNodeItem, "kind" | "addr" | "name">): PoolNodeKind {
  if (node.kind) return node.kind;
  const a = node.addr.toLowerCase();
  const n = node.name.toLowerCase();
  if (a.includes("stun") || n.includes("stun") || a.endsWith(":3478") || a.endsWith(":19302")) {
    return "stun";
  }
  if (a.includes("workers.dev") || n.includes("worker")) {
    return "cf_worker";
  }
  if (a.includes("derp") || n.includes("derp")) {
    return "derp";
  }
  if (n.includes("frp") || a.endsWith(":7000")) {
    return "frp";
  }
  if (
    a.startsWith("tcp://") ||
    a.startsWith("udp://") ||
    a.includes("easytier") ||
    a.endsWith(":11010")
  ) {
    return "easytier";
  }
  return "relay";
}

/** 按目标特征自动推导初始多方案集合（覆盖直连/组网/中转/无服务器，开箱即测）。 */
export function buildSmartStarterPlans(
  targetKey: string,
  baseIp: string,
  defaultRelay: string,
  deviceRelay?: string | null,
): RealTestPlanDto[] {
  const ip = baseIp || "192.168.1.10";
  const isTs = ip.startsWith("100.");
  const isZt = ip.startsWith("10.147.") || ip.startsWith("172.22.");
  const primaryMode: RemotePlanMode = isTs ? "tailscale" : isZt ? "zerotier" : "direct";
  const starter: RealTestPlanDto[] = [
    {
      planId: `${targetKey}-primary`,
      planName: MODE_META[primaryMode].label,
      mode: primaryMode,
      targetAddr: ip,
      relayAddr: "",
      rounds: 3,
    },
  ];
  const relay = (deviceRelay ?? "").trim() || defaultRelay.trim();
  if (relay) {
    const isCf = relay.toLowerCase().includes("workers.dev");
    starter.push({
      planId: `${targetKey}-relay`,
      planName: isCf ? "Cloudflare Workers 中继" : "自建中转服务器",
      mode: isCf ? "cf_worker" : "relay",
      targetAddr: ip,
      relayAddr: relay,
      rounds: 3,
    });
  }
  return starter;
}

export function familyOfMode(mode: RemotePlanMode): SchemeFamilyId {
  return MODE_META[mode]?.family ?? "lan";
}
