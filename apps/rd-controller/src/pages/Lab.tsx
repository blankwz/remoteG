/**
 * RD Geek Lab · 方案实验室（第一性原理分层架构）。
 *
 * 双层清晰结构，智能与自定义并重，辐射全部 5 大远程方案家族：
 * 1) 主视图「智能连接中心」：
 *    - 一键全方案智能寻优（自动 NAT 体检 + 多通道并发/序贯实测 + 自动推荐最快线路 + 一键采纳连接）
 *    - 5 大方案家族全景雷达（局域网直连 / P2P 打洞 / 自建中转 / 虚拟组网 Mesh / 轻量无服务器穿透）
 *    - 按目标自定义多方案绑定与切换（支持 9 种具体通道：LAN / STUN / rd-relay / EasyTier / Tailscale / ZeroTier / CF Workers / frp / DERP）
 *    - 按需展开的 4 个极简工具抽屉：节点池与订阅 / SSH 一键部署 / NAT·EasyTier·9位ID / 实时探测日志
 * 2) 独立视图「画质与传输调优（GeekTuningDeck）」：
 *    - 会话内抽屉模式（Ctrl+Shift+L）默认直达，完整保留 4K@144Hz/200Mbps 旋钮、BBR/RS-FEC、Netem、示波器、1:1 微距镜、A/B 对决与 Panic Reset。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  labDiagnoseNat,
  labFetchPublicNodes,
  labProbeNode,
  labProbeTargetPlan,
  type NatDiagnosisDto,
  type PoolNodeKind,
  type RealPlanProbeResultDto,
  type RealTestPlanDto,
  type RemotePlanMode,
} from "../api/lab";
import {
  connectDevice,
  easytierAutoInstall,
  easytierDetectStatus,
  easytierGenerateInvite,
  easytierJoinInvite,
  editDevice,
  getRelayAddr,
  listDevices,
  relayIdStatus,
  relayResolveId,
  setRelayAddr,
  type DeviceDto,
  type EasyTierStatusDto,
  type RelayIdStatusDto,
  type RelayResolveIdResultDto,
} from "../api/devices";
import { deployRelaySsh } from "../api/relayDeploy";
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

type ToolDrawerId = "none" | "pool" | "ssh" | "mesh" | "logs";

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
    }, 3400);
  }, []);

  // ── 目标设备与全局状态 ──
  const [savedDevices, setSavedDevices] = useState<DeviceDto[]>([]);
  const [targetDeviceId, setTargetDeviceId] = useState<string>(deviceId ?? "");
  const [customIp, setCustomIp] = useState<string>("");
  const [defaultRelayAddr, setDefaultRelayAddr] = useState<string>("");
  const [connectingTarget, setConnectingTarget] = useState(false);

  // ── 5 大家族过滤 & 工具抽屉 ──
  const [familyFilter, setFamilyFilter] = useState<SchemeFamilyId | "all">("all");
  const [openDrawer, setOpenDrawer] = useState<ToolDrawerId>("none");

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
    candidateNode?: RelayNodeItem;
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

  // ── NAT / EasyTier / 9位ID ──
  const [natBusy, setNatBusy] = useState(false);
  const [natResult, setNatResult] = useState<NatDiagnosisDto | null>(null);
  const [etStatus, setEtStatus] = useState<EasyTierStatusDto | null>(null);
  const [etBusy, setEtBusy] = useState(false);
  const [etInviteCode, setEtInviteCode] = useState("");
  const [etJoinInput, setEtJoinInput] = useState("");
  const [relayIdInfo, setRelayIdInfo] = useState<RelayIdStatusDto | null>(null);
  const [peerNumericId, setPeerNumericId] = useState("");
  const [relayLookupRes, setRelayLookupRes] = useState<RelayResolveIdResultDto | null>(null);

  // ── SSH 一键部署自建中转 ──
  const [sshName, setSshName] = useState("");
  const [sshHost, setSshHost] = useState("");
  const [sshPort, setSshPort] = useState("22");
  const [sshUser, setSshUser] = useState("root");
  const [sshKeyPath, setSshKeyPath] = useState("");
  const [sshMode, setSshMode] = useState<"udp" | "docker">("udp");
  const [sshDomain, setSshDomain] = useState("");
  const [sshBusy, setSshBusy] = useState(false);

  // ── 实时探测日志 ──
  const [eventLogs, setEventLogs] = useState<
    { id: number; time: string; level: "info" | "ok" | "warn"; text: string }[]
  >(() => [
    {
      id: 1,
      time: new Date().toLocaleTimeString("zh-CN", { hour12: false }),
      level: "info",
      text: "智能连接中心就绪：选择目标后点「一键全方案智能寻优」，自动覆盖直连/打洞/中转/组网/穿透 5 大方案。",
    },
  ]);
  const logSeqRef = useRef(2);
  const pushLog = useCallback((level: "info" | "ok" | "warn", text: string) => {
    const id = logSeqRef.current++;
    const time = new Date().toLocaleTimeString("zh-CN", { hour12: false });
    setEventLogs((prev) => [...prev, { id, time, level, text }].slice(-120));
  }, []);

  // ── 挂载初始化：设备列表、中继地址、EasyTier、9位ID ──
  useEffect(() => {
    void listDevices()
      .then((list) => {
        if (Array.isArray(list)) {
          setSavedDevices(list);
          if (!targetDeviceId && list[0]) {
            setTargetDeviceId(list[0].id);
          }
        }
      })
      .catch(() => undefined);
    void getRelayAddr()
      .then((r) => {
        const addr = typeof r === "string" ? r.trim() : "";
        setDefaultRelayAddr(addr);
        if (addr) {
          setServerPool((prev) => {
            if (prev.some((n) => n.addr === addr)) return prev;
            const kind = inferNodeKind({ addr, name: "默认中转" });
            const next: RelayNodeItem[] = [
              ...prev,
              { id: "default-relay", name: "默认中转节点", addr, kind, lastRttMs: null, status: "idle" },
            ];
            try {
              window.localStorage.setItem("rd.labServerPool", JSON.stringify(next));
            } catch {
              // ignore
            }
            return next;
          });
        }
      })
      .catch(() => undefined);
    void easytierDetectStatus()
      .then((st) => {
        if (st && typeof st === "object") {
          setEtStatus(st);
          if (st.inviteCode) setEtInviteCode(st.inviteCode);
        }
      })
      .catch(() => undefined);
    void relayIdStatus()
      .then((info) => {
        if (info && typeof info === "object") setRelayIdInfo(info);
      })
      .catch(() => undefined);
  }, [targetDeviceId]);

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

  // ── 确认并启用某个方案（同时写回设备中继/地址配置） ──
  const handleConfirmPlan = useCallback(
    async (plan: RealTestPlanDto) => {
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
            plan.targetAddr.trim() || selectedDevice.addr.replace(/:21118$/, "");
          const nextRelay = MODE_META[plan.mode].needsRelayAddr ? plan.relayAddr.trim() : "";
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

  // ── 使用当前已确认方案直连完整桌面 ──
  const handleConnectWithConfirmed = useCallback(async () => {
    if (!selectedDevice) return;
    setConnectingTarget(true);
    try {
      if (!selectedDevice.online) {
        await connectDevice(selectedDevice.id);
      }
      window.location.hash = `#/session/${selectedDevice.id}?open=${DESKTOP_PRIMARY_ID}`;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `连接 ${selectedDevice.name} 失败：${msg}`);
      showToast(`连接失败：${msg}`);
    } finally {
      setConnectingTarget(false);
    }
  }, [pushLog, selectedDevice, showToast]);

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
      if (res?.ok && (res.avgRttMs ?? 9999) < bestRtt) {
        bestRtt = res.avgRttMs ?? 9999;
        bestPlan = plan;
      }
    }
    if (bestPlan) {
      setSmartHint({
        level: "ok",
        title: `最优已绑定方案：${bestPlan.planName}（${bestRtt.toFixed(1)} ms）`,
        detail: "已完成多轮真实握手探测，点击右侧按钮一键切换为默认线路。",
        bestPlan,
      });
    } else {
      setSmartHint({
        level: "warn",
        title: "当前已绑定方案均不可达",
        detail: "建议点击「一键全方案智能寻优」自动尝试公共 STUN / EasyTier / DERP 候选节点，或通过 SSH 一键部署中转。",
      });
    }
    setRunningBatch(false);
  }, [activePlans, probeSinglePlan, pushLog]);

  // ── 一键全方案智能寻优（自动覆盖：NAT 体检 + 已绑定方案 + 节点池候选） ──
  const runSmartAutoRace = useCallback(async () => {
    setSmartRacing(true);
    setSmartHint(null);
    pushLog("info", "⚡ 启动全方案智能寻优：自动探测直连 / 打洞 / 中转 / 组网 / 穿透…");

    let bestPlan: RealTestPlanDto | undefined;
    let bestPlanRtt = Number.POSITIVE_INFINITY;

    // 1) 实测所有已绑定方案
    for (const plan of activePlans) {
      const res = await probeSinglePlan(plan);
      if (res?.ok && (res.avgRttMs ?? 9999) < bestPlanRtt) {
        bestPlanRtt = res.avgRttMs ?? 9999;
        bestPlan = plan;
      }
    }

    // 2) 若节点池为空，自动拉取内置+公共节点（STUN / EasyTier / DERP）
    let pool = serverPool;
    if (pool.length === 0) {
      pushLog("info", "节点池为空，自动载入内置 STUN / DERP / EasyTier 公共节点…");
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
        // ignore offline
      }
    }

    // 3) 若未做 NAT 体检，自动双 STUN 体检
    if (!natResult) {
      const stuns = pool
        .filter((n) => inferNodeKind(n) === "stun")
        .slice(0, 2)
        .map((n) => n.addr);
      if (stuns.length >= 2) {
        try {
          const diag = await labDiagnoseNat(stuns);
          setNatResult(diag);
          pushLog("info", `自动 NAT 体检：${diag.label}（${diag.advice}）`);
        } catch {
          // ignore
        }
      }
    }

    // 4) 并发抽取最快候选节点（每类取前 2 个，避免慢卡）
    const candidates = pool.slice(0, 6);
    let bestCandidate: RelayNodeItem | undefined;
    let bestCandRtt = Number.POSITIVE_INFINITY;
    if (candidates.length > 0) {
      const probed = await Promise.all(
        candidates.map(async (node) => {
          const k = inferNodeKind(node);
          try {
            const r = await labProbeNode(k, node.addr);
            return {
              ...node,
              kind: k,
              ok: r.ok,
              lastRttMs: r.rttMs,
              mappedAddr: r.mappedAddr,
              status: r.ok ? ("ok" as const) : ("fail" as const),
            };
          } catch {
            return { ...node, ok: false, status: "fail" as const };
          }
        }),
      );
      const mapById = new Map(probed.map((p) => [p.id, p]));
      saveServerPool(pool.map((n) => mapById.get(n.id) ?? n));
      for (const p of probed) {
        if (p.status === "ok" && (p.lastRttMs ?? 9999) < bestCandRtt) {
          bestCandRtt = p.lastRttMs ?? 9999;
          bestCandidate = p;
        }
      }
    }

    if (bestPlan && bestPlanRtt <= bestCandRtt + 15) {
      setSmartHint({
        level: "ok",
        title: `推荐最优方案：${bestPlan.planName}（${bestPlanRtt.toFixed(1)} ms）`,
        detail: "该方案已绑定且实测延迟最低，可直接切换并进入 4K@144Hz 桌面。",
        bestPlan,
      });
    } else if (bestCandidate) {
      setSmartHint({
        level: "suggest",
        title: `发现更快候选节点：${bestCandidate.name}（${bestCandRtt.toFixed(1)} ms）`,
        detail: `地址 ${bestCandidate.addr}，一键采纳将自动绑定为当前目标方案并设为默认。`,
        candidateNode: bestCandidate,
      });
    } else {
      setSmartHint({
        level: "warn",
        title: "暂未探测到可用直连或公共节点",
        detail: "可展开下方「SSH 一键部署自建中转」或填入 Cloudflare Workers / frp / ZeroTier 地址。",
      });
    }
    setSmartRacing(false);
  }, [
    activePlans,
    customSubUrl,
    natResult,
    probeSinglePlan,
    pushLog,
    saveServerPool,
    serverPool,
  ]);

  // ── 一键采纳候选节点为当前目标方案 ──
  const handleAdoptCandidate = useCallback(
    async (node: RelayNodeItem) => {
      const kind = inferNodeKind(node);
      const mode: RemotePlanMode =
        kind === "stun"
          ? "punch"
          : kind === "easytier"
            ? "easytier"
            : kind === "cf_worker"
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
      setSmartHint(null);
    },
    [activePlans, baseTargetIp, handleConfirmPlan, savePlansForTarget],
  );

  // ── 保存新增 / 编辑自定义方案 ──
  const handleSaveCustomPlan = useCallback(() => {
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
      pushLog("ok", `已绑定新方案「${plan.planName}」（${meta.shortLabel}）`);
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
    serverPool,
    showToast,
  ]);

  const handleDeletePlan = useCallback(
    (planId: string) => {
      const next = activePlans.filter((p) => p.planId !== planId);
      savePlansForTarget(next);
      pushLog("info", "已移除该绑定方案");
    },
    [activePlans, pushLog, savePlansForTarget],
  );

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
      .sort((a, b) => {
        if (a.favorite && !b.favorite) return -1;
        if (!a.favorite && b.favorite) return 1;
        return (a.lastRttMs ?? 99999) - (b.lastRttMs ?? 99999);
      });
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

  // ── NAT / EasyTier / 9位ID 动作 ──
  const handleDiagnoseNat = useCallback(async () => {
    setNatBusy(true);
    try {
      const stuns = serverPool
        .filter((n) => inferNodeKind(n) === "stun")
        .slice(0, 2)
        .map((n) => n.addr);
      const addrs =
        stuns.length >= 2
          ? stuns
          : ["stun.chat.bilibili.com:3478", "stun.hitv.com:3478"];
      const res = await labDiagnoseNat(addrs);
      setNatResult(res);
      pushLog("ok", `NAT 体检完成：${res.label} — ${res.advice}`);
    } catch (e) {
      showToast(`NAT 体检失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setNatBusy(false);
    }
  }, [pushLog, serverPool, showToast]);

  const handleEtAction = useCallback(
    async (action: "install" | "invite" | "join") => {
      setEtBusy(true);
      try {
        if (action === "install") {
          const res = await easytierAutoInstall();
          showToast(res.detail || "EasyTier 已就绪");
        } else if (action === "invite") {
          const peers = serverPool
            .filter((n) => inferNodeKind(n) === "easytier")
            .slice(0, 3)
            .map((n) => n.addr);
          const res = await easytierGenerateInvite(peers);
          setEtInviteCode(res.inviteCode);
          showToast("已生成 EasyTier 组网邀请码");
        } else if (action === "join" && etJoinInput.trim()) {
          await easytierJoinInvite(etJoinInput.trim());
          showToast("已加入 EasyTier 虚拟网络");
        }
        const st = await easytierDetectStatus();
        setEtStatus(st);
      } catch (e) {
        showToast(`EasyTier 操作失败：${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setEtBusy(false);
      }
    },
    [etJoinInput, serverPool, showToast],
  );

  const handleLookupNumericId = useCallback(async () => {
    const id = peerNumericId.replace(/\s+/g, "").trim();
    const relay = relayIdInfo?.relayAddr || defaultRelayAddr;
    if (!id) return;
    if (!relay) {
      showToast("请先配置或通过 SSH 一键部署自建中转地址");
      return;
    }
    try {
      const res = await relayResolveId(relay, id);
      setRelayLookupRes(res);
      pushLog("info", `9位ID ${id} 查询结果：${res.detail}`);
    } catch (e) {
      showToast(`9位ID 查询失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }, [defaultRelayAddr, peerNumericId, pushLog, relayIdInfo?.relayAddr, showToast]);

  // ── SSH 一键部署自建中转 ──
  const handleSshDeploy = useCallback(async () => {
    if (!sshHost.trim()) {
      showToast("请先填写服务器公网 IP 或域名");
      return;
    }
    setSshBusy(true);
    try {
      const res = await deployRelaySsh({
        host: sshHost.trim(),
        port: Number(sshPort) || 22,
        user: sshUser.trim() || "root",
        keyPath: sshKeyPath.trim(),
        relayPort: 21118,
        mode: sshMode,
        domain: sshDomain.trim(),
      });
      if (res.ok && res.relayAddr) {
        await setRelayAddr(res.relayAddr);
        setDefaultRelayAddr(res.relayAddr);
        saveServerPool([
          ...serverPool,
          {
            id: `ssh-${Date.now().toString(36)}`,
            name: sshName.trim() || `自建中转 (${sshHost.trim()})`,
            addr: res.relayAddr,
            kind: "relay",
            lastRttMs: null,
            status: "ok",
          },
        ]);
        pushLog("ok", `SSH 一键部署成功：${res.relayAddr}（已自动保存到全局中继与节点池）`);
        showToast(`部署成功：${res.relayAddr}`);
      } else {
        showToast(res.summary || "SSH 部署未完成，请检查日志");
      }
    } catch (e) {
      showToast(`SSH 部署失败：${e instanceof Error ? e.message : String(e)}`);
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

  // ── 计算 5 大家族全景雷达状态 ──
  const familyStatus = (famId: SchemeFamilyId) => {
    const famPlans = activePlans.filter((p) => familyOfMode(p.mode) === famId);
    const okResults = famPlans
      .map((p) => realResults[p.planId])
      .filter((r): r is RealPlanProbeResultDto => Boolean(r?.ok));
    if (okResults.length > 0) {
      const best = Math.min(...okResults.map((r) => r.avgRttMs ?? 999));
      return { tone: "ok" as const, text: `可达 ${best.toFixed(0)}ms` };
    }
    if (famPlans.length > 0) {
      return { tone: "ready" as const, text: `已绑定 ${famPlans.length} 个` };
    }
    const poolCount = serverPool.filter((n) => {
      const k = inferNodeKind(n);
      if (famId === "p2p") return k === "stun";
      if (famId === "relay") return k === "relay";
      if (famId === "mesh") return k === "easytier";
      if (famId === "tunnel") return k === "cf_worker" || k === "frp" || k === "derp";
      return false;
    }).length;
    if (poolCount > 0) {
      return { tone: "pool" as const, text: `节点池 ${poolCount} 个` };
    }
    return { tone: "idle" as const, text: "点击自定义" };
  };

  const visiblePlans =
    familyFilter === "all"
      ? activePlans
      : activePlans.filter((p) => familyOfMode(p.mode) === familyFilter);

  const visiblePool = serverPool.filter(
    (n) => poolFilter === "all" || inferNodeKind(n) === poolFilter,
  );

  return (
    <section
      className={drawer ? styles.labDrawer : styles.labPage}
      aria-label="方案实验室极客操控台"
    >
      {/* ── 顶部清晰双层导航栏 ── */}
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
              {activeLayer === "tuning" ? "▲ 收起高级区（画质调优）" : "▼ 展开高级调优（画质/弱网/示波器）"}
            </button>
          </div>
        </div>

        <div className={styles.actionGroup}>
          {selectedDevice && (
            <button
              type="button"
              className={`${styles.btn} ${styles.btnPrimary}`}
              disabled={connectingTarget}
              onClick={() => void handleConnectWithConfirmed()}
              title="用当前已确认的最优方案直接进入完整桌面"
            >
              {connectingTarget ? "连接中…" : `🖥️ 连接 ${selectedDevice.name}`}
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

      {/* ── Layer 1：智能连接中心（5 大远程方案全覆盖 · 自动寻优 + 自定义） ── */}
      {activeLayer === "connectivity" && (
        <div className={styles.centerBody}>
          {/* 1) 目标选择 + 一键全方案智能寻优主操作条 */}
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
              <option value="">自定义目标 IP / 9位ID</option>
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
              placeholder="输入任意目标 IP（LAN / 100.x Tailscale / ZeroTier，免填端口）"
              aria-label="自定义测试目标 IP"
            />
            <button
              type="button"
              className={`${styles.btn} ${styles.btnPrimary}`}
              disabled={smartRacing || runningBatch || testingPlanId !== null}
              onClick={() => void runSmartAutoRace()}
              title="自动体检 NAT + 实测已绑定与候选节点，一键找出最快稳定通道"
            >
              {smartRacing ? "⏳ 智能寻优中…" : "✨ 一键全方案智能寻优"}
            </button>
            <button
              type="button"
              className={styles.btn}
              disabled={smartRacing || runningBatch || testingPlanId !== null}
              onClick={() => void runBoundPlansBatch()}
            >
              {runningBatch ? "测速中…" : "▶ 测已绑定"}
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
          </div>

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
                    onClick={() => void handleConfirmPlan(smartHint.bestPlan!)}
                  >
                    ⚡ 一键应用为默认
                  </button>
                )}
                {smartHint.candidateNode && (
                  <button
                    type="button"
                    className={`${styles.btn} ${styles.btnPrimary}`}
                    onClick={() => void handleAdoptCandidate(smartHint.candidateNode!)}
                  >
                    ⚡ 一键采纳并启用
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

          {/* 2) 5 大远程方案全景覆盖雷达 */}
          <div className={styles.familyGrid} aria-label="五大远程方案家族">
            {SCHEME_FAMILIES.map((fam) => {
              const st = familyStatus(fam.id);
              const selected = familyFilter === fam.id;
              return (
                <button
                  key={fam.id}
                  type="button"
                  className={
                    selected
                      ? `${styles.familyCard} ${styles.familyCardActive}`
                      : styles.familyCard
                  }
                  onClick={() =>
                    setFamilyFilter((cur) => (cur === fam.id ? "all" : fam.id))
                  }
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

          {/* 3) 自定义方案新增/编辑表单（支持全部 9 种具体通道模式） */}
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
                  placeholder="方案名称（如：公司 Tailscale / 香港自建中转 / CF Workers）"
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
                          serverPool.some((n) => n.addr === newPlanRelay)
                            ? newPlanRelay
                            : ""
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

          {/* 4) 当前目标的已绑定方案与实测榜单 */}
          <div className={styles.planListCard}>
            <div className={styles.sectionHeadRow}>
              <h2 className={styles.sectionHeading}>
                当前目标连接方案（{visiblePlans.length}）
                {familyFilter !== "all" && (
                  <button
                    type="button"
                    className={styles.linkMiniBtn}
                    onClick={() => setFamilyFilter("all")}
                  >
                    显示全部 5 类
                  </button>
                )}
              </h2>
              {natResult && (
                <span className={styles.natPill} data-cone={natResult.natType === "cone"}>
                  NAT：{natResult.label}
                </span>
              )}
            </div>

            {visiblePlans.length === 0 ? (
              <p className={styles.emptyHint}>
                该分类下暂无自定义方案，点击右上角「+ 自定义方案」即可按需绑定。
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
                          className={styles.btn}
                          disabled={testingPlanId === plan.planId}
                          onClick={() => void probeSinglePlan(plan)}
                        >
                          {testingPlanId === plan.planId ? "测速中…" : "实测"}
                        </button>
                        {!isConfirmed && (
                          <button
                            type="button"
                            className={`${styles.btn} ${styles.btnPrimary}`}
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

          {/* 5) 按需展开的 4 个极简工具抽屉（零视觉噪音） */}
          <div className={styles.drawerBar} role="toolbar" aria-label="高级连接工具">
            {(
              [
                ["pool", `🌐 共用节点池 (${serverPool.length})`],
                ["ssh", "🚀 SSH 一键部署中转"],
                ["mesh", "🕸️ NAT 体检 · EasyTier · 9位ID"],
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

          {/* 抽屉 A：共用节点池与公共订阅 */}
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
                  {probingPool ? "并发测速中…" : "一键并发测速并排序"}
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
                  placeholder="快速添加节点（如 https://xxx.workers.dev 或 1.2.3.4:21118 节点名）"
                  aria-label="批量导入节点"
                />
                <button type="button" className={styles.btn} onClick={handleBatchImportNodes}>
                  加入节点池
                </button>
              </div>

              <ul className={styles.poolList}>
                {visiblePool.map((node) => (
                  <li key={node.id} className={styles.poolItem}>
                    <div className={styles.planInfo}>
                      <div className={styles.planTitleLine}>
                        <span className={styles.modeTag}>
                          {inferNodeKind(node).toUpperCase()}
                        </span>
                        <strong>{node.name}</strong>
                        {node.region && (
                          <span className={styles.planAddrText}>· {node.region}</span>
                        )}
                      </div>
                      <span className={styles.planAddrText}>
                        {node.addr}
                        {node.mappedAddr ? ` · 公网映射 ${node.mappedAddr}` : ""}
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
                        className={styles.btn}
                        onClick={() => void handleAdoptCandidate(node)}
                      >
                        绑给当前目标
                      </button>
                    </div>
                  </li>
                ))}
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
            </div>
          )}

          {/* 抽屉 C：NAT 体检 · EasyTier 一键组网 · 9位ID 查寻 */}
          {openDrawer === "mesh" && (
            <div className={styles.drawerPanel}>
              <div className={styles.planFormRow}>
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  disabled={natBusy}
                  onClick={() => void handleDiagnoseNat()}
                >
                  {natBusy ? "体检中…" : "双 STUN 自动 NAT 体检"}
                </button>
                {natResult && (
                  <span className={styles.planAddrText}>
                    {natResult.label} · {natResult.advice}
                  </span>
                )}
              </div>
              <div className={styles.planFormRow}>
                <button
                  type="button"
                  className={styles.btn}
                  disabled={etBusy}
                  onClick={() => void handleEtAction("install")}
                >
                  {etStatus?.installed ? "EasyTier 已安装" : "一键安装 EasyTier"}
                </button>
                <button
                  type="button"
                  className={styles.btn}
                  disabled={etBusy}
                  onClick={() => void handleEtAction("invite")}
                >
                  生成组网邀请码
                </button>
                {etInviteCode && (
                  <code className={styles.inlineCode}>{etInviteCode}</code>
                )}
                <input
                  type="text"
                  className={styles.textInput}
                  value={etJoinInput}
                  onChange={(e) => setEtJoinInput(e.currentTarget.value)}
                  placeholder="输入对方 EasyTier 邀请码加入同一虚拟局域网"
                  aria-label="EasyTier 邀请码"
                />
                <button
                  type="button"
                  className={styles.btn}
                  disabled={etBusy || !etJoinInput.trim()}
                  onClick={() => void handleEtAction("join")}
                >
                  加入组网
                </button>
              </div>
              <div className={styles.planFormRow}>
                <span className={styles.planAddrText}>
                  本机 9 位 ID：{relayIdInfo?.numericId ?? "配置中转后自动分配"}
                </span>
                <input
                  type="text"
                  className={styles.textInput}
                  value={peerNumericId}
                  onChange={(e) => setPeerNumericId(e.currentTarget.value)}
                  placeholder="查询对方 9 位设备 ID 在线状态"
                  aria-label="查询对方 9 位 ID"
                />
                <button
                  type="button"
                  className={styles.btn}
                  onClick={() => void handleLookupNumericId()}
                >
                  查询 9 位 ID
                </button>
                {relayLookupRes && (
                  <span className={styles.planAddrText}>
                    {relayLookupRes.online
                      ? `在线 · ${relayLookupRes.detail}`
                      : "未在线"}
                  </span>
                )}
              </div>
            </div>
          )}

          {/* 抽屉 D：实时探测日志 */}
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
