/**
 * RD Geek Lab · 方案实验室（分场景三步闭环实战向导 + 画质底层调优台）。
 *
 * 彻底解决「不会建方案、不会用打洞/虚拟组网、拉取并绑定公共 STUN 后不知道怎么走下去」的痛点：
 * 1) 顶层五大场景卡片点击即展开对应的【1 → 2 → 3 步可走通实战向导】：
 *    - 局域网直连：一键扫描同网段电脑 → 实测 QUIC 延迟 → 配对直入 4K@144Hz 桌面
 *    - P2P 打洞（零服务器公共 STUN）：自动优选国内最快 STUN + NAT 体检 → 被控端一键生成 `rd-p2p://` 打洞码（含同端口保活与双向开洞脉冲）→ 控制端粘贴直连桌面
 *    - 虚拟组网（EasyTier / Tailscale / ZeroTier）：一键装内核 → 免重启热启动组网口令房间 → 扫描在线虚拟 IP 成员 → 点一下连接桌面
 *    - 自建中转（rd-relay · 9位ID · SSH一键建）：保存即免重启热上线 → 本机 9 位 ID 展示 → 输对方 9 位 ID 自动打洞+中转直入桌面
 *    - 轻量穿透（frp / CF Workers / DERP）：填公网映射地址 → 实测连通 → 保存方案并直连桌面
 * 2) 节点池智能导流：STUN 节点一键送入「P2P 打洞向导」，EasyTier 节点送入「虚拟组网向导」，绝不再把公共 STUN 误绑成死胡同。
 * 3) 全链路内联配对闭环：任何方案或陌生 IP 点「连接桌面」时，若尚未配对，自动弹出内联 6 位配对码输入框，输完直达完整远程桌面。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  labDiagnoseNat,
  labExportDiagnosticLog,
  labFetchPublicNodes,
  labP2pCheckStatus,
  labP2pConnectToken,
  labP2pHostOpen,
  labP2pHostPunchPeer,
  labProbeNode,
  labProbeTargetPlan,
  labScanMeshPeers,
  type MeshPeerItemDto,
  type NatDiagnosisDto,
  type P2pHostOpenDto,
  type PoolNodeKind,
  type RealPlanProbeResultDto,
  type RealTestPlanDto,
  type RemotePlanMode,
} from "../api/lab";
import {
  connectByRelayId,
  connectDevice,
  discoverDevices,
  easytierAutoInstall,
  easytierDetectStatus,
  easytierGenerateInvite,
  easytierJoinInvite,
  editDevice,
  getRelayAddr,
  labConnectDirectOrMesh,
  listDevices,
  rememberHostVersion,
  rememberLastDeviceId,
  relayIdStatus,
  relayResolveId,
  setRelayAddr,
  type DeviceDto,
  type DiscoveredDeviceDto,
  type EasyTierStatusDto,
  type RelayIdStatusDto,
  type RelayResolveIdResultDto,
} from "../api/devices";
import { call } from "../api/bridge";
import {
  deployRelaySsh,
  normalizeSshInput,
  type SshDeployResult,
} from "../api/relayDeploy";
import { GeekTuningDeck } from "./lab/GeekTuningDeck";
import {
  buildSmartStarterPlans,
  familyOfMode,
  inferNodeKind,
  MODE_META,
  SCHEME_FAMILIES,
  type RelayNodeItem,
  type SchemeFamilyId,
} from "./lab/schemes";
import styles from "./Lab.module.css";

interface LabProps {
  /** 指定操控的设备 ID；省略时自动附着当前活跃设备。 */
  deviceId?: string | null;
  /** 是否作为 Session 内嵌极客抽屉展示（抽屉模式默认直达画质调优台）。 */
  drawer?: boolean;
  /** 抽屉模式下的关闭回调。 */
  onClose?: () => void;
}

const DESKTOP_PRIMARY_ID = "18446462598732840960";

const ALL_MODES: readonly RemotePlanMode[] = [
  "direct",
  "punch",
  "relay",
  "easytier",
  "tailscale",
  "zerotier",
  "cf_worker",
  "frp",
  "derp",
];

type ToolDrawerId = "none" | "pool" | "ssh" | "logs";

/** 待补填 6 位配对码的挂起连接上下文（任意通道首次连接未信任设备时触发）。 */
type PendingPairContext =
  | {
      kind: "direct";
      targetAddr: string;
      relayAddr?: string;
      name?: string;
      prompt: string;
    }
  | {
      kind: "relay_id";
      relayAddr: string;
      numericId: string;
      name?: string;
      prompt: string;
    }
  | {
      kind: "p2p";
      tokenOrAddr: string;
      stunAddr?: string;
      prompt: string;
    };

async function copyTextSafe(text: string): Promise<void> {
  try {
    await call<void>("local_clipboard_write", { text });
    return;
  } catch {
    // fallback to navigator.clipboard in browser preview
  }
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // ignore
  }
}

export function Lab({ deviceId, drawer = false, onClose }: LabProps) {
  /** 顶层视图分层：独立页默认「智能连接中心」，会话抽屉默认「画质与传输调优」。 */
  const [activeLayer, setActiveLayer] = useState<"connectivity" | "tuning">(
    drawer ? "tuning" : "connectivity",
  );
  const [toast, setToast] = useState<string | null>(null);
  const showToast = useCallback((msg: string) => {
    setToast(msg);
    window.setTimeout(() => {
      setToast((cur) => (cur === msg ? null : cur));
    }, 3800);
  }, []);

  // ── 目标设备与全局状态 ──
  const [savedDevices, setSavedDevices] = useState<DeviceDto[]>([]);
  const [targetDeviceId, setTargetDeviceId] = useState<string>(deviceId ?? "");
  const [customIp, setCustomIp] = useState<string>("");
  const [defaultRelayAddr, setDefaultRelayAddr] = useState<string>("");
  const [connectingBusy, setConnectingBusy] = useState(false);

  // ── 当前展开的五大实战向导场景 & 方案过滤 & 底部工具抽屉 ──
  const [activeGuide, setActiveGuide] = useState<SchemeFamilyId>("p2p");
  const [onlyFilterFamily, setOnlyFilterFamily] = useState(false);
  const [openDrawer, setOpenDrawer] = useState<ToolDrawerId>("none");

  // ── 统一内联 6 位配对码补填状态 ──
  const [pendingPair, setPendingPair] = useState<PendingPairContext | null>(null);
  const [pairCodeInput, setPairCodeInput] = useState("");

  // ── 按目标持久化的多绑定方案 ──
  const [plansByTarget, setPlansByTarget] = useState<Record<string, RealTestPlanDto[]>>(() => {
    try {
      const raw = window.localStorage.getItem("rd.labTargetPlans");
      if (raw) return JSON.parse(raw) as Record<string, RealTestPlanDto[]>;
    } catch {
      // ignore
    }
    return {};
  });
  const [confirmedPlanByTarget, setConfirmedPlanByTarget] = useState<Record<string, string>>(
    () => {
      try {
        const raw = window.localStorage.getItem("rd.labConfirmedPlans");
        if (raw) return JSON.parse(raw) as Record<string, string>;
      } catch {
        // ignore
      }
      return {};
    },
  );

  // ── 新增 / 编辑自定义方案表单 ──
  const [showAddPlan, setShowAddPlan] = useState(false);
  const [editingPlanId, setEditingPlanId] = useState<string | null>(null);
  const [newPlanName, setNewPlanName] = useState("");
  const [newPlanMode, setNewPlanMode] = useState<RemotePlanMode>("direct");
  const [newPlanAddr, setNewPlanAddr] = useState("");
  const [newPlanRelay, setNewPlanRelay] = useState("");

  // ── 真实测速与智能寻优状态 ──
  const [realResults, setRealResults] = useState<Record<string, RealPlanProbeResultDto>>({});
  const [testingPlanId, setTestingPlanId] = useState<string | null>(null);
  const [runningBatch, setRunningBatch] = useState(false);
  const [smartRacing, setSmartRacing] = useState(false);
  const [smartHint, setSmartHint] = useState<{
    level: "ok" | "suggest" | "warn";
    title: string;
    detail: string;
    bestPlan?: RealTestPlanDto;
    guideTarget?: SchemeFamilyId;
  } | null>(null);

  // ── 共用节点池（STUN / EasyTier / Relay / CF Workers / frp / DERP） ──
  const [serverPool, setServerPool] = useState<RelayNodeItem[]>(() => {
    try {
      const raw = window.localStorage.getItem("rd.labServerPool");
      if (raw) return JSON.parse(raw) as RelayNodeItem[];
    } catch {
      // ignore
    }
    return [];
  });
  const saveServerPool = useCallback((next: RelayNodeItem[]) => {
    setServerPool(next);
    try {
      window.localStorage.setItem("rd.labServerPool", JSON.stringify(next));
    } catch {
      // ignore
    }
  }, []);
  const [poolFilter, setPoolFilter] = useState<"all" | PoolNodeKind>("all");
  const [fetchingPublic, setFetchingPublic] = useState(false);
  const [customSubUrl, setCustomSubUrl] = useState(() => {
    try {
      return window.localStorage.getItem("rd.labCustomSubUrl") ?? "";
    } catch {
      return "";
    }
  });
  const [batchKind, setBatchKind] = useState<"auto" | PoolNodeKind>("auto");
  const [batchText, setBatchText] = useState("");
  const [probingPool, setProbingPool] = useState(false);
  const [probingNodeId, setProbingNodeId] = useState<string | null>(null);

  // ── 场景 1：局域网直连状态 ──
  const [lanScanning, setLanScanning] = useState(false);
  const [lanFound, setLanFound] = useState<DiscoveredDeviceDto[]>([]);
  const [lanIpInput, setLanIpInput] = useState("");
  const [lanPairCode, setLanPairCode] = useState("");

  // ── 场景 2：零服务器公共 STUN P2P 打洞向导状态 ──
  const [selectedStunAddr, setSelectedStunAddr] = useState<string>("stun.miwifi.com:3478");
  const [natBusy, setNatBusy] = useState(false);
  const [natResult, setNatResult] = useState<NatDiagnosisDto | null>(null);
  const [p2pHostBusy, setP2pHostBusy] = useState(false);
  const [p2pHostInfo, setP2pHostInfo] = useState<P2pHostOpenDto | null>(null);
  const [p2pReceiptInput, setP2pReceiptInput] = useState("");
  const [p2pPunchingPeer, setP2pPunchingPeer] = useState(false);
  const [p2pTokenInput, setP2pTokenInput] = useState("");
  const [p2pCtrlBusy, setP2pCtrlBusy] = useState(false);
  const [p2pReverseCode, setP2pReverseCode] = useState<string | null>(null);
  const [p2pLiveStatus, setP2pLiveStatus] = useState<{
    txPulses: number;
    rxPulses: number;
    ctrlNatType: string;
    hostNatType: string;
    attemptCount: number;
    detail: string;
    punchedPeer: string | null;
  } | null>(null);

  // ── 场景 3：虚拟组网（EasyTier + Tailscale + ZeroTier）状态 ──
  const [etStatus, setEtStatus] = useState<EasyTierStatusDto | null>(null);
  const [etBusy, setEtBusy] = useState(false);
  const [etInviteCode, setEtInviteCode] = useState("");
  const [etJoinInput, setEtJoinInput] = useState("");
  const [selectedEtBootstrap, setSelectedEtBootstrap] = useState<string>("");
  const [meshPeers, setMeshPeers] = useState<MeshPeerItemDto[]>([]);
  const [scanningMesh, setScanningMesh] = useState(false);
  const [meshManualIp, setMeshManualIp] = useState("");

  // ── 场景 4：自建中转（rd-relay · 9位ID · SSH一键部署）状态 ──
  const [relayInputAddr, setRelayInputAddr] = useState("");
  const [savingRelay, setSavingRelay] = useState(false);
  const [relayIdInfo, setRelayIdInfo] = useState<RelayIdStatusDto | null>(null);
  const [peerNumericId, setPeerNumericId] = useState("");
  const [relayPairCode, setRelayPairCode] = useState("");
  const [relayLookupRes, setRelayLookupRes] = useState<RelayResolveIdResultDto | null>(null);
  const [showInlineSsh, setShowInlineSsh] = useState(false);
  const [sshName, setSshName] = useState("");
  const [sshHost, setSshHost] = useState("");
  const [sshPort, setSshPort] = useState("22");
  const [sshUser, setSshUser] = useState("root");
  const [sshKeyPath, setSshKeyPath] = useState("");
  const [sshMode, setSshMode] = useState<"udp" | "docker">("udp");
  const [sshDomain, setSshDomain] = useState("");
  const [sshBusy, setSshBusy] = useState(false);
  const [sshResult, setSshResult] = useState<SshDeployResult | null>(null);

  // ── 场景 5：轻量/无服务器穿透（frp / CF Workers / DERP）状态 ──
  const [tunnelMode, setTunnelMode] = useState<"frp" | "cf_worker" | "derp">("frp");
  const [tunnelAddrInput, setTunnelAddrInput] = useState("");
  const [tunnelProbeText, setTunnelProbeText] = useState<string | null>(null);

  // ── 实时探测日志 ──
  const [eventLogs, setEventLogs] = useState<
    { id: number; time: string; level: "info" | "ok" | "warn"; text: string }[]
  >(() => [
    {
      id: 1,
      time: new Date().toLocaleTimeString("zh-CN", { hour12: false }),
      level: "info",
      text: "实战向导就绪：点击任一方案卡片（局域网 / P2P打洞 / 虚拟组网 / 自建中转 / 轻量穿透），按 1→2→3 步即可真实连入远程桌面。",
    },
  ]);
  const logSeqRef = useRef(2);
  const pushLog = useCallback((level: "info" | "ok" | "warn", text: string) => {
    const id = logSeqRef.current++;
    const time = new Date().toLocaleTimeString("zh-CN", { hour12: false });
    setEventLogs((prev) => [...prev, { id, time, level, text }].slice(-120));
  }, []);

  const enterRemoteDesktop = useCallback((devId: string) => {
    rememberLastDeviceId(devId);
    window.location.hash = `#/session/${devId}?open=${DESKTOP_PRIMARY_ID}`;
  }, []);

  const refreshDevicesAndStatus = useCallback(async () => {
    try {
      const list = await listDevices();
      if (Array.isArray(list)) {
        setSavedDevices(list);
        if (!targetDeviceId && list[0]) {
          setTargetDeviceId(list[0].id);
        }
      }
    } catch {
      // ignore
    }
    try {
      const r = await getRelayAddr();
      const addr = typeof r === "string" ? r.trim() : "";
      setDefaultRelayAddr(addr);
      setRelayInputAddr((prev) => prev || addr);
      if (addr) {
        setServerPool((prev) => {
          if (prev.some((n) => n.addr === addr)) return prev;
          const kind = inferNodeKind({ addr, name: "默认中转" });
          const next: RelayNodeItem[] = [
            ...prev,
            {
              id: "default-relay",
              name: "默认中转节点",
              addr,
              kind,
              lastRttMs: null,
              status: "idle",
            },
          ];
          try {
            window.localStorage.setItem("rd.labServerPool", JSON.stringify(next));
          } catch {
            // ignore
          }
          return next;
        });
      }
    } catch {
      // ignore
    }
    try {
      const st = await easytierDetectStatus();
      if (st && typeof st === "object") {
        setEtStatus(st);
        if (st.inviteCode) setEtInviteCode(st.inviteCode);
      }
    } catch {
      // ignore
    }
    try {
      const info = await relayIdStatus();
      if (info && typeof info === "object") setRelayIdInfo(info);
    } catch {
      // ignore
    }
  }, [targetDeviceId]);

  useEffect(() => {
    void refreshDevicesAndStatus();
  }, [refreshDevicesAndStatus]);

  const selectedDevice = savedDevices.find((d) => d.id === targetDeviceId) ?? null;
  const targetKey =
    customIp.trim().length > 0
      ? `custom:${customIp.trim()}`
      : selectedDevice
        ? `dev:${selectedDevice.id}`
        : "manual:default";
  const baseTargetIp =
    customIp.trim().length > 0
      ? customIp.trim()
      : selectedDevice
        ? selectedDevice.addr.replace(/:21118$/, "")
        : "";
  const activePlans: RealTestPlanDto[] = (() => {
    const existing = plansByTarget[targetKey];
    if (existing && existing.length > 0) return existing;
    return buildSmartStarterPlans(
      targetKey,
      baseTargetIp,
      defaultRelayAddr,
      selectedDevice?.relay,
    );
  })();
  const confirmedPlanId = confirmedPlanByTarget[targetKey] ?? activePlans[0]?.planId ?? null;

  const savePlansForTarget = useCallback(
    (nextPlans: RealTestPlanDto[]) => {
      setPlansByTarget((prev) => {
        const updated = { ...prev, [targetKey]: nextPlans };
        try {
          window.localStorage.setItem("rd.labTargetPlans", JSON.stringify(updated));
        } catch {
          // ignore
        }
        return updated;
      });
    },
    [targetKey],
  );

  // ── 通用：通过 IP / 虚拟网 IP / 穿透地址发起真实连接（自动处理已信任直入 & 未信任弹配对码） ──
  const executeDirectOrMeshConnect = useCallback(
    async (params: {
      targetAddr: string;
      relayAddr?: string;
      code?: string;
      name?: string;
    }) => {
      const addr = params.targetAddr.trim();
      if (!addr) {
        showToast("请先填写或选择目标设备地址");
        return;
      }
      setConnectingBusy(true);
      try {
        const res = await labConnectDirectOrMesh(params);
        if (res.needPairCode) {
          setPendingPair({
            kind: "direct",
            targetAddr: addr,
            relayAddr: params.relayAddr,
            name: params.name,
            prompt: res.detail,
          });
          setPairCodeInput("");
          pushLog("info", res.detail);
          showToast("已连通对方电脑！请在下方输入 6 位配对码");
          return;
        }
        if (res.ok && res.device) {
          setPendingPair(null);
          pushLog("ok", res.detail);
          await refreshDevicesAndStatus();
          enterRemoteDesktop(res.device.id);
        }
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        pushLog("warn", `连接失败：${msg}`);
        showToast(`连接失败：${msg}`);
      } finally {
        setConnectingBusy(false);
      }
    },
    [enterRemoteDesktop, pushLog, refreshDevicesAndStatus, showToast],
  );

  // ── 确认并启用某个方案（安全过滤 STUN，仅把真正中继写回设备 relay） ──
  const handleConfirmPlan = useCallback(
    async (plan: RealTestPlanDto) => {
      if (plan.mode === "punch") {
        if (plan.relayAddr.trim()) {
          setSelectedStunAddr(plan.relayAddr.trim());
        }
        setActiveGuide("p2p");
        showToast("已切到「P2P 打洞向导」，请按第 2 步生成/粘贴打洞连接码连接！");
        return;
      }
      setConfirmedPlanByTarget((prev) => {
        const next = { ...prev, [targetKey]: plan.planId };
        try {
          window.localStorage.setItem("rd.labConfirmedPlans", JSON.stringify(next));
        } catch {
          // ignore
        }
        return next;
      });
      if (selectedDevice) {
        try {
          const nextAddr =
            plan.mode === "frp" && plan.relayAddr.trim()
              ? plan.relayAddr.trim()
              : plan.targetAddr.trim() || selectedDevice.addr.replace(/:21118$/, "");
          const isRealRelay = plan.mode === "relay" || plan.mode === "cf_worker" || plan.mode === "derp";
          const nextRelay = isRealRelay ? plan.relayAddr.trim() : "";
          await editDevice(selectedDevice.id, selectedDevice.name, nextAddr, nextRelay);
          pushLog("ok", `已将「${plan.planName}」设为 ${selectedDevice.name} 的默认连接方案`);
          showToast(`已切换为「${plan.planName}」`);
        } catch (e) {
          pushLog("warn", `同步设备配置失败：${e instanceof Error ? e.message : String(e)}`);
        }
      } else {
        showToast(`已选中默认方案：${plan.planName}`);
      }
    },
    [pushLog, selectedDevice, showToast, targetKey],
  );

  // ── 一键使用任意已绑定方案直接连接远程桌面 ──
  const handleConnectPlan = useCallback(
    async (plan: RealTestPlanDto) => {
      if (plan.mode === "punch") {
        if (plan.relayAddr.trim()) {
          setSelectedStunAddr(plan.relayAddr.trim());
        }
        setActiveGuide("p2p");
        showToast("公共 STUN 需双端交换洞口：请在下方第 2 步生成或粘贴 rd-p2p:// 打洞码直连！");
        return;
      }
      await handleConfirmPlan(plan);
      if (plan.mode === "frp" && plan.relayAddr.trim()) {
        await executeDirectOrMeshConnect({
          targetAddr: plan.relayAddr.trim(),
          name: plan.planName,
        });
        return;
      }
      const target = plan.targetAddr.trim() || baseTargetIp;
      const relay =
        plan.mode === "relay" || plan.mode === "cf_worker" || plan.mode === "derp"
          ? plan.relayAddr.trim()
          : undefined;
      if (selectedDevice && !customIp.trim() && target === selectedDevice.addr.replace(/:21118$/, "")) {
        setConnectingBusy(true);
        try {
          if (!selectedDevice.online) {
            const ack = await connectDevice(selectedDevice.id);
            rememberHostVersion(selectedDevice.id, ack.hostVersion);
          }
          enterRemoteDesktop(selectedDevice.id);
          return;
        } catch {
          // fallback to unified pair-aware connect flow
        } finally {
          setConnectingBusy(false);
        }
      }
      await executeDirectOrMeshConnect({
        targetAddr: target,
        relayAddr: relay,
        name: selectedDevice?.name || plan.planName,
      });
    },
    [
      baseTargetIp,
      customIp,
      enterRemoteDesktop,
      executeDirectOrMeshConnect,
      handleConfirmPlan,
      selectedDevice,
      showToast,
    ],
  );

  // ── 顶部快捷连接按钮 ──
  const handleHeaderQuickConnect = useCallback(async () => {
    if (customIp.trim()) {
      await executeDirectOrMeshConnect({
        targetAddr: customIp.trim(),
        relayAddr: defaultRelayAddr || undefined,
      });
      return;
    }
    if (!selectedDevice) return;
    setConnectingBusy(true);
    try {
      if (!selectedDevice.online) {
        const ack = await connectDevice(selectedDevice.id);
        rememberHostVersion(selectedDevice.id, ack.hostVersion);
      }
      enterRemoteDesktop(selectedDevice.id);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (msg.includes("尚未信任") || msg.includes("未鉴权")) {
        setPendingPair({
          kind: "direct",
          targetAddr: selectedDevice.addr,
          relayAddr: selectedDevice.relay ?? undefined,
          name: selectedDevice.name,
          prompt: `设备「${selectedDevice.name}」需要重新验证 6 位配对码：`,
        });
      } else {
        pushLog("warn", `连接 ${selectedDevice.name} 失败：${msg}`);
        showToast(`连接失败：${msg}`);
      }
    } finally {
      setConnectingBusy(false);
    }
  }, [
    customIp,
    defaultRelayAddr,
    enterRemoteDesktop,
    executeDirectOrMeshConnect,
    pushLog,
    selectedDevice,
    showToast,
  ]);

  // ── 提交补填的 6 位配对码完成连接并直入桌面 ──
  const handleSubmitPendingPair = useCallback(async () => {
    if (!pendingPair) return;
    const code = pairCodeInput.trim();
    if (code.length !== 6) {
      showToast("请输入对方电脑显示的 6 位数字配对码");
      return;
    }
    if (pendingPair.kind === "direct") {
      await executeDirectOrMeshConnect({
        targetAddr: pendingPair.targetAddr,
        relayAddr: pendingPair.relayAddr,
        code,
        name: pendingPair.name,
      });
    } else if (pendingPair.kind === "relay_id") {
      setConnectingBusy(true);
      try {
        const res = await connectByRelayId({
          relayAddr: pendingPair.relayAddr,
          numericId: pendingPair.numericId,
          code,
          name: pendingPair.name,
        });
        if (res.ok && res.device) {
          setPendingPair(null);
          await refreshDevicesAndStatus();
          enterRemoteDesktop(res.device.id);
        } else {
          showToast(res.detail);
        }
      } catch (e) {
        showToast(`配对失败：${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setConnectingBusy(false);
      }
    } else if (pendingPair.kind === "p2p") {
      setP2pCtrlBusy(true);
      try {
        const res = await labP2pConnectToken(
          pendingPair.tokenOrAddr,
          code,
          pendingPair.stunAddr,
        );
        if (res.ok && res.device) {
          setPendingPair(null);
          await refreshDevicesAndStatus();
          enterRemoteDesktop(res.device.id);
        } else {
          showToast(res.detail);
        }
      } catch (e) {
        showToast(`P2P 配对失败：${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setP2pCtrlBusy(false);
      }
    }
  }, [
    enterRemoteDesktop,
    executeDirectOrMeshConnect,
    pairCodeInput,
    pendingPair,
    refreshDevicesAndStatus,
    showToast,
  ]);

  // ── 单个方案真实探测 ──
  const probeSinglePlan = useCallback(
    async (plan: RealTestPlanDto): Promise<RealPlanProbeResultDto | null> => {
      setTestingPlanId(plan.planId);
      try {
        const res = await labProbeTargetPlan(plan);
        setRealResults((prev) => ({ ...prev, [plan.planId]: res }));
        res.logs.forEach((l) => pushLog(res.ok ? "ok" : "warn", l));
        return res;
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        pushLog("warn", `探测「${plan.planName}」异常：${msg}`);
        return null;
      } finally {
        setTestingPlanId(null);
      }
    },
    [pushLog],
  );

  // ── 批量实测当前目标已绑定方案 ──
  const runBoundPlansBatch = useCallback(async () => {
    setRunningBatch(true);
    setSmartHint(null);
    pushLog("info", `开始实测 ${activePlans.length} 个已绑定方案…`);
    let bestPlan: RealTestPlanDto | undefined;
    let bestRtt = Number.POSITIVE_INFINITY;
    for (const plan of activePlans) {
      const res = await probeSinglePlan(plan);
      if (plan.mode !== "punch" && res?.ok && (res.avgRttMs ?? 9999) < bestRtt) {
        bestRtt = res.avgRttMs ?? 9999;
        bestPlan = plan;
      }
    }
    if (bestPlan) {
      setSmartHint({
        level: "ok",
        title: `最优可达方案：${bestPlan.planName}（${bestRtt.toFixed(1)} ms）`,
        detail: "真实 QUIC 握手已验证可达，点右侧按钮直接连入远程桌面。",
        bestPlan,
      });
    } else {
      setSmartHint({
        level: "warn",
        title: "当前直连/已绑方案未连通对方设备",
        detail: "跨网连接请在下方切换「P2P 打洞向导」生成打洞码，或使用「虚拟组网 (EasyTier)」口令组网！",
        guideTarget: "p2p",
      });
    }
    setRunningBatch(false);
  }, [activePlans, probeSinglePlan, pushLog]);

  // ── 一键全方案智能寻优（真实区分“直接可连方案”与“跨网打洞/组网引导”） ──
  const runSmartAutoRace = useCallback(async () => {
    setSmartRacing(true);
    setSmartHint(null);
    pushLog("info", "⚡ 启动全方案智能寻优：实测已绑线路 + 同步公共节点 + 自动 NAT 穿透体检…");

    let bestPlan: RealTestPlanDto | undefined;
    let bestPlanRtt = Number.POSITIVE_INFINITY;

    for (const plan of activePlans) {
      const res = await probeSinglePlan(plan);
      if (plan.mode !== "punch" && res?.ok && (res.avgRttMs ?? 9999) < bestPlanRtt) {
        bestPlanRtt = res.avgRttMs ?? 9999;
        bestPlan = plan;
      }
    }

    let pool = serverPool;
    if (pool.length === 0) {
      try {
        const fetched = await labFetchPublicNodes(customSubUrl || undefined);
        if (fetched?.nodes?.length) {
          pool = fetched.nodes.map((n, idx) => ({
            id: `pub-${idx}-${Date.now().toString(36)}`,
            name: n.name,
            addr: n.addr,
            kind: n.kind,
            region: n.region,
            source: n.source,
            lastRttMs: null,
            status: "idle" as const,
          }));
          saveServerPool(pool);
        }
      } catch {
        // ignore
      }
    }

    let currentNat = natResult;
    if (!currentNat) {
      const stuns = pool
        .filter((n) => inferNodeKind(n) === "stun")
        .slice(0, 2)
        .map((n) => n.addr);
      try {
        const diag = await labDiagnoseNat(
          stuns.length >= 2 ? stuns : ["stun.miwifi.com:3478", "stun.chat.bilibili.com:3478"],
        );
        currentNat = diag;
        setNatResult(diag);
        if (diag.serverA) setSelectedStunAddr(diag.serverA);
        pushLog("info", `自动 NAT 体检：${diag.label}`);
      } catch {
        // ignore
      }
    }

    if (bestPlan) {
      setSmartHint({
        level: "ok",
        title: `推荐直接连接：${bestPlan.planName}（${bestPlanRtt.toFixed(1)} ms）`,
        detail: "该通道已完成真实握手，点击右侧按钮直接进入 4K@144Hz 完整桌面！",
        bestPlan,
      });
    } else if (currentNat?.natType === "cone") {
      setActiveGuide("p2p");
      setSmartHint({
        level: "suggest",
        title: `本机为「圆锥型 NAT」（STUN ${currentNat.rttAMs ?? 20}ms），非常适合免费 P2P 打洞！`,
        detail:
          "已自动为您切开下方「P2P 打洞三步向导」：被控端点『生成打洞码』→ 控制端粘贴，无需买服务器即可跨网直连！",
        guideTarget: "mesh",
      });
    } else {
      setActiveGuide("mesh");
      setSmartHint({
        level: "warn",
        title: "当前网络适合使用「虚拟组网」或「自建中转 9位ID」跨网连接",
        detail:
          "已为您展开「虚拟组网向导」：点『创建组网房间』把口令发给对方，免重启自动组进同一局域网即可直连！",
        guideTarget: "relay",
      });
    }
    setSmartRacing(false);
  }, [activePlans, customSubUrl, natResult, probeSinglePlan, pushLog, saveServerPool, serverPool]);

  // ── 节点池智能采用：按节点真实用途导流，绝不再让用户“绑完 STUN 卡住”！ ──
  const handleUsePoolNode = useCallback(
    async (node: RelayNodeItem) => {
      const kind = inferNodeKind(node);
      if (kind === "stun") {
        setSelectedStunAddr(node.addr);
        setActiveGuide("p2p");
        setOpenDrawer("none");
        pushLog(
          "ok",
          `已选用公共 STUN「${node.name} (${node.addr})」作为打洞服务器，请按向导第 2 步生成/粘贴打洞码！`,
        );
        showToast(`已选用 ${node.name}！请看下方第 2 步生成或粘贴打洞码`);
        return;
      }
      if (kind === "easytier") {
        setSelectedEtBootstrap(node.addr);
        setActiveGuide("mesh");
        setOpenDrawer("none");
        pushLog("ok", `已选用 EasyTier 引导节点「${node.addr}」，点第 2 步生成组网口令即可！`);
        showToast(`已选用引导节点 ${node.name}！点第 2 步创建组网房间即可`);
        return;
      }
      const mode: RemotePlanMode =
        kind === "cf_worker"
          ? "cf_worker"
          : kind === "frp"
            ? "frp"
            : kind === "derp"
              ? "derp"
              : "relay";
      const plan: RealTestPlanDto = {
        planId: `adopted-${Date.now().toString(36)}`,
        planName: node.name,
        mode,
        targetAddr: baseTargetIp || "192.168.1.10",
        relayAddr: node.addr,
        rounds: 3,
      };
      savePlansForTarget([...activePlans, plan]);
      await handleConfirmPlan(plan);
      setOpenDrawer("none");
      showToast(`已绑定方案「${node.name}」，可在方案列表直接点「连接桌面」`);
    },
    [activePlans, baseTargetIp, handleConfirmPlan, pushLog, savePlansForTarget, showToast],
  );

  // ── 保存新增 / 编辑自定义方案 ──
  const handleSaveCustomPlan = useCallback(() => {
    if (newPlanMode === "punch") {
      const stun = newPlanRelay.trim() || selectedStunAddr;
      setSelectedStunAddr(stun);
      setShowAddPlan(false);
      setActiveGuide("p2p");
      showToast("P2P 打洞需双端交换洞口：已为您切到「P2P 打洞向导」，按步骤 2 生成/粘贴打洞码即可！");
      return;
    }
    const meta = MODE_META[newPlanMode];
    const name = newPlanName.trim() || meta.label;
    const addr = newPlanAddr.trim() || baseTargetIp;
    const relay = newPlanRelay.trim() || defaultRelayAddr;
    if (!meta.needsRelayAddr && !addr) {
      showToast("请填写目标设备 IP 地址");
      return;
    }
    if (meta.needsRelayAddr && !relay) {
      showToast(`请填写 ${meta.shortLabel} 服务器地址，或从节点池选择`);
      return;
    }
    if (meta.needsRelayAddr && relay && !serverPool.some((n) => n.addr === relay)) {
      saveServerPool([
        ...serverPool,
        {
          id: `node-${Date.now().toString(36)}`,
          name,
          addr: relay,
          kind: meta.poolKind ?? "relay",
          lastRttMs: null,
          status: "idle",
        },
      ]);
    }
    const plan: RealTestPlanDto = {
      planId: editingPlanId ?? `plan-${Date.now().toString(36)}`,
      planName: name,
      mode: newPlanMode,
      targetAddr: addr,
      relayAddr: meta.needsRelayAddr ? relay : "",
      rounds: 3,
    };
    if (editingPlanId) {
      savePlansForTarget(activePlans.map((p) => (p.planId === editingPlanId ? plan : p)));
      pushLog("ok", `已更新自定义方案「${plan.planName}」`);
    } else {
      savePlansForTarget([...activePlans, plan]);
      pushLog("ok", `已新增方案「${plan.planName}」（点方案右侧『连接桌面』即可使用）`);
    }
    setNewPlanName("");
    setNewPlanAddr("");
    setNewPlanRelay("");
    setEditingPlanId(null);
    setShowAddPlan(false);
  }, [
    activePlans,
    baseTargetIp,
    defaultRelayAddr,
    editingPlanId,
    newPlanAddr,
    newPlanMode,
    newPlanName,
    newPlanRelay,
    pushLog,
    savePlansForTarget,
    saveServerPool,
    selectedStunAddr,
    serverPool,
    showToast,
  ]);

  const handleDeletePlan = useCallback(
    (planId: string) => {
      savePlansForTarget(activePlans.filter((p) => p.planId !== planId));
      pushLog("info", "已移除该绑定方案");
    },
    [activePlans, pushLog, savePlansForTarget],
  );

  // ── 场景 1：局域网扫描 ──
  const handleScanLan = useCallback(async () => {
    setLanScanning(true);
    try {
      const list = await discoverDevices();
      setLanFound(list);
      if (list.length > 0 && !lanIpInput) {
        setLanIpInput(list[0]!.addr.replace(/:21118$/, ""));
      }
      showToast(
        list.length > 0
          ? `发现 ${list.length} 台同网段设备，点击即可填入连接`
          : "暂未扫描到广播应答，可直接手动输入对方局域网 IP",
      );
    } catch (e) {
      showToast(`扫描失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setLanScanning(false);
    }
  }, [lanIpInput, showToast]);

  // ── 场景 2：一键拉取最快公共 STUN + NAT 体检 ──
  const handleQuickSetupStun = useCallback(async () => {
    setNatBusy(true);
    try {
      let pool = serverPool;
      const res = await labFetchPublicNodes(customSubUrl || undefined);
      const existing = new Set(pool.map((n) => n.addr.toLowerCase()));
      const added: RelayNodeItem[] = [];
      for (const n of res.nodes) {
        if (!existing.has(n.addr.toLowerCase())) {
          existing.add(n.addr.toLowerCase());
          added.push({
            id: `pub-${Date.now().toString(36)}-${added.length}`,
            name: n.name,
            addr: n.addr,
            kind: n.kind,
            region: n.region,
            source: n.source,
            lastRttMs: null,
            status: "idle",
          });
        }
      }
      pool = [...pool, ...added];
      const stunNodes = pool.filter((n) => inferNodeKind(n) === "stun").slice(0, 5);
      const probed = await Promise.all(
        stunNodes.map(async (node) => {
          try {
            const r = await labProbeNode("stun", node.addr);
            return {
              ...node,
              kind: "stun" as const,
              lastRttMs: r.rttMs,
              mappedAddr: r.mappedAddr,
              status: r.ok ? ("ok" as const) : ("fail" as const),
            };
          } catch {
            return { ...node, status: "fail" as const };
          }
        }),
      );
      const byId = new Map(probed.map((p) => [p.id, p]));
      const merged = pool.map((n) => byId.get(n.id) ?? n);
      saveServerPool(merged);

      const okStuns = probed
        .filter((p) => p.status === "ok" && p.lastRttMs !== null)
        .sort((a, b) => (a.lastRttMs ?? 9999) - (b.lastRttMs ?? 9999));
      if (okStuns[0]) {
        setSelectedStunAddr(okStuns[0].addr);
      }
      const diagAddrs =
        okStuns.length >= 2
          ? [okStuns[0]!.addr, okStuns[1]!.addr]
          : ["stun.miwifi.com:3478", "stun.chat.bilibili.com:3478"];
      const diag = await labDiagnoseNat(diagAddrs);
      setNatResult(diag);
      pushLog(
        "ok",
        `STUN 优选完成：已选「${okStuns[0]?.name ?? diag.serverA}」，NAT 诊断：${diag.label}`,
      );
      showToast(`已选最快 STUN（${okStuns[0]?.lastRttMs ?? diag.rttAMs ?? 20}ms）· ${diag.label}`);
    } catch (e) {
      showToast(`STUN 检测失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setNatBusy(false);
    }
  }, [customSubUrl, pushLog, saveServerPool, serverPool, showToast]);

  // ── 场景 2：被控端生成真实 STUN 打洞连接码 ──
  const handleGenerateP2pHostCode = useCallback(async () => {
    setP2pHostBusy(true);
    try {
      const info = await labP2pHostOpen(selectedStunAddr);
      setP2pHostInfo(info);
      await copyTextSafe(info.token);
      pushLog(
        "ok",
        `本机打洞监听已开启：公网洞口 ${info.mappedAddr}（已自动复制 rd-p2p:// 连接码，10分钟保活中）`,
      );
      showToast("已生成并复制本机打洞连接码！发给控制端粘贴即可直连");
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `生成打洞连接码失败：${msg}`);
      showToast(msg);
    } finally {
      setP2pHostBusy(false);
    }
  }, [pushLog, selectedStunAddr, showToast]);

  // ── 场景 2：被控端粘贴回执码协助双向开洞 ──
  const handleHostAssistPunch = useCallback(async () => {
    if (!p2pReceiptInput.trim()) {
      showToast("请先粘贴控制端发来的 rd-punch:// 回执码");
      return;
    }
    setP2pPunchingPeer(true);
    try {
      const msg = await labP2pHostPunchPeer(p2pReceiptInput.trim());
      pushLog("ok", msg);
      showToast("双向开洞脉冲已发出！请让对方立刻点「完成打洞连接」");
    } catch (e) {
      showToast(e instanceof Error ? e.message : String(e));
    } finally {
      setP2pPunchingPeer(false);
    }
  }, [p2pReceiptInput, pushLog, showToast]);

  // ── 场景 2：控制端粘贴 rd-p2p:// 连接码发起打洞直连 ──
  const handleConnectByP2pToken = useCallback(async () => {
    if (!p2pTokenInput.trim()) {
      showToast("请粘贴被控端生成的 rd-p2p:// 打洞连接码");
      return;
    }
    setP2pCtrlBusy(true);
    try {
      const res = await labP2pConnectToken(p2pTokenInput.trim(), undefined, selectedStunAddr);
      if (res.ok && res.device) {
        setP2pReverseCode(null);
        setP2pLiveStatus(null);
        setPendingPair(null);
        pushLog("ok", res.detail);
        await refreshDevicesAndStatus();
        enterRemoteDesktop(res.device.id);
        return;
      }
      if (res.needPairCode) {
        setP2pReverseCode(null);
        setP2pLiveStatus(null);
        setPendingPair({
          kind: "p2p",
          tokenOrAddr: p2pTokenInput.trim(),
          stunAddr: selectedStunAddr,
          prompt: res.detail,
        });
        setPairCodeInput("");
        showToast("已打通！请输入对方 6 位配对码");
        return;
      }
      if (res.needReversePunch) {
        setP2pReverseCode(res.reverseCode);
        setP2pLiveStatus({
          txPulses: res.txPulses ?? 0,
          rxPulses: res.rxPulses ?? 0,
          ctrlNatType: res.ctrlNatType ?? "unknown",
          hostNatType: res.hostNatType ?? "unknown",
          attemptCount: res.attemptCount ?? 1,
          detail: res.detail,
          punchedPeer: null,
        });
        await copyTextSafe(res.reverseCode);
        pushLog(
          (res.attemptCount ?? 1) > 1 ? "warn" : "info",
          `${res.detail}（回执码：${res.reverseCode}）`,
        );
        showToast(
          (res.attemptCount ?? 1) > 1
            ? `第 ${res.attemptCount} 次检测：已发 ${res.txPulses ?? 0} 包 / 收包 ${res.rxPulses ?? 0}（详见下方诊断框）`
            : "已复制回执码！发给被控端点「协助开洞」，打通瞬间将自动连入桌面",
        );
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `打洞连接异常：${msg}`);
      showToast(msg);
    } finally {
      setP2pCtrlBusy(false);
    }
  }, [
    enterRemoteDesktop,
    p2pTokenInput,
    pushLog,
    refreshDevicesAndStatus,
    selectedStunAddr,
    showToast,
  ]);

  // ── 控制端处于等待双向开洞状态时：每秒实时刷新脉冲计数，收到对方开洞包立即零点击自动连入桌面！ ──
  const autoConnectingRef = useRef(false);
  useEffect(() => {
    if (!p2pReverseCode) {
      autoConnectingRef.current = false;
      return undefined;
    }
    const iv = window.setInterval(() => {
      void labP2pCheckStatus()
        .then((st) => {
          setP2pLiveStatus((prev) =>
            prev
              ? {
                  ...prev,
                  txPulses: st.txPulses,
                  rxPulses: st.rxPulses,
                  ctrlNatType: st.ctrlNatType || prev.ctrlNatType,
                  punchedPeer: st.punchedPeer,
                }
              : prev,
          );
          if (st.punchedPeer && !autoConnectingRef.current && !p2pCtrlBusy) {
            autoConnectingRef.current = true;
            pushLog("ok", `🎯 检测到对方开洞回包 (${st.punchedPeer})！正在自动完成握手进入桌面…`);
            void handleConnectByP2pToken().finally(() => {
              autoConnectingRef.current = false;
            });
          }
        })
        .catch(() => undefined);
    }, 1000);
    return () => window.clearInterval(iv);
  }, [handleConnectByP2pToken, p2pCtrlBusy, p2pReverseCode, pushLog]);

  // ── 一键复制完整排障日志（含系统底层 rd-app.log + P2P 槽位收发包计数 + 界面事件日志） ──
  const handleCopyDiagnosticLog = useCallback(async () => {
    try {
      const backendLog = await labExportDiagnosticLog();
      const uiTail = eventLogs
        .slice(-25)
        .map((l) => `[${l.time}] [${l.level.toUpperCase()}] ${l.text}`)
        .join("\n");
      const full = `${backendLog}\n--- 实验室前端交互记录 ---\n${uiTail}\n`;
      await copyTextSafe(full);
      pushLog("ok", "已将完整排障诊断日志复制到剪贴板，可直接粘贴发给开发者！");
      showToast("📋 已复制排障诊断日志！直接粘贴发给开发者即可");
    } catch (e) {
      showToast(`导出日志失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }, [eventLogs, pushLog, showToast]);

  // ── 场景 3：虚拟组网动作（安装 / 生成房间 / 加入房间 / 扫描在线成员） ──
  const handleScanAllMeshPeers = useCallback(async () => {
    setScanningMesh(true);
    try {
      const [st, list] = await Promise.all([
        easytierDetectStatus().catch(() => null),
        labScanMeshPeers().catch(() => [] as MeshPeerItemDto[]),
      ]);
      if (st) setEtStatus(st);
      setMeshPeers(list);
      showToast(
        list.length > 0
          ? `已发现 ${list.length} 台虚拟组网/局域网在线设备`
          : "暂未发现其他在线组网成员（请确认对方已加入同一组网口令）",
      );
    } finally {
      setScanningMesh(false);
    }
  }, [showToast]);

  const handleEtAction = useCallback(
    async (action: "install" | "invite" | "join") => {
      setEtBusy(true);
      try {
        if (action === "install") {
          const res = await easytierAutoInstall();
          pushLog("ok", res.detail);
          showToast("EasyTier 下载安装完成！现在可直接创建或加入组网房间");
        } else if (action === "invite") {
          const peers = selectedEtBootstrap
            ? [selectedEtBootstrap]
            : serverPool
                .filter((n) => inferNodeKind(n) === "easytier")
                .slice(0, 3)
                .map((n) => n.addr);
          const res = await easytierGenerateInvite(peers);
          setEtInviteCode(res.inviteCode);
          await copyTextSafe(res.inviteCode);
          pushLog("ok", res.hint);
          showToast("房间已创建并免重启启动！口令已复制，发给对方粘贴即可");
        } else if (action === "join") {
          if (!etJoinInput.trim()) {
            showToast("请先粘贴对方发来的 rd-et:// 组网口令");
            return;
          }
          const res = await easytierJoinInvite(etJoinInput.trim());
          setEtInviteCode(res.inviteCode);
          pushLog("ok", res.hint);
          showToast("已加入房间并后台启动！点击「刷新在线成员」即可看到对方");
        }
        const st = await easytierDetectStatus();
        setEtStatus(st);
        const list = await labScanMeshPeers().catch(() => [] as MeshPeerItemDto[]);
        setMeshPeers(list);
      } catch (e) {
        showToast(`EasyTier 操作失败：${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setEtBusy(false);
      }
    },
    [etJoinInput, pushLog, selectedEtBootstrap, serverPool, showToast],
  );

  // ── 场景 4：自建中转保存热上线 & 9位ID一键连接 ──
  const handleSaveRelayHot = useCallback(async () => {
    const addr = relayInputAddr.trim();
    if (!addr) {
      showToast("请填写自建 rd-relay 服务器地址（如 1.2.3.4:21118）");
      return;
    }
    setSavingRelay(true);
    try {
      await setRelayAddr(addr);
      setDefaultRelayAddr(addr);
      const info = await relayIdStatus();
      setRelayIdInfo(info);
      pushLog("ok", `已保存并免重启热连接中转服务器 ${addr}，本机 9 位 ID（${info.numericId}）已上线！`);
      showToast(`中转已生效！本机 9 位 ID：${info.numericId}`);
    } catch (e) {
      showToast(`保存中转失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setSavingRelay(false);
    }
  }, [pushLog, relayInputAddr, showToast]);

  const handleLookupNumericId = useCallback(async () => {
    const id = peerNumericId.replace(/\s+/g, "").trim();
    const relay = relayInputAddr.trim() || relayIdInfo?.relayAddr || defaultRelayAddr;
    if (!id) {
      showToast("请先输入对方的 9 位设备 ID");
      return;
    }
    if (!relay) {
      showToast("请先在第 1 步填写或通过 SSH 一键部署自建中转地址");
      return;
    }
    try {
      const res = await relayResolveId(relay, id);
      setRelayLookupRes(res);
      pushLog(res.online ? "ok" : "warn", `9位ID ${id}：${res.detail}`);
    } catch (e) {
      showToast(`查询失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }, [defaultRelayAddr, peerNumericId, pushLog, relayIdInfo?.relayAddr, relayInputAddr, showToast]);

  const handleConnectNumericId = useCallback(async () => {
    const id = peerNumericId.replace(/\s+/g, "").trim();
    const relay = relayInputAddr.trim() || relayIdInfo?.relayAddr || defaultRelayAddr;
    if (!id) {
      showToast("请输入对方的 9 位设备 ID");
      return;
    }
    if (!relay) {
      showToast("请先在第 1 步配置自建中转服务器地址");
      return;
    }
    setConnectingBusy(true);
    try {
      const res = await connectByRelayId({
        relayAddr: relay,
        numericId: id,
        code: relayPairCode.trim() || undefined,
      });
      if (res.needPairCode) {
        setPendingPair({
          kind: "relay_id",
          relayAddr: relay,
          numericId: id,
          prompt: res.detail,
        });
        setPairCodeInput("");
        showToast("已找到并连通对方！请输入 6 位配对码");
        return;
      }
      if (res.ok && res.device) {
        setPendingPair(null);
        pushLog("ok", res.detail);
        await refreshDevicesAndStatus();
        enterRemoteDesktop(res.device.id);
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `9位ID 连接失败：${msg}`);
      showToast(msg);
    } finally {
      setConnectingBusy(false);
    }
  }, [
    defaultRelayAddr,
    enterRemoteDesktop,
    peerNumericId,
    pushLog,
    refreshDevicesAndStatus,
    relayIdInfo?.relayAddr,
    relayInputAddr,
    relayPairCode,
    showToast,
  ]);

  // ── SSH 一键部署自建中转 ──
  const handleSshDeploy = useCallback(async () => {
    if (!sshHost.trim()) {
      showToast("请先填写 VPS 公网 IP 或域名");
      return;
    }
    setSshBusy(true);
    setSshResult(null);
    try {
      const normalized = normalizeSshInput({
        host: sshHost,
        port: sshPort,
        user: sshUser,
        keyPath: sshKeyPath,
      });
      const res = await deployRelaySsh({
        host: normalized.host,
        port: normalized.port,
        user: normalized.user,
        keyPath: normalized.keyPath,
        relayPort: 21118,
        mode: sshMode,
        domain: sshDomain.trim(),
      });
      setSshResult(res);
      if (res.ok && res.relayAddr) {
        await setRelayAddr(res.relayAddr);
        setDefaultRelayAddr(res.relayAddr);
        setRelayInputAddr(res.relayAddr);
        const info = await relayIdStatus().catch(() => null);
        if (info) setRelayIdInfo(info);
        saveServerPool([
          ...serverPool,
          {
            id: `ssh-${Date.now().toString(36)}`,
            name: sshName.trim() || `自建中转 (${normalized.host})`,
            addr: res.relayAddr,
            kind: "relay",
            lastRttMs: null,
            status: "ok",
          },
        ]);
        setShowInlineSsh(false);
        pushLog("ok", `SSH 一键部署成功：${res.relayAddr}（${res.summary}）`);
        showToast(res.summary || `部署成功并已生效：${res.relayAddr}`);
      } else {
        const detail = res.log.trim() ? ` | 详情：${res.log.trim()}` : "";
        pushLog("warn", `${res.summary || "SSH 部署未完成"}${detail}`);
        showToast(res.summary || "SSH 部署未完成，请查看下方诊断日志");
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `SSH 部署失败：${msg}`);
      showToast(`SSH 部署失败：${msg}`);
    } finally {
      setSshBusy(false);
    }
  }, [
    pushLog,
    saveServerPool,
    serverPool,
    showToast,
    sshDomain,
    sshHost,
    sshKeyPath,
    sshMode,
    sshName,
    sshPort,
    sshUser,
  ]);

  // ── 节点池操作：拉取公共源 / 批量导入 / 并发测速 ──
  const handleFetchPublicNodes = useCallback(async () => {
    setFetchingPublic(true);
    try {
      if (customSubUrl.trim()) {
        window.localStorage.setItem("rd.labCustomSubUrl", customSubUrl.trim());
      }
      const res = await labFetchPublicNodes(customSubUrl || undefined);
      const existing = new Set(serverPool.map((n) => n.addr.toLowerCase()));
      const added: RelayNodeItem[] = [];
      for (const n of res.nodes) {
        if (!existing.has(n.addr.toLowerCase())) {
          existing.add(n.addr.toLowerCase());
          added.push({
            id: `pub-${Date.now().toString(36)}-${added.length}`,
            name: n.name,
            addr: n.addr,
            kind: n.kind,
            region: n.region,
            source: n.source,
            lastRttMs: null,
            status: "idle",
          });
        }
      }
      saveServerPool([...serverPool, ...added]);
      res.logs.forEach((l) => pushLog("info", l));
      showToast(`已同步公共节点池：新增 ${added.length} 个节点`);
    } catch (e) {
      showToast(`拉取公共节点失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setFetchingPublic(false);
    }
  }, [customSubUrl, pushLog, saveServerPool, serverPool, showToast]);

  const handleBatchImportNodes = useCallback(() => {
    const lines = batchText
      .split(/\r?\n/)
      .map((l) => l.trim())
      .filter((l) => l.length > 0 && !l.startsWith("#"));
    if (lines.length === 0) return;
    const existing = new Set(serverPool.map((n) => n.addr.toLowerCase()));
    const added: RelayNodeItem[] = [];
    for (const line of lines) {
      const [addrPart, namePart] = line.split(/\s+/);
      const addr = (addrPart ?? "").trim();
      if (!addr || existing.has(addr.toLowerCase())) continue;
      existing.add(addr.toLowerCase());
      const kind: PoolNodeKind =
        batchKind === "auto" ? inferNodeKind({ addr, name: namePart ?? addr }) : batchKind;
      added.push({
        id: `imp-${Date.now().toString(36)}-${added.length}`,
        name: namePart?.trim() || `${kind.toUpperCase()} · ${addr.slice(0, 24)}`,
        addr,
        kind,
        lastRttMs: null,
        status: "idle",
      });
    }
    saveServerPool([...serverPool, ...added]);
    setBatchText("");
    showToast(`已导入 ${added.length} 个自定义节点`);
  }, [batchKind, batchText, saveServerPool, serverPool, showToast]);

  const handleProbeAllPool = useCallback(async () => {
    const targets = serverPool.filter(
      (n) => poolFilter === "all" || inferNodeKind(n) === poolFilter,
    );
    if (targets.length === 0) return;
    setProbingPool(true);
    const results = await Promise.all(
      targets.map(async (node) => {
        const k = inferNodeKind(node);
        try {
          const r = await labProbeNode(k, node.addr);
          return {
            ...node,
            kind: k,
            lastRttMs: r.rttMs,
            mappedAddr: r.mappedAddr,
            status: r.ok ? ("ok" as const) : ("fail" as const),
          };
        } catch {
          return { ...node, status: "fail" as const };
        }
      }),
    );
    const byId = new Map(results.map((r) => [r.id, r]));
    const merged = serverPool
      .map((n) => byId.get(n.id) ?? n)
      .sort((a, b) => (a.lastRttMs ?? 99999) - (b.lastRttMs ?? 99999));
    saveServerPool(merged);
    setProbingPool(false);
    showToast(`已完成 ${targets.length} 个节点并发测速并按延迟排序`);
  }, [poolFilter, saveServerPool, serverPool, showToast]);

  const handleProbeOneNode = useCallback(
    async (node: RelayNodeItem) => {
      setProbingNodeId(node.id);
      const k = inferNodeKind(node);
      try {
        const r = await labProbeNode(k, node.addr);
        saveServerPool(
          serverPool.map((n) =>
            n.id === node.id
              ? {
                  ...n,
                  kind: k,
                  lastRttMs: r.rttMs,
                  mappedAddr: r.mappedAddr,
                  status: r.ok ? "ok" : "fail",
                }
              : n,
          ),
        );
        pushLog(r.ok ? "ok" : "warn", `${node.name}：${r.detail}`);
      } finally {
        setProbingNodeId(null);
      }
    },
    [pushLog, saveServerPool, serverPool],
  );

  // ── 计算 5 大家族卡片状态 ──
  const familyStatus = (famId: SchemeFamilyId) => {
    if (famId === "p2p" && p2pHostInfo) {
      return { tone: "ok" as const, text: "洞口保活中" };
    }
    if (famId === "mesh" && etStatus?.running) {
      return { tone: "ok" as const, text: `已组网 (${etStatus.peers.length}台)` };
    }
    if (famId === "relay" && defaultRelayAddr) {
      return { tone: "ok" as const, text: "9位ID 就绪" };
    }
    const famPlans = activePlans.filter((p) => familyOfMode(p.mode) === famId);
    const okResults = famPlans
      .map((p) => realResults[p.planId])
      .filter((r): r is RealPlanProbeResultDto => Boolean(r?.ok));
    if (okResults.length > 0) {
      const best = Math.min(...okResults.map((r) => r.avgRttMs ?? 999));
      return { tone: "ok" as const, text: `可达 ${best.toFixed(0)}ms` };
    }
    return { tone: "idle" as const, text: "点此按步向导" };
  };

  const stunPoolNodes = serverPool.filter((n) => inferNodeKind(n) === "stun");
  const visiblePlans = onlyFilterFamily
    ? activePlans.filter((p) => familyOfMode(p.mode) === activeGuide)
    : activePlans;
  const visiblePool = serverPool.filter(
    (n) => poolFilter === "all" || inferNodeKind(n) === poolFilter,
  );

  return (
    <section
      className={drawer ? styles.labDrawer : styles.labPage}
      aria-label="方案实验室极客操控台"
    >
      {/* ── 顶部双层导航栏 ── */}
      <header className={styles.topBar}>
        <div className={styles.titleGroup}>
          <span className={styles.labBadge}>RD LAB</span>
          <h1 className={styles.labTitle}>方案实验室 · 真栈操控台</h1>
          <div className={styles.layerTabs} role="tablist" aria-label="实验室功能分层">
            <button
              type="button"
              role="tab"
              aria-selected={activeLayer === "connectivity"}
              className={
                activeLayer === "connectivity"
                  ? `${styles.layerTab} ${styles.layerTabActive}`
                  : styles.layerTab
              }
              onClick={() => setActiveLayer("connectivity")}
            >
              ⚡ 智能连接中心
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={activeLayer === "tuning"}
              className={
                activeLayer === "tuning"
                  ? `${styles.layerTab} ${styles.layerTabActive}`
                  : styles.layerTab
              }
              onClick={() =>
                setActiveLayer((cur) => (cur === "tuning" ? "connectivity" : "tuning"))
              }
            >
              {activeLayer === "tuning"
                ? "▲ 收起高级区（画质调优）"
                : "▼ 展开高级调优（画质/弱网/示波器）"}
            </button>
          </div>
        </div>

        <div className={styles.actionGroup}>
          {(selectedDevice || customIp.trim()) && (
            <button
              type="button"
              className={`${styles.btn} ${styles.btnPrimary}`}
              disabled={connectingBusy}
              onClick={() => void handleHeaderQuickConnect()}
              title="直接连接并进入完整远程桌面"
            >
              {connectingBusy
                ? "连接中…"
                : `🖥️ 连接 ${customIp.trim() || selectedDevice?.name || "目标"}`}
            </button>
          )}
          {drawer && onClose && (
            <button type="button" className={styles.btn} onClick={onClose}>
              收起抽屉 (Esc)
            </button>
          )}
        </div>
      </header>

      {toast && (
        <div className={styles.toastBar} role="status">
          {toast}
        </div>
      )}

      {/* ── Layer 1：智能连接中心（五大场景分步闭环实战向导 + 多方案测速直连） ── */}
      {activeLayer === "connectivity" && (
        <div className={styles.centerBody}>
          {/* 1) 目标选择 + 一键全方案智能寻优条 */}
          <div className={styles.smartBar}>
            <select
              className={styles.select}
              aria-label="选择要测试的被控设备"
              value={targetDeviceId}
              onChange={(e) => {
                setTargetDeviceId(e.target.value);
                setCustomIp("");
                setRealResults({});
                setSmartHint(null);
              }}
            >
              <option value="">新设备 / 自定义目标</option>
              {savedDevices.map((d) => (
                <option key={d.id} value={d.id}>
                  {d.name} ({d.addr.replace(/:21118$/, "")}){d.online ? " · 在线" : ""}
                </option>
              ))}
            </select>
            <input
              type="text"
              className={styles.textInput}
              value={customIp}
              onChange={(e) => setCustomIp(e.currentTarget.value)}
              placeholder="可选：输入对方 IP（LAN / 100.x Tailscale / 10.126.x EasyTier）"
              aria-label="自定义测试目标 IP"
            />
            <button
              type="button"
              className={`${styles.btn} ${styles.btnPrimary}`}
              disabled={smartRacing || runningBatch || testingPlanId !== null}
              onClick={() => void runSmartAutoRace()}
              title="自动检测本机网络与 NAT 类型，推荐最适合当前网络的连接方式"
            >
              {smartRacing ? "⏳ 智能诊断寻优中…" : "✨ 一键全方案智能寻优"}
            </button>
            <button
              type="button"
              className={styles.btn}
              disabled={smartRacing || runningBatch || testingPlanId !== null}
              onClick={() => void runBoundPlansBatch()}
            >
              {runningBatch ? "测速中…" : "▶ 测已绑方案"}
            </button>
            <button
              type="button"
              className={styles.btn}
              onClick={() => {
                setEditingPlanId(null);
                setShowAddPlan((v) => !v);
              }}
            >
              + 自定义方案
            </button>
            <button
              type="button"
              className={styles.btn}
              onClick={() => void handleCopyDiagnosticLog()}
              title="一键复制本机网卡、NAT 类型、打洞脉冲收发数与后台 rd-app.log 日志，直接粘贴即可排障"
            >
              📋 复制排障日志
            </button>
          </div>

          {/* 统一内联 6 位配对码补填栏（首次连接陌生电脑时零跳转就地完成配对并进桌面！） */}
          {pendingPair && (
            <div className={styles.pairPromptCard} role="region" aria-label="输入配对码完成连接">
              <div className={styles.hintTextCol}>
                <strong>🔐 {pendingPair.prompt}</strong>
                <span>请让对方查看软件主界面左侧的「一次性配对码（6位数字）」，输入后直接进入完整桌面：</span>
              </div>
              <div className={styles.planFormRow}>
                <input
                  type="text"
                  inputMode="numeric"
                  maxLength={6}
                  className={styles.codeInput}
                  value={pairCodeInput}
                  onChange={(e) =>
                    setPairCodeInput(e.currentTarget.value.replace(/\D/g, "").slice(0, 6))
                  }
                  placeholder="6位数字"
                  aria-label="输入对方 6 位配对码"
                />
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  disabled={connectingBusy || p2pCtrlBusy}
                  onClick={() => void handleSubmitPendingPair()}
                >
                  {connectingBusy || p2pCtrlBusy ? "验证并连接中…" : "⚡ 完成配对并进入桌面"}
                </button>
                <button
                  type="button"
                  className={styles.btn}
                  onClick={() => setPendingPair(null)}
                >
                  取消
                </button>
              </div>
            </div>
          )}

          {/* 智能寻优结论横幅 */}
          {smartHint && (
            <div
              className={
                smartHint.level === "warn" ? styles.hintBannerWarn : styles.hintBannerOk
              }
            >
              <div className={styles.hintTextCol}>
                <strong>{smartHint.title}</strong>
                <span>{smartHint.detail}</span>
              </div>
              <div className={styles.hintActions}>
                {smartHint.bestPlan && (
                  <button
                    type="button"
                    className={`${styles.btn} ${styles.btnPrimary}`}
                    onClick={() => void handleConnectPlan(smartHint.bestPlan!)}
                  >
                    🖥️ 立即用此方案连入桌面
                  </button>
                )}
                {smartHint.guideTarget && (
                  <button
                    type="button"
                    className={styles.btn}
                    onClick={() => setActiveGuide(smartHint.guideTarget!)}
                  >
                    切换到备选向导
                  </button>
                )}
                <button
                  type="button"
                  className={styles.btn}
                  onClick={() => setSmartHint(null)}
                >
                  关闭
                </button>
              </div>
            </div>
          )}

          {/* 2) 5 大远程方案卡片（点击任一卡片即切换下方【1→2→3 步实战向导】！） */}
          <div className={styles.familyGrid} aria-label="五大远程方案家族">
            {SCHEME_FAMILIES.map((fam) => {
              const st = familyStatus(fam.id);
              const selected = activeGuide === fam.id;
              return (
                <button
                  key={fam.id}
                  type="button"
                  aria-pressed={selected}
                  className={
                    selected
                      ? `${styles.familyCard} ${styles.familyCardActive}`
                      : styles.familyCard
                  }
                  onClick={() => setActiveGuide(fam.id)}
                >
                  <div className={styles.familyCardTop}>
                    <span className={styles.familyTitle}>{fam.title}</span>
                    <span className={styles.familyStatus} data-tone={st.tone}>
                      {st.text}
                    </span>
                  </div>
                  <span className={styles.familySub}>{fam.subtitle}</span>
                </button>
              );
            })}
          </div>

          {/* ── 3) 分场景 1→2→3 步可走通实战向导工作台 ── */}
          <div className={styles.guideWorkbench} aria-label="当前方案分步操作向导">
            {/* ===== 向导 A：局域网直连 ===== */}
            {activeGuide === "lan" && (
              <>
                <div className={styles.guideHeader}>
                  <div className={styles.guideTitleGroup}>
                    <h2 className={styles.guideTitle}>局域网直连 · 三步进入 4K@144Hz 桌面</h2>
                    <p className={styles.guideSubtitle}>
                      适用：两台设备连在同一个路由器 / WiFi / 办公室交换机下，零中转、延迟 &lt; 3ms。
                    </p>
                  </div>
                </div>
                <div className={styles.stepStack}>
                  <div className={styles.stepCard}>
                    <div className={styles.stepCardHead}>
                      <div className={styles.stepTitleRow}>
                        <span className={styles.stepNum}>第 1 步</span>
                        <span>自动扫描或输入对方局域网 IP</span>
                      </div>
                      <button
                        type="button"
                        className={styles.btn}
                        disabled={lanScanning}
                        onClick={() => void handleScanLan()}
                      >
                        {lanScanning ? "扫描中…" : "🔍 扫描同网段电脑"}
                      </button>
                    </div>
                    <div className={styles.planFormRow}>
                      <input
                        type="text"
                        className={styles.textInput}
                        value={lanIpInput || baseTargetIp}
                        onChange={(e) => setLanIpInput(e.currentTarget.value)}
                        placeholder="对方电脑局域网 IP（如 192.168.1.10，免填端口）"
                        aria-label="局域网目标 IP"
                      />
                      <input
                        type="text"
                        inputMode="numeric"
                        maxLength={6}
                        className={styles.smallInput}
                        value={lanPairCode}
                        onChange={(e) =>
                          setLanPairCode(e.currentTarget.value.replace(/\D/g, "").slice(0, 6))
                        }
                        placeholder="6位码(首连)"
                        aria-label="局域网首次连接 6 位配对码"
                      />
                    </div>
                    {lanFound.length > 0 && (
                      <div className={styles.presetRow}>
                        {lanFound.map((d) => (
                          <button
                            key={d.addr}
                            type="button"
                            className={styles.presetChip}
                            onClick={() => setLanIpInput(d.addr.replace(/:21118$/, ""))}
                          >
                            填入 {d.name} ({d.addr.replace(/:21118$/, "")})
                          </button>
                        ))}
                      </div>
                    )}
                  </div>

                  <div className={styles.stepCard}>
                    <div className={styles.stepCardHead}>
                      <div className={styles.stepTitleRow}>
                        <span className={styles.stepNum}>第 2 &amp; 3 步</span>
                        <span>实测连通延迟，或一键直入完整远程桌面</span>
                      </div>
                      <div className={styles.planFormRow}>
                        <button
                          type="button"
                          className={styles.btn}
                          disabled={testingPlanId !== null}
                          onClick={() => {
                            const ip = (lanIpInput || baseTargetIp).trim();
                            if (!ip) {
                              showToast("请先在第 1 步输入或扫描出对方 IP");
                              return;
                            }
                            void probeSinglePlan({
                              planId: "lan-guide-probe",
                              planName: `局域网直连 (${ip})`,
                              mode: "direct",
                              targetAddr: ip,
                              relayAddr: "",
                              rounds: 3,
                            });
                          }}
                        >
                          ⚡ 实测 QUIC 延迟
                        </button>
                        <button
                          type="button"
                          className={styles.btn}
                          onClick={() => {
                            const ip = (lanIpInput || baseTargetIp).trim();
                            if (!ip) {
                              showToast("请先输入对方局域网 IP");
                              return;
                            }
                            const plan: RealTestPlanDto = {
                              planId: `lan-${Date.now().toString(36)}`,
                              planName: `LAN 直连 (${ip})`,
                              mode: "direct",
                              targetAddr: ip,
                              relayAddr: "",
                              rounds: 3,
                            };
                            savePlansForTarget([...activePlans, plan]);
                            showToast("已保存到下方目标方案列表");
                          }}
                        >
                          💾 存为目标方案
                        </button>
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          disabled={connectingBusy}
                          onClick={() =>
                            void executeDirectOrMeshConnect({
                              targetAddr: (lanIpInput || baseTargetIp).trim(),
                              code: lanPairCode || undefined,
                            })
                          }
                        >
                          {connectingBusy ? "连接中…" : "🖥️ 立即直连完整桌面"}
                        </button>
                      </div>
                    </div>
                  </div>
                </div>
              </>
            )}

            {/* ===== 向导 B：P2P 打洞（零服务器公共 STUN 完整闭环！） ===== */}
            {activeGuide === "p2p" && (
              <>
                <div className={styles.guideHeader}>
                  <div className={styles.guideTitleGroup}>
                    <h2 className={styles.guideTitle}>
                      P2P 打洞直连 · 零服务器公共 STUN 实战向导
                    </h2>
                    <p className={styles.guideSubtitle}>
                      无需购买 VPS 服务器！利用小米 / B站等公共 STUN 探测双端公网洞口，点对点加密直连。
                    </p>
                  </div>
                </div>

                <div className={styles.guideExplainer}>
                  💡 <strong>为什么只绑定公共 STUN 还不能直接连？</strong>{" "}
                  因为公共 STUN 就像一面「公网镜子」，只告诉本机自己的公网 IP:端口，不保存对方信息。
                  正确用法只需两步：<strong>① 被控端点「生成打洞连接码」发给控制端 → ② 控制端粘贴连接码点「一键打洞连接」</strong>即可走通！
                </div>

                <div className={styles.stepStack}>
                  {/* 第 1 步：选定公共 STUN + NAT 体检 */}
                  <div className={natResult ? `${styles.stepCard} ${styles.stepCardDone}` : styles.stepCard}>
                    <div className={styles.stepCardHead}>
                      <div className={styles.stepTitleRow}>
                        <span className={natResult ? `${styles.stepNum} ${styles.stepNumDone}` : styles.stepNum}>
                          第 1 步
                        </span>
                        <span>优选低延迟公共 STUN 节点并检测 NAT 类型</span>
                      </div>
                      <button
                        type="button"
                        className={`${styles.btn} ${styles.btnPrimary}`}
                        disabled={natBusy}
                        onClick={() => void handleQuickSetupStun()}
                      >
                        {natBusy ? "正在拉取并测速…" : "⚡ 一键拉取国内高速 STUN + NAT 体检"}
                      </button>
                    </div>
                    <div className={styles.planFormRow}>
                      <span className={styles.fieldLabel}>当前打洞 STUN：</span>
                      <select
                        className={styles.select}
                        aria-label="选择打洞使用的公共 STUN 服务器"
                        value={selectedStunAddr}
                        onChange={(e) => setSelectedStunAddr(e.target.value)}
                      >
                        <option value="stun.miwifi.com:3478">小米公共 STUN (stun.miwifi.com:3478)</option>
                        <option value="stun.chat.bilibili.com:3478">B站公共 STUN (stun.chat.bilibili.com:3478)</option>
                        <option value="stun.hitv.com:3478">芒果TV STUN (stun.hitv.com:3478)</option>
                        <option value="stun.qq.com:3478">腾讯 QQ STUN (stun.qq.com:3478)</option>
                        <option value="stun.cloudflare.com:3478">Cloudflare STUN (stun.cloudflare.com:3478)</option>
                        {stunPoolNodes.map((n) => (
                          <option key={n.id} value={n.addr}>
                            {n.name} ({n.addr}){n.lastRttMs !== null ? ` · ${n.lastRttMs}ms` : ""}
                          </option>
                        ))}
                      </select>
                      {natResult && (
                        <span className={styles.natPill} data-cone={natResult.natType === "cone"}>
                          {natResult.label}
                        </span>
                      )}
                    </div>
                    {natResult && <p className={styles.stepHint}>{natResult.advice}</p>}
                  </div>

                  {/* 第 2 & 3 步：被控端 vs 控制端 双栏明确动作 */}
                  <div className={styles.stepCard}>
                    <div className={styles.stepTitleRow}>
                      <span className={styles.stepNum}>第 2 步</span>
                      <span>按这台电脑的角色，选择左边（被别人连）或右边（去连别人）</span>
                    </div>

                    <div className={styles.roleSplitGrid}>
                      {/* 左栏：被控端 */}
                      <div className={styles.roleCard}>
                        <span className={styles.roleBadgeHost}>角色 A · 这台是被控电脑（让别人控制我）</span>
                        <p className={styles.stepHint}>
                          点击下方按钮，软件会在本机开启打洞监听、锁定公网映射端口（自动每 12 秒保活）并附带一次性配对码：
                        </p>
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          disabled={p2pHostBusy}
                          onClick={() => void handleGenerateP2pHostCode()}
                        >
                          {p2pHostBusy ? "正在开启公网洞口…" : "🔑 生成本机 STUN 打洞连接码"}
                        </button>

                        {p2pHostInfo && (
                          <div className={styles.tokenBox}>
                            <div className={styles.planFormRow}>
                              <span className={styles.planAddrText}>
                                公网洞口：{p2pHostInfo.mappedAddr} · 内置配对码：{p2pHostInfo.pairCode}
                              </span>
                              <button
                                type="button"
                                className={styles.btn}
                                onClick={() => {
                                  void copyTextSafe(p2pHostInfo.token);
                                  showToast("打洞连接码已复制！请通过微信/QQ发给控制端电脑");
                                }}
                              >
                                📋 复制连接码发给对方
                              </button>
                            </div>
                            <code className={styles.tokenCode}>{p2pHostInfo.token}</code>
                          </div>
                        )}

                        <div className={styles.planFormRow}>
                          <input
                            type="text"
                            className={styles.textInput}
                            value={p2pReceiptInput}
                            onChange={(e) => setP2pReceiptInput(e.currentTarget.value)}
                            placeholder="若对方提示需双向开洞：粘贴对方的 rd-punch:// 回执码"
                            aria-label="粘贴控制端回执码协助双向开洞"
                          />
                          <button
                            type="button"
                            className={styles.btn}
                            disabled={p2pPunchingPeer || !p2pReceiptInput.trim()}
                            onClick={() => void handleHostAssistPunch()}
                          >
                            {p2pPunchingPeer ? "发脉冲中…" : "🔓 协助开洞"}
                          </button>
                        </div>
                      </div>

                      {/* 右栏：控制端 */}
                      <div className={styles.roleCard}>
                        <span className={styles.roleBadgeCtrl}>角色 B · 这台是控制端（我要连远程电脑）</span>
                        <p className={styles.stepHint}>
                          把对方电脑生成的 <code>rd-p2p://...</code> 打洞连接码粘贴到下方，点击按钮即可自动穿透并进入远程桌面：
                        </p>
                        <input
                          type="text"
                          className={styles.textInput}
                          value={p2pTokenInput}
                          onChange={(e) => setP2pTokenInput(e.currentTarget.value)}
                          placeholder="在此粘贴对方的 rd-p2p://... 打洞连接码"
                          aria-label="粘贴对方 STUN 打洞连接码"
                        />
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          disabled={p2pCtrlBusy}
                          onClick={() => void handleConnectByP2pToken()}
                        >
                          {p2pCtrlBusy
                            ? "正在竞速打洞握手中…"
                            : p2pReverseCode
                              ? "🚀 对方已点协助开洞 → 立即完成连接"
                              : "🚀 一键打洞并进入远程桌面"}
                        </button>

                        {p2pReverseCode && (
                          <div className={styles.reverseBox}>
                            <strong>
                              📡 双向洞口实时守候中（已发脉冲 {p2pLiveStatus?.txPulses ?? 0} 包 · 收包{" "}
                              {p2pLiveStatus?.rxPulses ?? 0} 包 · 本机 NAT:{" "}
                              {p2pLiveStatus?.ctrlNatType === "symmetric"
                                ? "对称型"
                                : p2pLiveStatus?.ctrlNatType === "cone"
                                  ? "圆锥型"
                                  : "检测中"}{" "}
                              / 对方 NAT:{" "}
                              {p2pLiveStatus?.hostNatType === "symmetric"
                                ? "对称型"
                                : p2pLiveStatus?.hostNatType === "cone"
                                  ? "圆锥型"
                                  : "默认"}
                              ）：
                            </strong>
                            <span>
                              {p2pLiveStatus?.detail ??
                                "请复制下方回执码发给被控端，粘贴到左栏点「🔓 协助开洞」——打通瞬间本页将自动进入桌面！"}
                            </span>
                            <div className={styles.planFormRow}>
                              <code className={styles.inlineCode}>{p2pReverseCode}</code>
                              <button
                                type="button"
                                className={styles.btn}
                                onClick={() => {
                                  void copyTextSafe(p2pReverseCode);
                                  showToast("回执码已复制！发给被控端点「协助开洞」即可自动连通");
                                }}
                              >
                                📋 复制回执码
                              </button>
                              <button
                                type="button"
                                className={styles.btn}
                                onClick={() => void handleCopyDiagnosticLog()}
                              >
                                📋 复制排障日志
                              </button>
                              {(p2pLiveStatus?.ctrlNatType === "symmetric" ||
                                p2pLiveStatus?.hostNatType === "symmetric") && (
                                <button
                                  type="button"
                                  className={`${styles.btn} ${styles.btnPrimary}`}
                                  onClick={() => setActiveGuide("mesh")}
                                >
                                  🌐 切到虚拟组网（100% 穿透对称型 NAT）
                                </button>
                              )}
                            </div>
                          </div>
                        )}
                      </div>
                    </div>
                  </div>
                </div>
              </>
            )}

            {/* ===== 向导 C：虚拟组网（EasyTier 零账号口令组网 + Tailscale / ZeroTier） ===== */}
            {activeGuide === "mesh" && (
              <>
                <div className={styles.guideHeader}>
                  <div className={styles.guideTitleGroup}>
                    <h2 className={styles.guideTitle}>
                      虚拟组网直连 · EasyTier 免注册口令组网 / Tailscale / ZeroTier
                    </h2>
                    <p className={styles.guideSubtitle}>
                      两台电脑即使都在对称型 NAT 后也能组进同一个虚拟局域网，免重启后台自动拉起，直接点成员进桌面！
                    </p>
                  </div>
                </div>

                <div className={styles.stepStack}>
                  {/* 第 1 步：内核准备 */}
                  <div className={etStatus?.installed ? `${styles.stepCard} ${styles.stepCardDone}` : styles.stepCard}>
                    <div className={styles.stepCardHead}>
                      <div className={styles.stepTitleRow}>
                        <span className={etStatus?.installed ? `${styles.stepNum} ${styles.stepNumDone}` : styles.stepNum}>
                          第 1 步
                        </span>
                        <span>
                          准备组网内核（状态：
                          {etStatus?.installed ? "✓ EasyTier 已就绪" : "未安装 EasyTier，或可直接使用已有 Tailscale/ZeroTier"}）
                        </span>
                      </div>
                      <button
                        type="button"
                        className={etStatus?.installed ? styles.btn : `${styles.btn} ${styles.btnPrimary}`}
                        disabled={etBusy}
                        onClick={() => void handleEtAction("install")}
                      >
                        {etBusy
                          ? "下载安装中…"
                          : etStatus?.installed
                            ? "✓ 重新检测 / 更新 EasyTier"
                            : "⬇️ 一键自动下载安装 EasyTier"}
                      </button>
                    </div>
                  </div>

                  {/* 第 2 步：创建或加入组网房间（免重启立即上线） */}
                  <div className={etStatus?.running ? `${styles.stepCard} ${styles.stepCardDone}` : styles.stepCard}>
                    <div className={styles.stepCardHead}>
                      <div className={styles.stepTitleRow}>
                        <span className={etStatus?.running ? `${styles.stepNum} ${styles.stepNumDone}` : styles.stepNum}>
                          第 2 步
                        </span>
                        <span>两台电脑用同一串「组网口令」加入虚拟局域网（免重启当场生效！）</span>
                      </div>
                      {etStatus?.running && (
                        <span className={styles.rttBadgeOk}>
                          🟢 运行中{etStatus.virtualIp ? ` · 本机 ${etStatus.virtualIp}` : ""}
                          {etStatus.noTun ? " (免管理员穿透模式)" : ""}
                        </span>
                      )}
                    </div>

                    <div className={styles.roleSplitGrid}>
                      <div className={styles.roleCard}>
                        <span className={styles.roleBadgeHost}>电脑 A（发起组网房间）</span>
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          disabled={etBusy}
                          onClick={() => void handleEtAction("invite")}
                        >
                          {etBusy ? "启动组网中…" : "🏠 创建组网房间并自动启动"}
                        </button>
                        {etInviteCode && (
                          <div className={styles.tokenBox}>
                            <div className={styles.planFormRow}>
                              <span className={styles.planAddrText}>把下方口令发给另一台电脑粘贴加入：</span>
                              <button
                                type="button"
                                className={styles.btn}
                                onClick={() => {
                                  void copyTextSafe(etInviteCode);
                                  showToast("组网口令已复制！");
                                }}
                              >
                                📋 复制口令
                              </button>
                            </div>
                            <code className={styles.tokenCode}>{etInviteCode}</code>
                          </div>
                        )}
                      </div>

                      <div className={styles.roleCard}>
                        <span className={styles.roleBadgeCtrl}>电脑 B（粘贴口令加入同一房间）</span>
                        <input
                          type="text"
                          className={styles.textInput}
                          value={etJoinInput}
                          onChange={(e) => setEtJoinInput(e.currentTarget.value)}
                          placeholder="在此粘贴对方发来的 rd-et://... 组网口令"
                          aria-label="粘贴 EasyTier 组网口令"
                        />
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          disabled={etBusy || !etJoinInput.trim()}
                          onClick={() => void handleEtAction("join")}
                        >
                          {etBusy ? "正在加入并启动…" : "🔗 加入房间并立即启动"}
                        </button>
                      </div>
                    </div>
                  </div>

                  {/* 第 3 步：扫描在线成员并一键连接桌面 */}
                  <div className={styles.stepCard}>
                    <div className={styles.stepCardHead}>
                      <div className={styles.stepTitleRow}>
                        <span className={styles.stepNum}>第 3 步</span>
                        <span>扫描在线虚拟组网设备（EasyTier / Tailscale / ZeroTier）→ 一键进桌面</span>
                      </div>
                      <button
                        type="button"
                        className={`${styles.btn} ${styles.btnPrimary}`}
                        disabled={scanningMesh}
                        onClick={() => void handleScanAllMeshPeers()}
                      >
                        {scanningMesh ? "扫描中…" : "🔄 刷新在线组网设备"}
                      </button>
                    </div>

                    {meshPeers.length > 0 && (
                      <ul className={styles.peerList}>
                        {meshPeers.map((p) => (
                          <li key={`${p.source}-${p.addr}`} className={styles.peerRow}>
                            <div className={styles.planInfo}>
                              <div className={styles.planTitleLine}>
                                <span className={styles.modeTag}>{p.source}</span>
                                <strong>{p.name}</strong>
                                <code className={styles.inlineCode}>{p.addr}</code>
                                {p.latencyMs !== null && (
                                  <span className={styles.rttBadgeOk}>{p.latencyMs.toFixed(1)} ms</span>
                                )}
                              </div>
                            </div>
                            <div className={styles.planActions}>
                              <button
                                type="button"
                                className={styles.btn}
                                onClick={() => {
                                  const mode: RemotePlanMode =
                                    p.source === "Tailscale"
                                      ? "tailscale"
                                      : p.source === "ZeroTier"
                                        ? "zerotier"
                                        : "easytier";
                                  savePlansForTarget([
                                    ...activePlans,
                                    {
                                      planId: `mesh-${Date.now().toString(36)}`,
                                      planName: `${p.source} · ${p.name}`,
                                      mode,
                                      targetAddr: p.addr,
                                      relayAddr: "",
                                      rounds: 3,
                                    },
                                  ]);
                                  showToast(`已将 ${p.name} 加入目标方案列表`);
                                }}
                              >
                                存为方案
                              </button>
                              <button
                                type="button"
                                className={`${styles.btn} ${styles.btnPrimary}`}
                                disabled={connectingBusy}
                                onClick={() =>
                                  void executeDirectOrMeshConnect({
                                    targetAddr: p.addr,
                                    name: p.name,
                                  })
                                }
                              >
                                🖥️ 连接桌面
                              </button>
                            </div>
                          </li>
                        ))}
                      </ul>
                    )}

                    <div className={styles.planFormRow}>
                      <input
                        type="text"
                        className={styles.textInput}
                        value={meshManualIp}
                        onChange={(e) => setMeshManualIp(e.currentTarget.value)}
                        placeholder="或直接输入对方虚拟网 IP（如 10.126.126.2 / 100.81.x.x / 10.147.x.x）"
                        aria-label="手动输入虚拟组网 IP"
                      />
                      <button
                        type="button"
                        className={`${styles.btn} ${styles.btnPrimary}`}
                        disabled={connectingBusy || !meshManualIp.trim()}
                        onClick={() =>
                          void executeDirectOrMeshConnect({ targetAddr: meshManualIp.trim() })
                        }
                      >
                        🖥️ 连接该虚拟 IP
                      </button>
                    </div>
                  </div>
                </div>
              </>
            )}

            {/* ===== 向导 D：自建中转（rd-relay · 9位设备ID秒连 · SSH一键部署） ===== */}
            {activeGuide === "relay" && (
              <>
                <div className={styles.guideHeader}>
                  <div className={styles.guideTitleGroup}>
                    <h2 className={styles.guideTitle}>
                      自建中转服务器 · 像 RustDesk 一样输 9 位 ID 跨网直连
                    </h2>
                    <p className={styles.guideSubtitle}>
                      双端填入同一个自建 `rd-relay` 地址后，自动优先尝试中转协调打洞直连，打不通零感知走 VPS 满速转发！
                    </p>
                  </div>
                </div>

                <div className={styles.stepStack}>
                  <div className={defaultRelayAddr ? `${styles.stepCard} ${styles.stepCardDone}` : styles.stepCard}>
                    <div className={styles.stepCardHead}>
                      <div className={styles.stepTitleRow}>
                        <span className={defaultRelayAddr ? `${styles.stepNum} ${styles.stepNumDone}` : styles.stepNum}>
                          第 1 步
                        </span>
                        <span>填写已有中转地址（保存即免重启上线），或用 SSH 一键自动部署到云服务器</span>
                      </div>
                      <button
                        type="button"
                        className={styles.btn}
                        onClick={() => setShowInlineSsh((v) => !v)}
                      >
                        {showInlineSsh ? "收起 SSH 部署向导" : "🚀 没有现成中转？SSH 一键部署"}
                      </button>
                    </div>
                    <div className={styles.planFormRow}>
                      <input
                        type="text"
                        className={styles.textInput}
                        value={relayInputAddr}
                        onChange={(e) => setRelayInputAddr(e.currentTarget.value)}
                        placeholder="自建中转地址（如 1.2.3.4:21118，双端填同一个）"
                        aria-label="自建中转服务器地址"
                      />
                      <button
                        type="button"
                        className={`${styles.btn} ${styles.btnPrimary}`}
                        disabled={savingRelay}
                        onClick={() => void handleSaveRelayHot()}
                      >
                        {savingRelay ? "保存上线中…" : "💾 保存并立即热上线"}
                      </button>
                    </div>

                    {showInlineSsh && (
                      <div className={styles.roleCard}>
                        <div className={styles.planFormRow}>
                          <input
                            type="text"
                            className={styles.textInput}
                            value={sshHost}
                            onChange={(e) => setSshHost(e.currentTarget.value)}
                            placeholder="云服务器公网 IP（如 47.x.x.x）"
                            aria-label="SSH 云服务器主机"
                          />
                          <input
                            type="text"
                            className={styles.smallInput}
                            value={sshPort}
                            onChange={(e) => setSshPort(e.currentTarget.value)}
                            placeholder="22"
                            aria-label="SSH 端口"
                          />
                          <input
                            type="text"
                            className={styles.smallInput}
                            value={sshUser}
                            onChange={(e) => setSshUser(e.currentTarget.value)}
                            placeholder="root"
                            aria-label="SSH 用户名"
                          />
                          <input
                            type="text"
                            className={styles.textInput}
                            value={sshKeyPath}
                            onChange={(e) => setSshKeyPath(e.currentTarget.value)}
                            placeholder="私钥路径（留空默认用 ~/.ssh 密钥）"
                            aria-label="SSH 私钥路径"
                          />
                          <button
                            type="button"
                            className={`${styles.btn} ${styles.btnPrimary}`}
                            disabled={sshBusy}
                            onClick={() => void handleSshDeploy()}
                          >
                            {sshBusy ? "正在上传部署…" : "🚀 一键部署并自动回填"}
                          </button>
                        </div>
                        {sshResult && (
                          <div style={{ marginTop: "8px", fontSize: "12px" }}>
                            <div style={{ color: sshResult.ok ? "var(--rd-ok)" : "#ff7b7b" }}>
                              {sshResult.summary}
                            </div>
                            {sshResult.log.trim().length > 0 && (
                              <pre
                                style={{
                                  margin: "6px 0 0",
                                  padding: "8px",
                                  maxHeight: "140px",
                                  overflow: "auto",
                                  fontSize: "11px",
                                  background: "rgba(0,0,0,0.3)",
                                  borderRadius: "6px",
                                  whiteSpace: "pre-wrap",
                                }}
                              >
                                {sshResult.log}
                              </pre>
                            )}
                          </div>
                        )}
                      </div>
                    )}
                  </div>

                  <div className={styles.stepCard}>
                    <div className={styles.roleSplitGrid}>
                      <div className={styles.roleCard}>
                        <span className={styles.roleBadgeHost}>第 2 步 · 本机 9 位 ID（让别人连我）</span>
                        <div className={styles.planFormRow}>
                          <code className={styles.bigIdCode}>
                            {relayIdInfo?.numericId ?? "--- --- ---"}
                          </code>
                          {relayIdInfo?.numericId && (
                            <button
                              type="button"
                              className={styles.btn}
                              onClick={() => {
                                void copyTextSafe(relayIdInfo.numericId.replace(/\s+/g, ""));
                                showToast(`已复制本机 9 位 ID：${relayIdInfo.numericId}`);
                              }}
                            >
                              📋 复制本机 ID
                            </button>
                          )}
                        </div>
                        <span className={styles.stepHint}>
                          {defaultRelayAddr
                            ? `已注册到 ${defaultRelayAddr}，把这 9 位数字 + 主界面 6 位配对码发给对方即可被控。`
                            : "请先完成第 1 步保存中转地址，本机 9 位 ID 将立即对外可连。"}
                        </span>
                      </div>

                      <div className={styles.roleCard}>
                        <span className={styles.roleBadgeCtrl}>第 3 步 · 输入对方 9 位 ID 连接远程桌面</span>
                        <div className={styles.planFormRow}>
                          <input
                            type="text"
                            className={styles.textInput}
                            value={peerNumericId}
                            onChange={(e) => setPeerNumericId(e.currentTarget.value)}
                            placeholder="输入对方 9 位设备 ID（如 482 916 305）"
                            aria-label="输入对方 9 位设备 ID"
                          />
                          <input
                            type="text"
                            inputMode="numeric"
                            maxLength={6}
                            className={styles.smallInput}
                            value={relayPairCode}
                            onChange={(e) =>
                              setRelayPairCode(e.currentTarget.value.replace(/\D/g, "").slice(0, 6))
                            }
                            placeholder="6位码(首连)"
                            aria-label="9位ID首次连接配对码"
                          />
                        </div>
                        <div className={styles.planFormRow}>
                          <button
                            type="button"
                            className={styles.btn}
                            onClick={() => void handleLookupNumericId()}
                          >
                            🔍 查在线状态
                          </button>
                          <button
                            type="button"
                            className={`${styles.btn} ${styles.btnPrimary}`}
                            disabled={connectingBusy}
                            onClick={() => void handleConnectNumericId()}
                          >
                            {connectingBusy ? "正在寻址连接…" : "🖥️ 通过 9 位 ID 连接桌面"}
                          </button>
                        </div>
                        {relayLookupRes && (
                          <span className={styles.planAddrText}>{relayLookupRes.detail}</span>
                        )}
                      </div>
                    </div>
                  </div>
                </div>
              </>
            )}

            {/* ===== 向导 E：轻量/无服务器穿透（frp / CF Workers / DERP） ===== */}
            {activeGuide === "tunnel" && (
              <>
                <div className={styles.guideHeader}>
                  <div className={styles.guideTitleGroup}>
                    <h2 className={styles.guideTitle}>
                      轻量 / 无服务器穿透 · frp 端口映射 / Cloudflare Workers / DERP
                    </h2>
                    <p className={styles.guideSubtitle}>
                      适用：已在被控端配置好 frpc UDP 21118 端口映射、或自建了 CF Workers / DERP 边缘节点。
                    </p>
                  </div>
                </div>
                <div className={styles.stepStack}>
                  <div className={styles.stepCard}>
                    <div className={styles.stepTitleRow}>
                      <span className={styles.stepNum}>第 1 ~ 3 步</span>
                      <span>选择穿透类型 → 实测延迟 → 保存方案或直接连入桌面</span>
                    </div>
                    <div className={styles.planFormRow}>
                      <select
                        className={styles.select}
                        aria-label="选择轻量穿透类型"
                        value={tunnelMode}
                        onChange={(e) =>
                          setTunnelMode(e.target.value as "frp" | "cf_worker" | "derp")
                        }
                      >
                        <option value="frp">frp 公网端口映射 (host:port)</option>
                        <option value="cf_worker">Cloudflare Workers (https://...)</option>
                        <option value="derp">DERP 中继节点 (https://...)</option>
                      </select>
                      <input
                        type="text"
                        className={styles.textInput}
                        value={tunnelAddrInput}
                        onChange={(e) => setTunnelAddrInput(e.currentTarget.value)}
                        placeholder={MODE_META[tunnelMode].placeholder}
                        aria-label="轻量穿透服务器地址"
                      />
                    </div>
                    <div className={styles.planFormRow}>
                      <button
                        type="button"
                        className={styles.btn}
                        onClick={async () => {
                          if (!tunnelAddrInput.trim()) {
                            showToast("请先填写穿透节点地址");
                            return;
                          }
                          const r = await labProbeNode(tunnelMode, tunnelAddrInput.trim());
                          setTunnelProbeText(r.detail);
                          pushLog(r.ok ? "ok" : "warn", r.detail);
                        }}
                      >
                        ⚡ 实测节点连通性
                      </button>
                      <button
                        type="button"
                        className={styles.btn}
                        onClick={() => {
                          if (!tunnelAddrInput.trim()) {
                            showToast("请先填写穿透节点地址");
                            return;
                          }
                          savePlansForTarget([
                            ...activePlans,
                            {
                              planId: `tun-${Date.now().toString(36)}`,
                              planName: `${MODE_META[tunnelMode].shortLabel} (${tunnelAddrInput.trim()})`,
                              mode: tunnelMode,
                              targetAddr: baseTargetIp || tunnelAddrInput.trim(),
                              relayAddr: tunnelAddrInput.trim(),
                              rounds: 3,
                            },
                          ]);
                          showToast("已绑定到下方目标方案列表！");
                        }}
                      >
                        💾 绑定到目标方案
                      </button>
                      {tunnelMode === "frp" && (
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          disabled={connectingBusy || !tunnelAddrInput.trim()}
                          onClick={() =>
                            void executeDirectOrMeshConnect({
                              targetAddr: tunnelAddrInput.trim(),
                              name: `frp (${tunnelAddrInput.trim()})`,
                            })
                          }
                        >
                          🖥️ 通过 frp 映射连接桌面
                        </button>
                      )}
                    </div>
                    {tunnelProbeText && (
                      <span className={styles.planAddrText}>{tunnelProbeText}</span>
                    )}
                  </div>
                </div>
              </>
            )}
          </div>

          {/* 4) 自定义方案新增/编辑表单（支持全部 9 种具体通道模式） */}
          {showAddPlan && (
            <div className={styles.planFormCard}>
              <div className={styles.planFormRow}>
                <select
                  className={styles.select}
                  aria-label="方案类型"
                  value={newPlanMode}
                  onChange={(e) => setNewPlanMode(e.target.value as RemotePlanMode)}
                >
                  {ALL_MODES.map((m) => (
                    <option key={m} value={m}>
                      {MODE_META[m].label}
                    </option>
                  ))}
                </select>
                <input
                  type="text"
                  className={styles.textInput}
                  value={newPlanName}
                  onChange={(e) => setNewPlanName(e.currentTarget.value)}
                  placeholder="方案名称（如：公司 Tailscale / 香港自建中转 / frp 映射）"
                  aria-label="自定义方案名称"
                />
              </div>
              <div className={styles.planFormRow}>
                {MODE_META[newPlanMode].needsRelayAddr ? (
                  <>
                    {serverPool.length > 0 && (
                      <select
                        className={styles.select}
                        aria-label="从节点池快速填入"
                        value={
                          serverPool.some((n) => n.addr === newPlanRelay) ? newPlanRelay : ""
                        }
                        onChange={(e) => {
                          if (e.target.value) setNewPlanRelay(e.target.value);
                        }}
                      >
                        <option value="">从共用节点池选择…</option>
                        {serverPool.map((n) => (
                          <option key={n.id} value={n.addr}>
                            {n.name} ({n.addr})
                            {n.lastRttMs !== null ? ` · ${n.lastRttMs}ms` : ""}
                          </option>
                        ))}
                      </select>
                    )}
                    <input
                      type="text"
                      className={styles.textInput}
                      value={newPlanRelay}
                      onChange={(e) => setNewPlanRelay(e.currentTarget.value)}
                      placeholder={MODE_META[newPlanMode].placeholder}
                      aria-label="节点或中转服务器地址"
                    />
                  </>
                ) : (
                  <input
                    type="text"
                    className={styles.textInput}
                    value={newPlanAddr}
                    onChange={(e) => setNewPlanAddr(e.currentTarget.value)}
                    placeholder={MODE_META[newPlanMode].placeholder}
                    aria-label="目标设备地址"
                  />
                )}
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  onClick={handleSaveCustomPlan}
                >
                  {editingPlanId ? "保存修改" : "保存方案"}
                </button>
                <button
                  type="button"
                  className={styles.btn}
                  onClick={() => {
                    setShowAddPlan(false);
                    setEditingPlanId(null);
                  }}
                >
                  取消
                </button>
              </div>
            </div>
          )}

          {/* 5) 当前目标的已绑定方案与一键直连榜单 */}
          <div className={styles.planListCard}>
            <div className={styles.sectionHeadRow}>
              <h2 className={styles.sectionHeading}>
                已绑定连接方案（{visiblePlans.length}）
                <button
                  type="button"
                  className={styles.linkMiniBtn}
                  onClick={() => setOnlyFilterFamily((v) => !v)}
                >
                  {onlyFilterFamily ? "显示全部方案" : "仅看当前选中分类"}
                </button>
              </h2>
              {natResult && (
                <span className={styles.natPill} data-cone={natResult.natType === "cone"}>
                  NAT：{natResult.label}
                </span>
              )}
            </div>

            {visiblePlans.length === 0 ? (
              <p className={styles.emptyHint}>
                当前筛选下暂无已存方案，可在上方实战向导中点「存为方案」或直接一键连接桌面。
              </p>
            ) : (
              <ul className={styles.planList}>
                {visiblePlans.map((plan) => {
                  const res = realResults[plan.planId];
                  const isConfirmed = confirmedPlanId === plan.planId;
                  const meta = MODE_META[plan.mode];
                  return (
                    <li
                      key={plan.planId}
                      className={
                        isConfirmed
                          ? `${styles.planRow} ${styles.planRowConfirmed}`
                          : styles.planRow
                      }
                    >
                      <div className={styles.planInfo}>
                        <div className={styles.planTitleLine}>
                          <span className={styles.modeTag}>{meta.shortLabel}</span>
                          <strong>{plan.planName}</strong>
                          {isConfirmed && <span className={styles.defaultTag}>当前默认</span>}
                        </div>
                        <span className={styles.planAddrText}>
                          {meta.needsRelayAddr
                            ? `节点：${plan.relayAddr || "未填写"}`
                            : `目标：${plan.targetAddr || baseTargetIp || "未填写"}`}
                          {res ? ` · ${res.routeDetail}` : ""}
                        </span>
                      </div>
                      <div className={styles.planMetrics}>
                        {res ? (
                          <span
                            className={res.ok ? styles.rttBadgeOk : styles.rttBadgeFail}
                          >
                            {res.ok && res.avgRttMs !== null
                              ? `${res.avgRttMs.toFixed(1)} ms · 丢包 ${res.lossPct}%`
                              : "不可达"}
                          </span>
                        ) : (
                          <span className={styles.rttBadgeIdle}>未测速</span>
                        )}
                      </div>
                      <div className={styles.planActions}>
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          disabled={connectingBusy}
                          onClick={() => void handleConnectPlan(plan)}
                        >
                          {plan.mode === "punch" ? "去打洞连接" : "🖥️ 连接桌面"}
                        </button>
                        <button
                          type="button"
                          className={styles.btn}
                          disabled={testingPlanId === plan.planId}
                          onClick={() => void probeSinglePlan(plan)}
                        >
                          {testingPlanId === plan.planId ? "测速中…" : "实测"}
                        </button>
                        {!isConfirmed && plan.mode !== "punch" && (
                          <button
                            type="button"
                            className={styles.btn}
                            onClick={() => void handleConfirmPlan(plan)}
                          >
                            设为默认
                          </button>
                        )}
                        <button
                          type="button"
                          className={styles.btn}
                          onClick={() => {
                            setEditingPlanId(plan.planId);
                            setNewPlanName(plan.planName);
                            setNewPlanMode(plan.mode);
                            setNewPlanAddr(plan.targetAddr);
                            setNewPlanRelay(plan.relayAddr);
                            setShowAddPlan(true);
                          }}
                        >
                          编辑
                        </button>
                        {activePlans.length > 1 && (
                          <button
                            type="button"
                            className={styles.btn}
                            onClick={() => handleDeletePlan(plan.planId)}
                          >
                            删除
                          </button>
                        )}
                      </div>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>

          {/* 6) 底部辅助工具抽屉（节点池与订阅 / SSH部署 / 实时日志） */}
          <div className={styles.drawerBar} role="toolbar" aria-label="高级连接工具">
            {(
              [
                ["pool", `🌐 共用节点池与公共订阅 (${serverPool.length})`],
                ["ssh", "🚀 SSH 一键部署自建中转"],
                ["logs", `📋 实时探测日志 (${eventLogs.length})`],
              ] as const
            ).map(([id, label]) => (
              <button
                key={id}
                type="button"
                className={
                  openDrawer === id ? `${styles.btn} ${styles.btnPrimary}` : styles.btn
                }
                aria-expanded={openDrawer === id}
                onClick={() => setOpenDrawer((cur) => (cur === id ? "none" : id))}
              >
                {label}
              </button>
            ))}
          </div>

          {/* 抽屉 A：共用节点池与公共订阅（按类型智能导流到对应实战向导！） */}
          {openDrawer === "pool" && (
            <div className={styles.drawerPanel}>
              <div className={styles.planFormRow}>
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  disabled={fetchingPublic}
                  onClick={() => void handleFetchPublicNodes()}
                >
                  {fetchingPublic ? "同步中…" : "拉取公共 STUN / EasyTier / DERP 节点"}
                </button>
                <button
                  type="button"
                  className={styles.btn}
                  disabled={probingPool || visiblePool.length === 0}
                  onClick={() => void handleProbeAllPool()}
                >
                  {probingPool ? "并发测速中…" : "一键并发测速并按延迟排序"}
                </button>
                <select
                  className={styles.select}
                  aria-label="筛选节点类型"
                  value={poolFilter}
                  onChange={(e) => setPoolFilter(e.target.value as "all" | PoolNodeKind)}
                >
                  <option value="all">全部节点 ({serverPool.length})</option>
                  <option value="stun">STUN 打洞</option>
                  <option value="relay">自建中转</option>
                  <option value="easytier">EasyTier</option>
                  <option value="cf_worker">CF Workers</option>
                  <option value="frp">frp 隧道</option>
                  <option value="derp">DERP 中继</option>
                </select>
                <input
                  type="text"
                  className={styles.textInput}
                  value={customSubUrl}
                  onChange={(e) => setCustomSubUrl(e.currentTarget.value)}
                  placeholder="可选：自定义 EasyTier / 节点订阅 URL"
                  aria-label="自定义节点订阅地址"
                />
              </div>

              <div className={styles.planFormRow}>
                <select
                  className={styles.select}
                  aria-label="批量导入类型"
                  value={batchKind}
                  onChange={(e) => setBatchKind(e.target.value as "auto" | PoolNodeKind)}
                >
                  <option value="auto">自动识别类型</option>
                  <option value="stun">STUN</option>
                  <option value="relay">自建中转</option>
                  <option value="easytier">EasyTier</option>
                  <option value="cf_worker">CF Workers</option>
                  <option value="frp">frp</option>
                  <option value="derp">DERP</option>
                </select>
                <input
                  type="text"
                  className={styles.textInput}
                  value={batchText}
                  onChange={(e) => setBatchText(e.currentTarget.value)}
                  placeholder="快速添加自定义节点（如 1.2.3.4:21118 或 https://xxx.workers.dev）"
                  aria-label="批量导入节点"
                />
                <button type="button" className={styles.btn} onClick={handleBatchImportNodes}>
                  加入节点池
                </button>
              </div>

              <ul className={styles.poolList}>
                {visiblePool.map((node) => {
                  const kind = inferNodeKind(node);
                  const actionLabel =
                    kind === "stun"
                      ? "⚡ 选为打洞 STUN → 去打洞向导"
                      : kind === "easytier"
                        ? "⚡ 选为引导节点 → 去组网向导"
                        : "⚡ 绑定为目标方案";
                  return (
                    <li key={node.id} className={styles.poolItem}>
                      <div className={styles.planInfo}>
                        <div className={styles.planTitleLine}>
                          <span className={styles.modeTag}>{kind.toUpperCase()}</span>
                          <strong>{node.name}</strong>
                          {node.region && (
                            <span className={styles.planAddrText}>· {node.region}</span>
                          )}
                        </div>
                        <span className={styles.planAddrText}>
                          {node.addr}
                          {node.mappedAddr ? ` · 本机公网映射 ${node.mappedAddr}` : ""}
                        </span>
                      </div>
                      <span
                        className={
                          node.status === "ok"
                            ? styles.rttBadgeOk
                            : node.status === "fail"
                              ? styles.rttBadgeFail
                              : styles.rttBadgeIdle
                        }
                      >
                        {node.lastRttMs !== null ? `${node.lastRttMs.toFixed(1)} ms` : node.status}
                      </span>
                      <div className={styles.planActions}>
                        <button
                          type="button"
                          className={styles.btn}
                          disabled={probingNodeId === node.id}
                          onClick={() => void handleProbeOneNode(node)}
                        >
                          测速
                        </button>
                        <button
                          type="button"
                          className={`${styles.btn} ${styles.btnPrimary}`}
                          onClick={() => void handleUsePoolNode(node)}
                        >
                          {actionLabel}
                        </button>
                      </div>
                    </li>
                  );
                })}
              </ul>
            </div>
          )}

          {/* 抽屉 B：SSH 一键部署自建中转 */}
          {openDrawer === "ssh" && (
            <div className={styles.drawerPanel}>
              <div className={styles.planFormRow}>
                <input
                  type="text"
                  className={styles.textInput}
                  value={sshHost}
                  onChange={(e) => setSshHost(e.currentTarget.value)}
                  placeholder="VPS 公网 IP 或域名（如 1.2.3.4）"
                  aria-label="SSH 服务器主机"
                />
                <input
                  type="text"
                  className={styles.smallInput}
                  value={sshPort}
                  onChange={(e) => setSshPort(e.currentTarget.value)}
                  placeholder="22"
                  aria-label="SSH 端口"
                />
                <input
                  type="text"
                  className={styles.smallInput}
                  value={sshUser}
                  onChange={(e) => setSshUser(e.currentTarget.value)}
                  placeholder="root"
                  aria-label="SSH 用户名"
                />
                <select
                  className={styles.select}
                  aria-label="部署模式"
                  value={sshMode}
                  onChange={(e) => setSshMode(e.target.value as "udp" | "docker")}
                >
                  <option value="udp">原生 rd-relay UDP (21118)</option>
                  <option value="docker">Docker HTTPS/WSS (443)</option>
                </select>
              </div>
              <div className={styles.planFormRow}>
                <input
                  type="text"
                  className={styles.textInput}
                  value={sshKeyPath}
                  onChange={(e) => setSshKeyPath(e.currentTarget.value)}
                  placeholder="私钥路径（留空自动使用 ~/.ssh/id_ed25519 或 id_rsa）"
                  aria-label="SSH 私钥路径"
                />
                <input
                  type="text"
                  className={styles.textInput}
                  value={sshName}
                  onChange={(e) => setSshName(e.currentTarget.value)}
                  placeholder="节点备注名（如：阿里云上海中转）"
                  aria-label="自建节点备注名"
                />
                {sshMode === "docker" && (
                  <input
                    type="text"
                    className={styles.textInput}
                    value={sshDomain}
                    onChange={(e) => setSshDomain(e.currentTarget.value)}
                    placeholder="已解析域名（如 relay.example.com）"
                    aria-label="Docker HTTPS 域名"
                  />
                )}
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  disabled={sshBusy}
                  onClick={() => void handleSshDeploy()}
                >
                  {sshBusy ? "正在上传并部署…" : "🚀 一键上传部署并自动回填"}
                </button>
              </div>
              {sshResult && (
                <div style={{ marginTop: "8px", fontSize: "12px" }}>
                  <div style={{ color: sshResult.ok ? "var(--rd-ok)" : "#ff7b7b" }}>
                    {sshResult.summary}
                  </div>
                  {sshResult.log.trim().length > 0 && (
                    <pre
                      style={{
                        margin: "6px 0 0",
                        padding: "8px",
                        maxHeight: "140px",
                        overflow: "auto",
                        fontSize: "11px",
                        background: "rgba(0,0,0,0.3)",
                        borderRadius: "6px",
                        whiteSpace: "pre-wrap",
                      }}
                    >
                      {sshResult.log}
                    </pre>
                  )}
                </div>
              )}
            </div>
          )}

          {/* 抽屉 C：实时探测日志 */}
          {openDrawer === "logs" && (
            <div className={styles.logPanel} role="log" aria-live="polite">
              {eventLogs.map((l) => (
                <div key={l.id} className={styles.logLine} data-level={l.level}>
                  <span className={styles.logTime}>[{l.time}]</span>
                  <span>{l.text}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {/* ── Layer 2：画质与传输底层调优台（4K@144Hz / 200Mbps / 示波器 / 1:1 微距镜 / A-B） ── */}
      {activeLayer === "tuning" && (
        <GeekTuningDeck
          deviceId={selectedDevice?.id ?? deviceId ?? null}
          drawer={drawer}
          onToast={showToast}
        />
      )}
    </section>
  );
}
