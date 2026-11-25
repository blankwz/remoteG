/**
 * RD Geek Lab · 方案实验室（v0.4 原生极客操控台）。
 *
 * 一处实现，双态复用：
 * 1) 独立窗口/页面 `#/lab`（全屏五脏俱全：四组真旋钮 + 链路解剖 + 60Hz 示波器 + 1:1 微距像素检视镜 + A/B 矩阵对决）
 * 2) Session 远控页右侧抽屉（`Ctrl+Shift+L` 呼出，边控远程窗口边拨旋钮看波形与微距锐度）
 */
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent,
} from "react";
import {
  labApplyTransport,
  labApplyTuning,
  labBenchmarkAllRoutes,
  labDiagnoseNat,
  labFetchPublicNodes,
  labGetState,
  labListProfiles,
  labPanicReset,
  labProbeNode,
  labProbeTargetPlan,
  labRunAbBench,
  labSaveProfile,
  labSetNetem,
  labStartSandbox,
  labStopSandbox,
  type LabAbResultDto,
  type LabAllRoutesBenchDto,
  type LabCodecConfig,
  type LabLiveStateDto,
  type LabProfile,
  type LabTransportConfig,
  type NatDiagnosisDto,
  type NetemConfigDto,
  type RealPlanProbeResultDto,
  type RealTestPlanDto,
} from "../api/lab";
import { isTauriRuntime } from "../api/bridge";
import {
  connectDevice,
  easytierAutoInstall,
  easytierDetectStatus,
  easytierGenerateInvite,
  easytierJoinInvite,
  editDevice,
  getRelayAddr,
  listDevices,
  probeDevice,
  relayIdStatus,
  relayResolveId,
  setRelayAddr,
  tailscalePing,
  type DeviceDto,
  type EasyTierStatusDto,
  type RelayIdStatusDto,
  type RelayResolveIdResultDto,
} from "../api/devices";
import { deployRelaySsh } from "../api/relayDeploy";
import {
  CanvasTileSurface,
  FRAME_INVALIDATE,
  FRAME_TILE,
  FRAME_VIDEO,
  parseTileHeader,
  TILE_HEADER_LEN,
  TileLayer,
  tilePayloadLen,
} from "../session/TileLayer";
import {
  H264Decoder,
  parseVideoHeader,
  VIDEO_HEADER_LEN,
  type DecodeStats,
} from "../video/decoder";
import styles from "./Lab.module.css";

interface LabProps {
  /** 指定操控的设备 ID；省略时自动附着当前活跃设备或回落本机真栈沙盒。 */
  deviceId?: string | null;
  /** 是否作为 Session 内嵌极客抽屉展示。 */
  drawer?: boolean;
  /** 抽屉模式下的关闭回调。 */
  onClose?: () => void;
}

const DEFAULT_TRANSPORT: LabTransportConfig = {
  routeMode: "auto",
  congestion: "bbr",
  mediaFec: false,
  fecOverheadPct: 20,
};

const DEFAULT_CODEC: LabCodecConfig = {
  manualLock: false,
  bitrateKbps: 8000,
  maxFps: 60,
  preferHw: true,
  request444: false,
  tileEnabled: true,
  tileStillMs: 300,
  tileBudgetPct: 50,
};

const DEFAULT_NETEM: NetemConfigDto = {
  enabled: false,
  delayMs: 0,
  jitterMs: 0,
  lossPct: 0,
  burstLen: 1,
  rateKbps: 0,
};

const ROUTE_OPTIONS = [
  { key: "auto", label: "AUTO 竞速" },
  { key: "direct", label: "LAN 直连" },
  { key: "punch", label: "STUN 打洞" },
  { key: "relay", label: "Native 中继" },
  { key: "easytier", label: "EasyTier" },
] as const;

const NETEM_PRESETS: {
  name: string;
  cfg: NetemConfigDto;
}[] = [
  {
    name: "关闭 (0损)",
    cfg: { enabled: false, delayMs: 0, jitterMs: 0, lossPct: 0, burstLen: 1, rateKbps: 0 },
  },
  {
    name: "跨城晚高峰 (60ms/2%)",
    cfg: { enabled: true, delayMs: 60, jitterMs: 12, lossPct: 2.0, burstLen: 2, rateKbps: 15000 },
  },
  {
    name: "烂酒店 WiFi (110ms/5%/Burst3)",
    cfg: { enabled: true, delayMs: 110, jitterMs: 35, lossPct: 5.0, burstLen: 3, rateKbps: 6000 },
  },
  {
    name: "高铁极限弱网 (180ms/9%/Burst5)",
    cfg: { enabled: true, delayMs: 180, jitterMs: 55, lossPct: 9.0, burstLen: 5, rateKbps: 3000 },
  },
];

/** 极简 SVG 示波器折线（零外部图表库开销）。 */
function OscilloscopeLine({
  data,
  color,
  secondary,
  secColor,
}: {
  data: number[];
  color: string;
  secondary?: number[];
  secColor?: string;
}) {
  const w = 320;
  const h = 54;
  if (data.length < 2) {
    return <svg className={styles.oscSvg} viewBox={`0 0 ${w} ${h}`} aria-hidden="true" />;
  }
  const all = secondary ? [...data, ...secondary] : data;
  const max = Math.max(...all, 1);
  const min = 0;
  const range = max - min || 1;
  const toPts = (arr: number[]) =>
    arr
      .map(
        (v, i) =>
          `${((i / Math.max(arr.length - 1, 1)) * w).toFixed(1)},${(
            h -
            4 -
            ((v - min) / range) * (h - 10)
          ).toFixed(1)}`,
      )
      .join(" ");

  return (
    <svg
      className={styles.oscSvg}
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      aria-hidden="true"
    >
      <polyline
        points={toPts(data)}
        fill="none"
        stroke={color}
        strokeWidth="1.8"
      />
      {secondary && secondary.length > 1 && (
        <polyline
          points={toPts(secondary)}
          fill="none"
          stroke={secColor ?? "#bc8cff"}
          strokeWidth="1.5"
          strokeDasharray="3 2"
        />
      )}
    </svg>
  );
}

/** 在 demo 预览或沙盒未出帧时绘制 1:1 极客清晰度标尺图案供微距放大镜检视。 */
function drawSyntheticRulerOnCanvas(
  canvas: HTMLCanvasElement,
  tick: number,
  tileEnabled: boolean,
  kbps: number,
): void {
  if (typeof navigator !== "undefined" && navigator.userAgent.includes("jsdom")) {
    return;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const w = 640;
  const h = 360;
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  ctx.fillStyle = "#10141a";
  ctx.fillRect(0, 0, w, h);

  // 1) 左上：1px 高频棋盘格 + RGB Subpixel 竖线
  for (let y = 10; y < 80; y += 2) {
    for (let x = 10; x < 160; x += 2) {
      ctx.fillStyle = "#f0f4f8";
      ctx.fillRect(x, y, 1, 1);
      ctx.fillRect(x + 1, y + 1, 1, 1);
    }
  }
  for (let x = 170; x < 300; x += 3) {
    ctx.fillStyle = "#ff3b30";
    ctx.fillRect(x, 10, 1, 70);
    ctx.fillStyle = "#4cd964";
    ctx.fillRect(x + 1, 10, 1, 70);
    ctx.fillStyle = "#007aff";
    ctx.fillRect(x + 2, 10, 1, 70);
  }

  // 2) 右上：9pt 极客代码字模区（检验 4:2:0 色度模糊 vs QOI 无损瓦片锐化）
  ctx.font = '12px "Cascadia Mono", Consolas, monospace';
  const sharp = tileEnabled || kbps >= 15000;
  ctx.fillStyle = sharp ? "#eef4ff" : "#b8c0cc";
  const lines = [
    "fn quic_bbr_pump(conn: &Conn, fec: &FecRs) {",
    "  // 1:1 Pixel Mapping + Lossless QOI Tile",
    "  let cwnd = conn.stats().path.cwnd;",
    "  assert!(text_clarity == 1.000 && rtt < 25);",
    "  tile_refiner.tick(raw, budget_bytes);",
    "}",
  ];
  lines.forEach((line, idx) => {
    ctx.fillText(line, 320, 28 + idx * 20);
  });

  if (tileEnabled) {
    ctx.strokeStyle = "#46d17e";
    ctx.lineWidth = 1;
    ctx.strokeRect(314, 10, 316, 130);
    ctx.fillStyle = "#46d17e";
    ctx.fillText("QOI LOSSLESS TILE REFINED (100% PIXEL EXACT)", 320, 155);
  } else {
    ctx.strokeStyle = "#e5b567";
    ctx.lineWidth = 1;
    ctx.strokeRect(314, 10, 316, 130);
    ctx.fillStyle = "#e5b567";
    ctx.fillText("H.264 4:2:0 BASE ONLY (CHROMA SUBSAMPLED)", 320, 155);
  }

  // 3) 底部：60fps 动态扫描游标
  const bx = (tick * 10) % (w - 40);
  ctx.fillStyle = "#1c222b";
  ctx.fillRect(10, 260, w - 20, 60);
  ctx.fillStyle = "#46d17e";
  ctx.fillRect(10 + bx, 264, 28, 52);
}

export function Lab({ deviceId, drawer = false, onClose }: LabProps) {
  const [live, setLive] = useState<LabLiveStateDto | null>(null);
  const [transport, setTransport] = useState<LabTransportConfig>(DEFAULT_TRANSPORT);
  const [codec, setCodec] = useState<LabCodecConfig>(DEFAULT_CODEC);
  const [netem, setNetem] = useState<NetemConfigDto>(DEFAULT_NETEM);
  const [profiles, setProfiles] = useState<LabProfile[]>([]);
  const [selectedProfileId, setSelectedProfileId] = useState<string>("");
  const [abIdA, setAbIdA] = useState<string>("geek-lan-444");
  const [abIdB, setAbIdB] = useState<string>("cross-wan-rs");
  const [abRunning, setAbRunning] = useState(false);
  const [abResult, setAbResult] = useState<LabAbResultDto | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [decodeStats, setDecodeStats] = useState<DecodeStats>({
    fps: 60,
    decodeMs: 1.1,
    width: 640,
    height: 360,
  });

  // 60Hz 示波器历史缓冲
  const [rttHist, setRttHist] = useState<number[]>([]);
  const [videoKbpsHist, setVideoKbpsHist] = useState<number[]>([]);
  const [tileKbpsHist, setTileKbpsHist] = useState<number[]>([]);
  const [fpsHist, setFpsHist] = useState<number[]>([]);
  const [fecRecHist, setFecRecHist] = useState<number[]>([]);

  // 1:1 微距像素检视镜引用
  const videoCanvasRef = useRef<HTMLCanvasElement>(null);
  const tileCanvasRef = useRef<HTMLCanvasElement>(null);
  const zoomCanvasRef = useRef<HTMLCanvasElement>(null);
  const magPosRef = useRef<{ x: number; y: number }>({ x: 360, y: 42 });
  const hasRealFramesRef = useRef(false);

  const showToast = useCallback((msg: string) => {
    setToast(msg);
    window.setTimeout(() => {
      setToast((cur) => (cur === msg ? null : cur));
    }, 3200);
  }, []);

  // 加载预设 Profile 列表与初始状态
  useEffect(() => {
    void labListProfiles()
      .then((list) => {
        setProfiles(list);
        if (list[0]) {
          setSelectedProfileId(list[0].id);
          setAbIdA(list[0].id);
        }
        if (list[1]) {
          setAbIdB(list[1].id);
        }
      })
      .catch(() => undefined);
  }, []);

  // 定时拉取底层链路解剖遥测并推进示波器波形
  useEffect(() => {
    let active = true;
    const poll = () => {
      void labGetState(deviceId)
        .then((st) => {
          if (!active) return;
          setLive(st);
          setRttHist((h) => [...h.slice(-59), st.rttMs]);
          setVideoKbpsHist((h) => [...h.slice(-59), st.videoKbps]);
          setTileKbpsHist((h) => [...h.slice(-59), st.tileKbps]);
          setFpsHist((h) => [...h.slice(-59), st.fpsCap]);
          setFecRecHist((h) => [...h.slice(-59), st.fecRecovered]);
        })
        .catch(() => undefined);
    };
    poll();
    const iv = window.setInterval(poll, 350);
    return () => {
      active = false;
      window.clearInterval(iv);
    };
  }, [deviceId]);

  // 更新 400% 最近邻微距放大镜画面
  const refreshMagnifier = useCallback(() => {
    if (typeof navigator !== "undefined" && navigator.userAgent.includes("jsdom")) {
      return;
    }
    const zoomEl = zoomCanvasRef.current;
    const vidEl = videoCanvasRef.current;
    const tileEl = tileCanvasRef.current;
    if (!zoomEl || !vidEl) return;
    const zctx = zoomEl.getContext("2d");
    if (!zctx) return;
    zoomEl.width = 192;
    zoomEl.height = 192;
    zctx.imageSmoothingEnabled = false;
    zctx.fillStyle = "#080a0d";
    zctx.fillRect(0, 0, 192, 192);

    // 若是在抽屉模式且底层 Session 页面有真实远控 videoCanvas，优先取真实远控画面！
    const sessionVideo = drawer
      ? document.querySelector<HTMLCanvasElement>("canvas")
      : null;
    const srcVideo =
      sessionVideo && sessionVideo !== vidEl && sessionVideo.width > 0
        ? sessionVideo
        : vidEl;

    const sx = Math.max(0, Math.min(srcVideo.width - 48, magPosRef.current.x - 24));
    const sy = Math.max(0, Math.min(srcVideo.height - 48, magPosRef.current.y - 24));
    try {
      // 48x48 源像素 → 192x192 (400% 像素级放大)
      zctx.drawImage(srcVideo, sx, sy, 48, 48, 0, 0, 192, 192);
      if (tileEl && srcVideo === vidEl && tileEl.width > 0) {
        zctx.drawImage(tileEl, sx, sy, 48, 48, 0, 0, 192, 192);
      }
    } catch {
      // ignore cross-frame resize race
    }
    // 中心十字准星
    zctx.strokeStyle = "rgba(70, 209, 126, 0.55)";
    zctx.lineWidth = 1;
    zctx.strokeRect(88, 88, 16, 16);
  }, [drawer]);

  // 启动真协议栈标尺沙盒流（WebCodecs 解码 + TileLayer 叠加）
  useEffect(() => {
    const videoEl = videoCanvasRef.current;
    const tileEl = tileCanvasRef.current;
    if (!videoEl || !tileEl) return undefined;

    const tileLayer = new TileLayer(new CanvasTileSurface(tileEl));
    let decoder: H264Decoder | null = null;
    try {
      decoder = new H264Decoder(videoEl, (st) => {
        hasRealFramesRef.current = true;
        tileLayer.setSize(st.width, st.height);
        setDecodeStats(st);
        refreshMagnifier();
      });
    } catch {
      decoder = null;
    }

    void labStartSandbox((buf) => {
      if (buf.byteLength < 1 + TILE_HEADER_LEN) return;
      const type = new Uint8Array(buf)[0] ?? 0;
      if (type === FRAME_VIDEO && buf.byteLength >= 1 + VIDEO_HEADER_LEN) {
        parseVideoHeader(buf.slice(1));
        decoder?.push(buf.slice(1));
      } else if (type === FRAME_TILE) {
        const hdr = parseTileHeader(buf, 1);
        const len = tilePayloadLen(hdr);
        if (buf.byteLength >= 1 + TILE_HEADER_LEN + len) {
          const rgba = new Uint8ClampedArray(buf, 1 + TILE_HEADER_LEN, len);
          tileLayer.handleTile(hdr, rgba);
          refreshMagnifier();
        }
      } else if (type === FRAME_INVALIDATE) {
        const hdr = parseTileHeader(buf, 1);
        tileLayer.handleInvalidate(hdr);
      }
    });

    // 兜底：若 WebCodecs 在测试/无 GPU 环境下未出帧，用 Canvas 实时绘制标尺帧保证微距检视镜随时可交互
    let tick = 0;
    const anim = window.setInterval(() => {
      tick += 1;
      if (!hasRealFramesRef.current || !isTauriRuntime()) {
        drawSyntheticRulerOnCanvas(
          videoEl,
          tick,
          codec.tileEnabled,
          codec.bitrateKbps,
        );
      }
      refreshMagnifier();
    }, 80);

    return () => {
      window.clearInterval(anim);
      decoder?.close();
      void labStopSandbox();
    };
  }, [codec.tileEnabled, codec.bitrateKbps, refreshMagnifier]);

  // 操控域 ① & ②：选路与传输层应用
  const [savedDevices, setSavedDevices] = useState<DeviceDto[]>([]);
  const [targetDeviceId, setTargetDeviceId] = useState<string>(deviceId ?? "");
  const [customIp, setCustomIp] = useState<string>("");
  const [defaultRelayAddr, setDefaultRelayAddr] = useState<string>("");
  const [advancedOpen, setAdvancedOpen] = useState<boolean>(false);

  // 按目标持久化的多绑定方案（1 个目标可绑定 N 个不同方案：直连 / Tailscale / 自建中转 / EasyTier）
  const [plansByTarget, setPlansByTarget] = useState<Record<string, RealTestPlanDto[]>>(() => {
    try {
      const raw = window.localStorage.getItem("rd.labTargetPlans");
      if (raw) return JSON.parse(raw) as Record<string, RealTestPlanDto[]>;
    } catch {
      // ignore
    }
    return {};
  });
  const [newPlanName, setNewPlanName] = useState("");
  const [newPlanMode, setNewPlanMode] = useState<RealTestPlanDto["mode"]>("direct");
  const [newPlanAddr, setNewPlanAddr] = useState("");
  const [newPlanRelay, setNewPlanRelay] = useState("");
  const [editingPlanId, setEditingPlanId] = useState<string | null>(null);
  const [showAddPlan, setShowAddPlan] = useState(false);

  // 每个目标「已确定生效」的默认方案 ID（持久化）
  const [confirmedPlanByTarget, setConfirmedPlanByTarget] = useState<Record<string, string>>(() => {
    try {
      const raw = window.localStorage.getItem("rd.labConfirmedPlans");
      if (raw) return JSON.parse(raw) as Record<string, string>;
    } catch {
      // ignore
    }
    return {};
  });
  const [applyingPlanId, setApplyingPlanId] = useState<string | null>(null);
  const [connectingTarget, setConnectingTarget] = useState(false);

  // 真实自动化测速结果（零造假：仅存后端真实探测回包）
  const [realResults, setRealResults] = useState<Record<string, RealPlanProbeResultDto>>({});
  const [testingPlanId, setTestingPlanId] = useState<string | null>(null);
  const [runningRealBatch, setRunningRealBatch] = useState(false);

  // 共用打洞 (STUN) / EasyTier / 自建中转服务器节点池（支持实时拉取公共源、并发测速排序、分类管理、SSH 一键自建）
  interface RelayNodeItem {
    id: string;
    name: string;
    addr: string;
    kind?: "stun" | "easytier" | "relay";
    region?: string;
    source?: string;
    favorite?: boolean;
    lastRttMs: number | null;
    mappedAddr?: string | null;
    status: "idle" | "ok" | "fail";
  }
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
  const [logsExpanded, setLogsExpanded] = useState(true);
  const logBoxRef = useRef<HTMLDivElement | null>(null);
  const [poolFilter, setPoolFilter] = useState<"all" | "stun" | "easytier" | "relay">("all");
  const [fetchingPublic, setFetchingPublic] = useState(false);
  const [showSubInput, setShowSubInput] = useState(false);
  const [customSubUrl, setCustomSubUrl] = useState(() => {
    try {
      return window.localStorage.getItem("rd.labCustomSubUrl") ?? "";
    } catch {
      return "";
    }
  });
  const [showBatchAdd, setShowBatchAdd] = useState(false);
  const [batchNodeKind, setBatchNodeKind] = useState<"auto" | "stun" | "easytier" | "relay">("auto");
  const [batchNodeText, setBatchNodeText] = useState("");
  const [probingPool, setProbingPool] = useState(false);
  const [probingNodeId, setProbingNodeId] = useState<string | null>(null);
  const [activeModeTab, setActiveModeTab] = useState<"direct" | "easytier" | "relay">("easytier");
  const [modeBodyOpen, setModeBodyOpen] = useState(false);
  const [selectedEtNodeIds, setSelectedEtNodeIds] = useState<string[]>([]);
  const [natBusy, setNatBusy] = useState(false);
  const [natResult, setNatResult] = useState<NatDiagnosisDto | null>(null);
  const [stunDrawerOpen, setStunDrawerOpen] = useState(false);
  const [relayIdInfo, setRelayIdInfo] = useState<RelayIdStatusDto | null>(null);
  const [peerNumericIdInput, setPeerNumericIdInput] = useState("");
  const [selectedRelayNodeAddr, setSelectedRelayNodeAddr] = useState("");
  const [relayLookupBusy, setRelayLookupBusy] = useState(false);
  const [relayLookupRes, setRelayLookupRes] = useState<RelayResolveIdResultDto | null>(null);
  const [etStatus, setEtStatus] = useState<EasyTierStatusDto | null>(null);
  const [etBusy, setEtBusy] = useState(false);
  const [etInstalling, setEtInstalling] = useState(false);
  const [etJoinInput, setEtJoinInput] = useState("");
  const [etInviteCode, setEtInviteCode] = useState("");
  const [smartDiscovering, setSmartDiscovering] = useState(false);
  const [smartHint, setSmartHint] = useState<{
    level: "ok" | "suggest" | "warn";
    title: string;
    detail: string;
    candidateNode?: RelayNodeItem;
    boundPlan?: RealTestPlanDto;
  } | null>(null);
  const [showSshDeploy, setShowSshDeploy] = useState(false);
  const [sshName, setSshName] = useState("");
  const [sshHost, setSshHost] = useState("");
  const [sshPort, setSshPort] = useState("22");
  const [sshUser, setSshUser] = useState("root");
  const [sshKeyPath, setSshKeyPath] = useState("");
  const [sshMode, setSshMode] = useState<"udp" | "docker">("udp");
  const [sshBusy, setSshBusy] = useState(false);

  const [eventLogs, setEventLogs] = useState<
    { id: number; time: string; level: "info" | "ok" | "warn"; text: string }[]
  >(() => [
    {
      id: 1,
      time: new Date().toLocaleTimeString("zh-CN", { hour12: false }),
      level: "info",
      text: "真实测速实验室就绪：1) 选择或输入目标 → 2) 为该目标绑定 1~N 个方案 → 3) 点「一键自动实测全部方案」查看真实网络延迟与动态日志。",
    },
  ]);
  const logSeqRef = useRef(2);
  const pushLog = useCallback((level: "info" | "ok" | "warn", text: string) => {
    const id = logSeqRef.current++;
    const time = new Date().toLocaleTimeString("zh-CN", { hour12: false });
    setEventLogs((prev) => [...prev, { id, time, level, text }].slice(-120));
    requestAnimationFrame(() => {
      if (logBoxRef.current) {
        logBoxRef.current.scrollTop = logBoxRef.current.scrollHeight;
      }
    });
  }, []);

  useEffect(() => {
    void listDevices()
      .then((list) => {
        if (Array.isArray(list)) {
          setSavedDevices(list);
          if (!targetDeviceId && list.length > 0 && list[0]) {
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
            const next: RelayNodeItem[] = [
              ...prev,
              { id: "default-relay", name: "默认中转/打洞节点", addr, lastRttMs: null, status: "idle" },
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
        setEtStatus(st);
        if (st.inviteCode) setEtInviteCode(st.inviteCode);
      })
      .catch(() => undefined);
    void relayIdStatus()
      .then((info) => {
        setRelayIdInfo(info);
        if (info.relayAddr) setSelectedRelayNodeAddr(info.relayAddr);
      })
      .catch(() => undefined);
  }, [targetDeviceId]);

  const selectedDevice =
    savedDevices.find((d) => d.id === targetDeviceId) ?? null;
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
  const effectiveTargetLabel =
    customIp.trim().length > 0
      ? customIp.trim()
      : selectedDevice
        ? `${selectedDevice.name} (${selectedDevice.addr.replace(/:21118$/, "")})`
        : "未指定目标（请先选择设备或输入 IP）";

  const activePlans: RealTestPlanDto[] = (() => {
    const existing = plansByTarget[targetKey];
    if (existing && existing.length > 0) {
      return existing;
    }
    const baseIp = baseTargetIp || "192.168.1.10";
    const starter: RealTestPlanDto[] = [
      {
        planId: `${targetKey}-direct`,
        planName: baseIp.startsWith("100.") ? "Tailscale 直连通道" : "直连通道 (QUIC)",
        mode: baseIp.startsWith("100.") ? "tailscale" : "direct",
        targetAddr: baseIp,
        relayAddr: "",
        rounds: 3,
      },
    ];
    if (defaultRelayAddr || selectedDevice?.relay) {
      starter.push({
        planId: `${targetKey}-relay`,
        planName: "自建中转服务器",
        mode: "relay",
        targetAddr: baseIp,
        relayAddr: selectedDevice?.relay ?? defaultRelayAddr,
        rounds: 3,
      });
    }
    return starter;
  })();

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

  const handleAddTargetPlan = useCallback(() => {
    const name =
      newPlanName.trim() ||
      (newPlanMode === "direct"
        ? "局域网直连"
        : newPlanMode === "tailscale"
          ? "Tailscale 跨网"
          : newPlanMode === "punch"
            ? "STUN 打洞"
            : newPlanMode === "relay"
              ? "自建中转"
              : "EasyTier 组网");
    const addr = newPlanAddr.trim() || baseTargetIp;
    const relay = newPlanRelay.trim() || defaultRelayAddr;
    if (newPlanMode !== "relay" && newPlanMode !== "punch" && !addr) {
      pushLog("warn", "新增方案失败：请填写目标 IP 地址");
      return;
    }
    if ((newPlanMode === "relay" || newPlanMode === "punch") && !relay) {
      pushLog("warn", "新增打洞/中转方案失败：请先选择共用服务器、填写服务器地址，或点「SSH 一键部署新节点」");
      return;
    }
    if ((newPlanMode === "relay" || newPlanMode === "punch") && relay) {
      // 自动收录到共用节点池，方便后续其他目标复用
      if (!serverPool.some((n) => n.addr === relay)) {
        saveServerPool([
          ...serverPool,
          { id: `node-${Date.now().toString(36)}`, name, addr: relay, lastRttMs: null, status: "idle" },
        ]);
      }
    }
    const plan: RealTestPlanDto = {
      planId: editingPlanId ?? `plan-${Date.now().toString(36)}`,
      planName: name,
      mode: newPlanMode,
      targetAddr: addr,
      relayAddr: relay,
      rounds: 3,
    };
    if (editingPlanId) {
      savePlansForTarget(
        activePlans.map((p) => (p.planId === editingPlanId ? plan : p)),
      );
      pushLog("ok", `已修改方案「${plan.planName}」（${plan.mode.toUpperCase()}）`);
    } else {
      savePlansForTarget([...activePlans, plan]);
      pushLog("ok", `已为目标「${effectiveTargetLabel}」新增方案：${plan.planName}（${plan.mode.toUpperCase()}）`);
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
    effectiveTargetLabel,
    newPlanAddr,
    newPlanMode,
    newPlanName,
    newPlanRelay,
    pushLog,
    savePlansForTarget,
    saveServerPool,
    serverPool,
  ]);

  const handleStartEditPlan = useCallback((plan: RealTestPlanDto) => {
    setEditingPlanId(plan.planId);
    setNewPlanName(plan.planName);
    setNewPlanMode(plan.mode);
    setNewPlanAddr(plan.targetAddr);
    setNewPlanRelay(plan.relayAddr);
    setShowAddPlan(true);
  }, []);

  const confirmedPlanId = confirmedPlanByTarget[targetKey] ?? null;

  // 确定方案：设为当前目标的生效默认方案，并同步写入设备地址/中转与底层选路策略
  const handleConfirmPlan = useCallback(
    async (plan: RealTestPlanDto) => {
      setApplyingPlanId(plan.planId);
      const nextConfirmed = { ...confirmedPlanByTarget, [targetKey]: plan.planId };
      setConfirmedPlanByTarget(nextConfirmed);
      try {
        window.localStorage.setItem("rd.labConfirmedPlans", JSON.stringify(nextConfirmed));
      } catch {
        // ignore
      }
      try {
        const routeMode =
          plan.mode === "relay"
            ? "relay"
            : plan.mode === "punch"
              ? "punch"
              : plan.mode === "easytier"
                ? "easytier"
                : "direct";
        if (selectedDevice) {
          const nextAddr = plan.targetAddr.trim() || selectedDevice.addr;
          const nextRelay =
            plan.mode === "relay" || plan.mode === "punch"
              ? plan.relayAddr.trim() || ""
              : selectedDevice.relay || "";
          await editDevice(selectedDevice.id, selectedDevice.name, nextAddr, nextRelay);
        }
        await labApplyTransport(selectedDevice?.id ?? null, {
          ...transport,
          routeMode,
        });
        showToast(`✅ 已确定方案「${plan.planName}」并同步生效`);
        pushLog(
          "ok",
          `✅ 已确定「${plan.planName}」为「${effectiveTargetLabel}」的默认连接方案（模式=${plan.mode.toUpperCase()}）`,
        );
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        showToast(`已标记生效，同步提示：${msg}`);
        pushLog("warn", `确定方案「${plan.planName}」同步底层时提示：${msg}`);
      } finally {
        setApplyingPlanId(null);
      }
    },
    [confirmedPlanByTarget, effectiveTargetLabel, pushLog, selectedDevice, showToast, targetKey, transport],
  );

  // 用当前确定的方案一键直连目标桌面
  const handleConnectWithConfirmed = useCallback(async () => {
    if (!selectedDevice) {
      showToast("请先在左侧选择一台已配对设备，或去设备页添加该 IP 后连接");
      return;
    }
    setConnectingTarget(true);
    try {
      if (!selectedDevice.online) {
        await connectDevice(selectedDevice.id);
      }
      window.location.hash = `#/session/${selectedDevice.id}?open=18446462598732840960`;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(`连接失败：${msg}`);
      pushLog("warn", `连接 ${selectedDevice.name} 失败：${msg}`);
    } finally {
      setConnectingTarget(false);
    }
  }, [pushLog, selectedDevice, showToast]);

  const handleDeleteTargetPlan = useCallback(
    (planId: string) => {
      savePlansForTarget(activePlans.filter((p) => p.planId !== planId));
      setRealResults((prev) => {
        const copy = { ...prev };
        delete copy[planId];
        return copy;
      });
    },
    [activePlans, savePlansForTarget],
  );

  const runSinglePlanReal = useCallback(
    async (plan: RealTestPlanDto): Promise<RealPlanProbeResultDto | null> => {
      setTestingPlanId(plan.planId);
      pushLog(
        "info",
        `▶ 正在真实探测方案「${plan.planName}」（${plan.mode.toUpperCase()} · ${
          plan.mode === "relay" ? plan.relayAddr : plan.targetAddr
        }）…`,
      );
      try {
        const res = await labProbeTargetPlan(plan);
        setRealResults((prev) => ({ ...prev, [plan.planId]: res }));
        for (const line of res.logs) {
          const lvl = line.includes("✗") ? "warn" : line.includes("✓") || line.startsWith("结论") && res.ok ? "ok" : "info";
          pushLog(lvl, `[${plan.planName}] ${line}`);
        }
        if (plan.mode === "tailscale" && plan.targetAddr.startsWith("100.")) {
          try {
            const ts = await tailscalePing(plan.targetAddr);
            pushLog("ok", `[${plan.planName}] Tailscale 路径诊断：${ts}`);
          } catch {
            // optional
          }
        }
        return res;
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        pushLog("warn", `[${plan.planName}] 实测调用异常：${msg}`);
        return null;
      } finally {
        setTestingPlanId(null);
      }
    },
    [pushLog],
  );

  const runAllTargetPlansReal = useCallback(async () => {
    if (activePlans.length === 0) {
      pushLog("warn", "当前目标还没有绑定任何测试方案，请先点「+ 新增绑定方案」");
      return;
    }
    setRunningRealBatch(true);
    pushLog(
      "info",
      `════════ 开始自动化实测目标「${effectiveTargetLabel}」（共 ${activePlans.length} 个绑定方案）════════`,
    );
    const collected: RealPlanProbeResultDto[] = [];
    for (const plan of activePlans) {
      const res = await runSinglePlanReal(plan);
      if (res) collected.push(res);
    }
    const reachable = collected
      .filter((r) => r.ok && r.avgRttMs !== null)
      .sort(
        (a, b) =>
          a.lossPct - b.lossPct ||
          (a.avgRttMs ?? 9999) - (b.avgRttMs ?? 9999),
      );
    if (reachable.length > 0 && reachable[0]) {
      const best = reachable[0];
      pushLog(
        "ok",
        `🏆 自动实测完成！最优方案：「${best.planName}」（均延迟 ${best.avgRttMs}ms，抖动 ${best.jitterMs ?? 0}ms，丢包 ${best.lossPct}%）`,
      );
    } else {
      pushLog(
        "warn",
        "自动实测完成：所有已绑定方案均不可达，请检查对方设备是否开机、软件是否运行或尝试添加中转方案。",
      );
    }
    setRunningRealBatch(false);
  }, [activePlans, effectiveTargetLabel, pushLog, runSinglePlanReal]);

  const bestRealPlanId = (() => {
    const okList = activePlans
      .map((p) => realResults[p.planId])
      .filter((r): r is RealPlanProbeResultDto => Boolean(r && r.ok && r.avgRttMs !== null))
      .sort(
        (a, b) =>
          a.lossPct - b.lossPct ||
          (a.avgRttMs ?? 9999) - (b.avgRttMs ?? 9999),
      );
    return okList[0]?.planId ?? null;
  })();

  const inferNodeKind = (node: RelayNodeItem): "stun" | "easytier" | "relay" => {
    if (node.kind) return node.kind;
    const lower = node.addr.toLowerCase();
    if (lower.includes("stun") || lower.endsWith(":3478") || lower.endsWith(":19302") || lower.endsWith(":18000")) {
      return "stun";
    }
    if (lower.startsWith("tcp://") || lower.startsWith("udp://") || lower.includes("easytier")) {
      return "easytier";
    }
    return "relay";
  };

  const sortServerPool = (list: RelayNodeItem[]): RelayNodeItem[] =>
    [...list].sort((a, b) => {
      const favDiff = (b.favorite ? 1 : 0) - (a.favorite ? 1 : 0);
      if (favDiff !== 0) return favDiff;
      const rank = (s: RelayNodeItem["status"]) => (s === "ok" ? 0 : s === "idle" ? 1 : 2);
      const rDiff = rank(a.status) - rank(b.status);
      if (rDiff !== 0) return rDiff;
      return (a.lastRttMs ?? 99999) - (b.lastRttMs ?? 99999);
    });

  // 实时在线拉取公共 STUN（国内大厂 + always-online-stun）与 EasyTier 社区节点
  const handleFetchPublicNodes = useCallback(async () => {
    setFetchingPublic(true);
    try {
      window.localStorage.setItem("rd.labCustomSubUrl", customSubUrl.trim());
    } catch {
      // ignore
    }
    pushLog(
      "info",
      customSubUrl.trim()
        ? `🌐 正在实时拉取公共 STUN 与 EasyTier 节点（含自定义订阅：${customSubUrl.trim()}）…`
        : "🌐 正在实时拉取公共 STUN（国内大厂 + GitHub 保活源）与 EasyTier 社区节点…",
    );
    try {
      const res = await labFetchPublicNodes(customSubUrl);
      for (const line of res.logs) {
        pushLog(line.includes("✓") ? "ok" : line.includes("⚠") ? "warn" : "info", line);
      }
      const existingAddrs = new Set(serverPool.map((n) => n.addr.toLowerCase()));
      const additions: RelayNodeItem[] = [];
      for (const item of res.nodes) {
        if (existingAddrs.has(item.addr.toLowerCase())) continue;
        existingAddrs.add(item.addr.toLowerCase());
        additions.push({
          id: `pub-${item.kind}-${Date.now()}-${additions.length}`,
          name: item.name,
          addr: item.addr,
          kind: item.kind,
          region: item.region,
          source: item.source,
          lastRttMs: null,
          mappedAddr: null,
          status: "idle",
        });
      }
      if (additions.length > 0) {
        saveServerPool(sortServerPool([...serverPool, ...additions]));
        showToast(`已入池 ${additions.length} 个公共节点，点「一键全池测速」测真实延迟`);
      } else {
        showToast("所有公共节点均已在节点池中");
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `拉取公共节点失败：${msg}`);
      showToast(`拉取失败：${msg}`);
    } finally {
      setFetchingPublic(false);
    }
  }, [customSubUrl, pushLog, saveServerPool, serverPool, showToast]);

  // 并发实测节点池中每个节点的真实延迟（STUN 走 RFC 5389 UDP + 公网映射 IP，EasyTier/Relay 走真实 TCP）并自动按延迟排序
  const handleProbeServerPool = useCallback(async () => {
    const targets =
      poolFilter === "all"
        ? serverPool
        : serverPool.filter((n) => inferNodeKind(n) === poolFilter);
    if (targets.length === 0) {
      pushLog("warn", "当前分组节点为空，请先点「🌐 实时获取公共节点」或「🚀 SSH 一键部署新节点」");
      return;
    }
    setProbingPool(true);
    pushLog("info", `🔍 开始并发实测 ${targets.length} 个节点的真实延迟（STUN 发 RFC 5389 UDP 包，EasyTier/中转测真实 TCP）…`);
    const map = new Map(serverPool.map((n) => [n.id, { ...n }]));
    const batchSize = 5;
    let okCount = 0;
    for (let i = 0; i < targets.length; i += batchSize) {
      const chunk = targets.slice(i, i + batchSize);
      await Promise.all(
        chunk.map(async (node) => {
          const kind = inferNodeKind(node);
          try {
            const res = await labProbeNode(kind, node.addr);
            if (res.ok) okCount += 1;
            map.set(node.id, {
              ...node,
              kind,
              lastRttMs: res.rttMs,
              mappedAddr: res.mappedAddr,
              status: res.ok ? "ok" : "fail",
            });
            pushLog(
              res.ok ? "ok" : "warn",
              `[${kind.toUpperCase()} · ${node.name}] ${res.ok ? `✓ ${res.detail}` : `✗ ${res.detail}`}`,
            );
          } catch (e) {
            map.set(node.id, { ...node, kind, lastRttMs: null, status: "fail" });
            pushLog("warn", `[${node.name}] 探测异常：${e instanceof Error ? e.message : String(e)}`);
          }
        }),
      );
      saveServerPool(sortServerPool(Array.from(map.values())));
    }
    pushLog("ok", `✓ 节点池测速完成：${okCount}/${targets.length} 个可用，已按延迟从低到高自动排序`);
    setProbingPool(false);
  }, [poolFilter, pushLog, saveServerPool, serverPool]);

  // 点击单个节点右上角延迟：立刻单独测速并刷新最新延迟与公网映射
  const handleProbeSingleNode = useCallback(
    async (node: RelayNodeItem) => {
      if (probingNodeId || probingPool) return;
      const kind = inferNodeKind(node);
      setProbingNodeId(node.id);
      try {
        const res = await labProbeNode(kind, node.addr);
        const updated = serverPool.map((n) =>
          n.id === node.id
            ? {
                ...n,
                kind,
                lastRttMs: res.rttMs,
                mappedAddr: res.mappedAddr,
                status: res.ok ? ("ok" as const) : ("fail" as const),
              }
            : n,
        );
        saveServerPool(sortServerPool(updated));
        pushLog(
          res.ok ? "ok" : "warn",
          `[${kind.toUpperCase()} · ${node.name}] ${res.ok ? `✓ ${res.detail}` : `✗ ${res.detail}`}`,
        );
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        saveServerPool(
          serverPool.map((n) =>
            n.id === node.id ? { ...n, kind, lastRttMs: null, status: "fail" as const } : n,
          ),
        );
        pushLog("warn", `[${node.name}] 单点测速异常：${msg}`);
      } finally {
        setProbingNodeId(null);
      }
    },
    [probingNodeId, probingPool, pushLog, saveServerPool, serverPool],
  );

  const handleBatchAddNodes = useCallback(() => {
    const lines = batchNodeText
      .split(/\r?\n/)
      .map((l) => l.trim())
      .filter((l) => l.length > 0 && !l.startsWith("#") && !l.startsWith("//"));
    if (lines.length === 0) {
      showToast("请先粘贴节点列表（一行一个）");
      return;
    }
    const existing = new Set(serverPool.map((n) => n.addr.toLowerCase()));
    const added: RelayNodeItem[] = [];
    let skipped = 0;

    for (const line of lines) {
      let name = "";
      let addr = line;
      // 支持「名称,地址」或「名称 地址」或纯「地址」
      if (line.includes(",")) {
        const parts = line.split(",").map((s) => s.trim()).filter(Boolean);
        if (parts.length >= 2) {
          name = parts[0] ?? "";
          addr = parts[1] ?? "";
        }
      } else if (line.includes(" ") || line.includes("\t")) {
        const parts = line.split(/\s+/).filter(Boolean);
        if (parts.length >= 2) {
          // 含 : 或 :// 的那段作为地址
          const addrIdx = parts.findIndex((p) => p.includes(":") || p.includes("://"));
          if (addrIdx >= 0) {
            addr = parts[addrIdx] ?? line;
            name = parts.filter((_, idx) => idx !== addrIdx).join(" ");
          }
        }
      }
      if (!addr || existing.has(addr.toLowerCase())) {
        skipped += 1;
        continue;
      }
      existing.add(addr.toLowerCase());
      const lower = addr.toLowerCase();
      const autoKind: "stun" | "easytier" | "relay" =
        batchNodeKind !== "auto"
          ? batchNodeKind
          : lower.includes("stun") || lower.endsWith(":3478") || lower.endsWith(":19302") || lower.endsWith(":18000")
            ? "stun"
            : lower.startsWith("tcp://") || lower.startsWith("udp://") || lower.includes("easytier")
              ? "easytier"
              : "relay";
      const hostHint = addr.replace(/^[a-z]+:\/\//i, "").split(":")[0] ?? addr;
      added.push({
        id: `batch-${Date.now()}-${added.length}`,
        name: name || `${autoKind.toUpperCase()} · ${hostHint}`,
        addr,
        kind: autoKind,
        source: "批量导入",
        lastRttMs: null,
        mappedAddr: null,
        status: "idle",
      });
    }

    if (added.length > 0) {
      saveServerPool(sortServerPool([...added, ...serverPool]));
      setBatchNodeText("");
      setShowBatchAdd(false);
      showToast(`已批量添加 ${added.length} 个节点${skipped > 0 ? `（跳过 ${skipped} 个重复）` : ""}`);
      pushLog("ok", `✓ 批量导入 ${added.length} 个节点到共用池${skipped > 0 ? `（跳过 ${skipped} 个重复）` : ""}`);
    } else {
      showToast(`没有新节点可添加（${skipped} 个已存在或格式无效）`);
    }
  }, [batchNodeKind, batchNodeText, pushLog, saveServerPool, serverPool, showToast]);

  const handleBindNodeToTarget = useCallback(
    (node: RelayNodeItem) => {
      const kind = inferNodeKind(node);
      const mode: RealTestPlanDto["mode"] =
        kind === "stun" ? "punch" : kind === "easytier" ? "easytier" : "relay";
      const plan: RealTestPlanDto = {
        planId: `plan-${Date.now()}`,
        planName: `${node.name}${node.lastRttMs !== null ? ` (${node.lastRttMs}ms)` : ""}`,
        mode,
        targetAddr: baseTargetIp || "192.168.1.10",
        relayAddr: node.addr,
        rounds: 3,
      };
      savePlansForTarget([...activePlans, plan]);
      showToast(`已将「${node.name}」绑定到当前目标方案`);
      pushLog("ok", `已一键绑定节点「${node.name}」(${node.addr}) 到目标「${effectiveTargetLabel}」`);
      return plan;
    },
    [activePlans, baseTargetIp, effectiveTargetLabel, pushLog, savePlansForTarget, showToast],
  );

  // 智能寻优：同时测试「当前已绑定方案」+「未建立的候选公共/自建节点」，自动诊断并一键推荐最优解
  const runSmartDiscovery = useCallback(async () => {
    setSmartDiscovering(true);
    setSmartHint(null);
    pushLog("info", `✨ 开始为「${effectiveTargetLabel}」执行智能全网寻优（测已绑定方案 + 未建立的候选公共节点）…`);

    // 1) 实测现有已绑定方案
    const boundResults: { plan: RealTestPlanDto; res: RealPlanProbeResultDto }[] = [];
    for (const plan of activePlans) {
      const res = await runSinglePlanReal(plan);
      if (res) boundResults.push({ plan, res });
    }
    const bestBound = boundResults
      .filter((x) => x.res.ok && x.res.avgRttMs !== null)
      .sort((a, b) => a.res.lossPct - b.res.lossPct || (a.res.avgRttMs ?? 9999) - (b.res.avgRttMs ?? 9999))[0];

    // 2) 准备候选节点池（若池为空，自动拉取公共 STUN / EasyTier 节点）
    let poolNow = [...serverPool];
    if (poolNow.length === 0) {
      pushLog("info", "节点池为空，正在自动拉取国内高速公共 STUN 与 EasyTier 候选节点…");
      try {
        const fetched = await labFetchPublicNodes(customSubUrl);
        poolNow = fetched.nodes.map((item, idx) => ({
          id: `auto-${item.kind}-${Date.now()}-${idx}`,
          name: item.name,
          addr: item.addr,
          kind: item.kind,
          region: item.region,
          source: item.source,
          lastRttMs: null,
          mappedAddr: null,
          status: "idle" as const,
        }));
        saveServerPool(poolNow);
        pushLog("ok", `✓ 自动载入 ${poolNow.length} 个公共候选节点`);
      } catch (e) {
        pushLog("warn", `自动拉取公共节点失败：${e instanceof Error ? e.message : String(e)}`);
      }
    }

    // 3) 过滤出尚未绑定为方案的候选节点，并发测速
    const boundAddrs = new Set(
      activePlans.map((p) => (p.relayAddr || p.targetAddr).trim().toLowerCase()),
    );
    const candidates = poolNow.filter((n) => !boundAddrs.has(n.addr.trim().toLowerCase()));
    const poolMap = new Map(poolNow.map((n) => [n.id, { ...n }]));

    if (candidates.length > 0) {
      pushLog("info", `🔍 正在并发探测 ${candidates.length} 个未建立的候选方案节点…`);
      for (let i = 0; i < candidates.length; i += 5) {
        const chunk = candidates.slice(i, i + 5);
        await Promise.all(
          chunk.map(async (node) => {
            const kind = inferNodeKind(node);
            try {
              const res = await labProbeNode(kind, node.addr);
              poolMap.set(node.id, {
                ...node,
                kind,
                lastRttMs: res.rttMs,
                mappedAddr: res.mappedAddr,
                status: res.ok ? "ok" : "fail",
              });
              if (res.ok) {
                pushLog("ok", `[候选 ${kind.toUpperCase()} · ${node.name}] ✓ ${res.detail}`);
              }
            } catch {
              poolMap.set(node.id, { ...node, kind, lastRttMs: null, status: "fail" });
            }
          }),
        );
      }
      const sortedPool = sortServerPool(Array.from(poolMap.values()));
      saveServerPool(sortedPool);
      poolNow = sortedPool;
    }

    const bestCandidate = poolNow
      .filter((n) => !boundAddrs.has(n.addr.trim().toLowerCase()) && n.status === "ok" && n.lastRttMs !== null)
      .sort((a, b) => (a.lastRttMs ?? 9999) - (b.lastRttMs ?? 9999))[0];

    // 4) 智能结论与诊断
    const bestBoundRtt = bestBound?.res.avgRttMs ?? null;
    const tailscaleSlow =
      bestBound?.plan.mode === "tailscale" && bestBoundRtt !== null && bestBoundRtt > 180;

    if (bestCandidate && (bestBoundRtt === null || (bestCandidate.lastRttMs ?? 9999) + 15 < bestBoundRtt)) {
      const reason = tailscaleSlow
        ? `检测到现有「${bestBound.plan.planName}」延迟高达 ${bestBoundRtt}ms（疑似绕道海外 DERP 中继）；`
        : bestBoundRtt !== null
          ? `现有最优「${bestBound?.plan.planName}」延迟 ${bestBoundRtt}ms；`
          : "当前已绑定方案均不可达；";
      setSmartHint({
        level: "suggest",
        title: `✨ 发现更优未建方案：${bestCandidate.name}（${bestCandidate.lastRttMs}ms）`,
        detail: `${reason}实测候选节点「${bestCandidate.name}」仅 ${bestCandidate.lastRttMs}ms${bestCandidate.mappedAddr ? `（公网映射 ${bestCandidate.mappedAddr}）` : ""}，建议一键采纳！`,
        candidateNode: bestCandidate,
      });
      pushLog("ok", `✨ 智能推荐：未建候选「${bestCandidate.name}」(${bestCandidate.lastRttMs}ms) 显著优于现有方案，可直接点上方「一键采纳并启用」`);
    } else if (bestBound) {
      setSmartHint({
        level: "ok",
        title: `✅ 现有方案「${bestBound.plan.planName}」已是当前最优（${bestBoundRtt}ms）`,
        detail: `已对比所有候选公共节点，现有「${bestBound.plan.planName}」（均延迟 ${bestBoundRtt}ms / 丢包 ${bestBound.res.lossPct}%）表现最佳。`,
        boundPlan: bestBound.plan,
      });
      pushLog("ok", `✅ 智能寻优结论：现有方案「${bestBound.plan.planName}」(${bestBoundRtt}ms) 已是最优解`);
    } else {
      setSmartHint({
        level: "warn",
        title: "⚠ 暂未找到低延迟直连或公共候选方案",
        detail: "建议展开下方节点池，使用「🚀 SSH 一键部署」搭建独享国内 VPS 中转，或批量粘贴新的公共节点。",
      });
      pushLog("warn", "智能寻优未发现可用低延迟路径，建议部署独享中转或导入更多节点");
    }

    setSmartDiscovering(false);
  }, [activePlans, customSubUrl, effectiveTargetLabel, pushLog, runSinglePlanReal, saveServerPool, serverPool]);

  const handleAdoptCandidate = useCallback(
    async (node: RelayNodeItem) => {
      const plan = handleBindNodeToTarget(node);
      await handleConfirmPlan(plan);
      setSmartHint(null);
    },
    [handleBindNodeToTarget, handleConfirmPlan],
  );

  // 方案 B：EasyTier 一键组网口令（生成 / 加入 / 刷新在线 10.126.x.x 成员）
  const handleRefreshEtStatus = useCallback(async () => {
    setEtBusy(true);
    try {
      const st = await easytierDetectStatus();
      setEtStatus(st);
      if (st.inviteCode) setEtInviteCode(st.inviteCode);
      pushLog(
        "ok",
        `[EasyTier] 检测完成：${st.enabled ? `网络 ${st.networkName} · 本机 ${st.virtualIp || "待重启分配"} · 在线对端 ${st.peers.length} 台` : "未启用"}`,
      );
    } catch (e) {
      pushLog("warn", `[EasyTier] 检测失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setEtBusy(false);
    }
  }, [pushLog]);

  const handleGenerateEtInvite = useCallback(async () => {
    const etNodes = serverPool.filter((n) => inferNodeKind(n) === "easytier");
    const picked = etNodes.filter((n) => selectedEtNodeIds.includes(n.id));
    const finalPeers =
      picked.length > 0
        ? picked.map((n) => n.addr)
        : etNodes
            .filter((n) => n.status === "ok")
            .sort((a, b) => (a.lastRttMs ?? 9999) - (b.lastRttMs ?? 9999))
            .slice(0, 3)
            .map((n) => n.addr);
    if (finalPeers.length === 0) {
      showToast("请先在下方拉取或粘贴 EasyTier 节点并测速，再勾选至少 1 个可用节点");
      pushLog("warn", "[EasyTier] 尚未勾选可用 EasyTier 引导节点，请先拉取/粘贴节点并测速勾选");
      return;
    }
    setEtBusy(true);
    try {
      const res = await easytierGenerateInvite(finalPeers);
      setEtInviteCode(res.inviteCode);
      showToast(`已用所选 ${finalPeers.length} 个节点生成口令！复制发给对方粘贴即可`);
      pushLog("ok", `[EasyTier] 已使用 ${finalPeers.join(", ")} 生成组网口令`);
      const st = await easytierDetectStatus().catch(() => null);
      if (st) setEtStatus(st);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(`生成口令失败：${msg}`);
      pushLog("warn", `[EasyTier] 生成口令失败：${msg}`);
    } finally {
      setEtBusy(false);
    }
  }, [pushLog, selectedEtNodeIds, serverPool, showToast]);

  // STUN 穿透体检：双 STUN 同端口真实判断圆锥型 / 对称型 NAT
  const handleDiagnoseNat = useCallback(async () => {
    setNatBusy(true);
    pushLog("info", "开始双 STUN 同端口真实 NAT 穿透体检…");
    try {
      const stunList = serverPool
        .filter((n) => inferNodeKind(n) === "stun" && n.status !== "fail")
        .sort((a, b) => (a.lastRttMs ?? 9999) - (b.lastRttMs ?? 9999))
        .map((n) => n.addr);
      const res = await labDiagnoseNat(stunList);
      setNatResult(res);
      pushLog(
        res.natType === "cone" ? "ok" : "warn",
        `[NAT 体检] ${res.label}（${res.serverA} → ${res.mappedA ?? "无"}；${res.serverB} → ${res.mappedB ?? "无"}）`,
      );
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `[NAT 体检] 失败：${msg}`);
    } finally {
      setNatBusy(false);
    }
  }, [pushLog, serverPool]);

  // 方案 A：设置本机中转地址 & 查询对方 9 位 ID 在线状态
  const handleSaveGlobalRelay = useCallback(
    async (addr: string) => {
      const trimmed = addr.trim();
      if (!trimmed) {
        showToast("请先填写或选择一个自建中转地址");
        return;
      }
      try {
        await setRelayAddr(trimmed);
        setSelectedRelayNodeAddr(trimmed);
        setDefaultRelayAddr(trimmed);
        const info = await relayIdStatus().catch(() => null);
        if (info) setRelayIdInfo(info);
        showToast(`已将 ${trimmed} 设为本机中转服务器（重启后生效）`);
        pushLog("ok", `[方案 A · 中转] 本机中转服务器已更新为 ${trimmed}`);
      } catch (e) {
        showToast(e instanceof Error ? e.message : String(e));
      }
    },
    [pushLog, showToast],
  );

  const handleResolvePeerId = useCallback(async () => {
    const relay = selectedRelayNodeAddr.trim() || defaultRelayAddr.trim();
    if (!relay) {
      showToast("请先选择或填写自建中转服务器地址");
      return;
    }
    setRelayLookupBusy(true);
    setRelayLookupRes(null);
    pushLog("info", `[方案 A · 9位ID] 正在通过中转 ${relay} 查询对方 ID「${peerNumericIdInput.trim()}」…`);
    try {
      const res = await relayResolveId(relay, peerNumericIdInput);
      setRelayLookupRes(res);
      pushLog(res.online ? "ok" : "warn", `[方案 A · 9位ID] ${res.detail}`);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      pushLog("warn", `[方案 A · 9位ID] 查询异常：${msg}`);
    } finally {
      setRelayLookupBusy(false);
    }
  }, [defaultRelayAddr, peerNumericIdInput, pushLog, selectedRelayNodeAddr, showToast]);

  const handleBindRelayIdPlan = useCallback(() => {
    const relay = selectedRelayNodeAddr.trim() || defaultRelayAddr.trim();
    if (!relay) {
      showToast("请先填写或选择中转服务器地址");
      return;
    }
    const idLabel = relayLookupRes?.numericId || peerNumericIdInput.trim() || "中转直连";
    const plan: RealTestPlanDto = {
      planId: `plan-relay-${Date.now()}`,
      planName: `自建中转 · ID ${idLabel}`,
      mode: "relay",
      targetAddr: baseTargetIp || "192.168.1.10",
      relayAddr: relay,
      rounds: 3,
    };
    savePlansForTarget([...activePlans, plan]);
    showToast(`已绑定方案「${plan.planName}」`);
    pushLog("ok", `已绑定自建中转方案「${plan.planName}」(${relay})`);
  }, [activePlans, baseTargetIp, defaultRelayAddr, peerNumericIdInput, pushLog, relayLookupRes, savePlansForTarget, selectedRelayNodeAddr, showToast]);

  const handleJoinEtInvite = useCallback(async () => {
    const code = etJoinInput.trim();
    if (!code) {
      showToast("请先粘贴对方发来的 rd-et:// 组网口令");
      return;
    }
    setEtBusy(true);
    try {
      const res = await easytierJoinInvite(code);
      setEtInviteCode(res.inviteCode);
      setEtJoinInput("");
      showToast(res.hint);
      pushLog("ok", `[EasyTier] ${res.hint}`);
      const st = await easytierDetectStatus().catch(() => null);
      if (st) setEtStatus(st);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(`加入失败：${msg}`);
      pushLog("warn", `[EasyTier] 加入失败：${msg}`);
    } finally {
      setEtBusy(false);
    }
  }, [etJoinInput, pushLog, showToast]);

  const handleBindEtPeerIp = useCallback(
    (hostname: string, ipv4: string) => {
      const plan: RealTestPlanDto = {
        planId: `plan-et-${Date.now()}`,
        planName: `EasyTier 直连 (${hostname})`,
        mode: "easytier",
        targetAddr: ipv4,
        relayAddr: "",
        rounds: 3,
      };
      savePlansForTarget([...activePlans, plan]);
      showToast(`已绑定 EasyTier 内网 IP ${ipv4}（彻底绕开 Tailscale）`);
      pushLog("ok", `已将 EasyTier 成员「${hostname}」(${ipv4}) 绑定为当前目标测速方案`);
    },
    [activePlans, pushLog, savePlansForTarget, showToast],
  );

  // 内联 SSH 一键上传代码、启动打洞/中转服务器并自动入池+实测连通
  const handleLabSshDeploy = useCallback(async () => {
    const host = sshHost.trim();
    if (!host) {
      pushLog("warn", "请先填写 Linux 服务器 IP 或域名");
      return;
    }
    setSshBusy(true);
    const label = sshName.trim() || `自建节点 (${host})`;
    pushLog("info", `🚀 开始 SSH 连接 ${sshUser.trim() || "root"}@${host}:${sshPort}，自动上传并启动打洞/中转服务…`);
    try {
      const res = await deployRelaySsh({
        host,
        port: Number(sshPort) || 22,
        user: sshUser.trim() || "root",
        keyPath: sshKeyPath.trim(),
        relayPort: 21118,
        mode: sshMode,
        domain: "",
      });
      for (const line of res.log.split(/\r?\n/).filter((l) => l.trim().length > 0)) {
        pushLog(res.ok ? "info" : "warn", `[SSH 部署] ${line}`);
      }
      if (res.ok && res.relayAddr) {
        const newNode: RelayNodeItem = {
          id: `node-${Date.now().toString(36)}`,
          name: label,
          addr: res.relayAddr,
          lastRttMs: null,
          status: "idle",
        };
        const nextPool = [
          ...serverPool.filter((n) => n.addr !== res.relayAddr),
          newNode,
        ];
        saveServerPool(nextPool);
        setNewPlanRelay(res.relayAddr);
        setShowSshDeploy(false);
        pushLog("ok", `✓ 部署成功！已自动加入共用节点池：${label} (${res.relayAddr})，正在自动测试节点延迟…`);
        // 立刻实测刚部署好的新节点
        const probe = await labProbeTargetPlan({
          planId: `pool-${newNode.id}`,
          planName: label,
          mode: "relay",
          targetAddr: baseTargetIp,
          relayAddr: res.relayAddr,
          rounds: 3,
        });
        saveServerPool(
          nextPool.map((n) =>
            n.id === newNode.id
              ? { ...n, lastRttMs: probe.nodeRttMs ?? probe.avgRttMs, status: probe.ok ? "ok" : "fail" }
              : n,
          ),
        );
        for (const line of probe.logs) {
          pushLog(probe.ok ? "ok" : "warn", `[新节点实测] ${line}`);
        }
      } else {
        pushLog("warn", `✗ SSH 部署未成功：${res.summary}`);
      }
    } catch (e) {
      pushLog("warn", `✗ SSH 部署调用失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setSshBusy(false);
    }
  }, [baseTargetIp, pushLog, saveServerPool, serverPool, sshHost, sshKeyPath, sshMode, sshName, sshPort, sshUser]);

  // 保留兼容引用避免未用告警
  void probeDevice;

  const applyTransportNow = useCallback(
    async (next: LabTransportConfig) => {
      setTransport(next);
      const impact = [
        `路径=${next.routeMode.toUpperCase()}`,
        `拥塞=${next.congestion === "bbr" ? "BBR(跨网抗抖动)" : "Cubic(传统退避)"}`,
        next.mediaFec
          ? `RS-FEC 开启(+${next.fecOverheadPct}% 冗余，弱网防花屏)`
          : "QUIC 可靠流(零冗余，局域网低开销)",
      ].join(" · ");
      pushLog("info", `调整传输层 → ${impact}`);
      try {
        const msg = await labApplyTransport(
          live?.activeDeviceId ?? deviceId ?? null,
          next,
        );
        showToast(msg);
      } catch (e) {
        showToast(e instanceof Error ? e.message : String(e));
      }
    },
    [deviceId, live?.activeDeviceId, pushLog, showToast],
  );

  // 操控域 ③：编码与瓦片毫秒级热调
  const applyCodecNow = useCallback(
    async (next: LabCodecConfig, forceIdr = false) => {
      setCodec(next);
      pushLog(
        "info",
        `调整编码参数 → ${(next.bitrateKbps / 1000).toFixed(1)} Mbps / ${next.maxFps}fps / ${
          next.preferHw ? "硬编优先" : "软编"
        } / ${next.request444 ? "YUV444(文字锐利)" : "YUV420(省带宽)"} / QOI瓦片${
          next.tileEnabled ? "开(静态1:1无损)" : "关"
        }`,
      );
      try {
        await labApplyTuning(
          live?.activeDeviceId ?? deviceId ?? null,
          next,
          transport.fecOverheadPct,
          forceIdr,
        );
        if (forceIdr) {
          showToast("⚡ 已向编码管线发送强制 IDR 关键帧与瓦片重刷指令");
        }
      } catch (e) {
        showToast(e instanceof Error ? e.message : String(e));
      }
    },
    [deviceId, live?.activeDeviceId, pushLog, showToast, transport.fecOverheadPct],
  );

  // 操控域 ④：可编程 Netem 阀门
  const applyNetemNow = useCallback(
    async (next: NetemConfigDto) => {
      setNetem(next);
      pushLog(
        next.enabled ? "warn" : "ok",
        next.enabled
          ? `开启弱网模拟：额外延迟 +${next.delayMs}ms、抖动 ${next.jitterMs}ms、丢包 ${next.lossPct}%（用于模拟跨网/烂WiFi测试抗丢包效果）`
          : "已关闭弱网模拟：恢复真实网络直通（0 额外延迟/丢包）",
      );
      try {
        const res = await labSetNetem(next);
        setNetem(res);
      } catch (e) {
        showToast(e instanceof Error ? e.message : String(e));
      }
    },
    [pushLog, showToast],
  );

  // 载入 Profile 并立刻应用
  const handleSelectProfile = useCallback(
    async (id: string) => {
      setSelectedProfileId(id);
      const found = profiles.find((p) => p.id === id);
      if (!found) return;
      setTransport(found.transport);
      setCodec(found.codec);
      await applyTransportNow(found.transport);
      await applyCodecNow(found.codec, true);
    },
    [applyCodecNow, applyTransportNow, profiles],
  );

  // 保存并锁定为当前设备默认方案
  const handleLockProfile = useCallback(async () => {
    const targetDev = live?.activeDeviceId ?? deviceId ?? null;
    const id = `custom-${Date.now()}`;
    const name = targetDev
      ? `⚡ 锁定方案 (${targetDev})`
      : `⚡ 极客自定义 (${codec.bitrateKbps / 1000}Mbps/${transport.routeMode.toUpperCase()})`;
    const newProf: LabProfile = {
      id,
      name,
      boundDeviceId: targetDev,
      transport,
      codec,
    };
    try {
      const list = await labSaveProfile(newProf);
      setProfiles(list);
      setSelectedProfileId(id);
      showToast(`已固化方案「${name}」并同步到底层配置`);
    } catch (e) {
      showToast(e instanceof Error ? e.message : String(e));
    }
  }, [codec, deviceId, live?.activeDeviceId, showToast, transport]);

  // ↺ Panic Reset 一键逃生舱
  const handlePanicReset = useCallback(async () => {
    setTransport(DEFAULT_TRANSPORT);
    setCodec(DEFAULT_CODEC);
    setNetem(DEFAULT_NETEM);
    try {
      await labPanicReset(live?.activeDeviceId ?? deviceId ?? null);
      showToast("↺ Panic Reset 已执行：Netem 已清空，恢复 AUTO 竞速 + Baseline-Adaptive 码控");
    } catch (e) {
      showToast(e instanceof Error ? e.message : String(e));
    }
  }, [deviceId, live?.activeDeviceId, showToast]);

  // ⚔️ 执行真栈 A/B 矩阵对决
  const handleRunAb = useCallback(async () => {
    const pA = profiles.find((p) => p.id === abIdA) ?? {
      id: "a",
      name: "当前方案 A",
      boundDeviceId: null,
      transport,
      codec,
    };
    const pB = profiles.find((p) => p.id === abIdB) ?? {
      id: "b",
      name: "对比方案 B (RS-FEC)",
      boundDeviceId: null,
      transport: { ...transport, mediaFec: !transport.mediaFec, fecOverheadPct: 25 },
      codec,
    };
    setAbRunning(true);
    try {
      const res = await labRunAbBench(pA, pB);
      setAbResult(res);
    } catch (e) {
      showToast(e instanceof Error ? e.message : String(e));
    } finally {
      setAbRunning(false);
    }
  }, [abIdA, abIdB, codec, profiles, showToast, transport]);

  const [allBenchRunning, setAllBenchRunning] = useState(false);
  const [allBenchResult, setAllBenchResult] = useState<LabAllRoutesBenchDto | null>(null);

  const handleBenchmarkAll = useCallback(async () => {
    setAllBenchRunning(true);
    pushLog("info", `开始对「${effectiveTargetLabel}」执行全方案自动测速与综合打分…`);
    try {
      const res = await labBenchmarkAllRoutes(profiles);
      setAllBenchResult(res);
      showToast(`已测完 ${res.ranked.length} 套方案，最优：${res.bestName}`);
      pushLog(
        "ok",
        `全方案测速完成（共 ${res.ranked.length} 套）→ 最优推荐：${res.bestName}（${res.verdict}）`,
      );
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(msg);
      pushLog("warn", `全方案测速失败：${msg}`);
    } finally {
      setAllBenchRunning(false);
    }
  }, [effectiveTargetLabel, profiles, pushLog, showToast]);

  const onStageMouseMove = (e: MouseEvent<HTMLDivElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return;
    const rx = ((e.clientX - rect.left) / rect.width) * 640;
    const ry = ((e.clientY - rect.top) / rect.height) * 360;
    magPosRef.current = { x: Math.round(rx), y: Math.round(ry) };
    refreshMagnifier();
  };

  const isOnline = Boolean(live?.activeDeviceId);

  return (
    <section
      className={drawer ? styles.labDrawer : styles.labPage}
      aria-label="方案实验室极客操控台"
    >
      {/* ── 顶部操控栏：简洁清晰的 4 步指引（旧编码预设下拉框已移入底部高级区，避免与连接方案冲突） ── */}
      <header className={styles.topBar}>
        <div className={styles.titleGroup}>
          <span className={styles.labBadge}>RD LAB</span>
          <h1 className={styles.labTitle}>方案实验室 · 真栈操控台</h1>
          <span className={styles.targetPill}>
            <span className={isOnline || selectedDevice?.online ? styles.dotOnline : styles.dotSandbox} />
            1. 选目标 → 2. 新增/修改方案 → 3. 自动实测 → 4. 切换使用
          </span>
        </div>

        <div className={styles.actionGroup}>
          {selectedDevice && (
            <button
              type="button"
              className={`${styles.btn} ${styles.btnPrimary}`}
              disabled={connectingTarget}
              onClick={() => void handleConnectWithConfirmed()}
              title="用当前已确定的连接方案直接打开该设备完整桌面"
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

      {/* ── 主工作台：按目标管理多方案 + 真实自动化测速 + 实时动态日志 ── */}
      <div
        style={{
          margin: "0 0 14px",
          padding: "16px",
          borderRadius: "12px",
          background: "rgba(255, 255, 255, 0.03)",
          border: "1px solid rgba(255, 255, 255, 0.1)",
          display: "flex",
          flexDirection: "column",
          gap: "14px",
        }}
      >
        {/* 1) 选择已有目标 或 手动输入 IP */}
        <div style={{ display: "flex", flexWrap: "wrap", gap: "8px", alignItems: "center" }}>
          <strong style={{ fontSize: "14px" }}>① 测试目标：</strong>
          <select
            className={styles.select}
            aria-label="选择要测试的被控设备"
            value={targetDeviceId}
            onChange={(e) => {
              setTargetDeviceId(e.target.value);
              setCustomIp("");
              setRealResults({});
              pushLog("info", "已切换测试目标设备");
            }}
          >
            <option value="">手动输入目标 IP</option>
            {savedDevices.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name} ({d.addr.replace(/:21118$/, "")}){d.online ? " · 在线" : ""}
              </option>
            ))}
          </select>
          <input
            type="text"
            value={customIp}
            onChange={(e) => setCustomIp(e.currentTarget.value)}
            placeholder="或手动输入目标 IP（如 100.81.205.0 / 192.168.1.10，免填端口）"
            aria-label="自定义测试目标 IP"
            style={{
              flex: "1 1 220px",
              minWidth: 0,
              padding: "7px 10px",
              borderRadius: "8px",
              border: "1px solid rgba(255, 255, 255, 0.16)",
              background: "rgba(0, 0, 0, 0.28)",
              color: "inherit",
              fontSize: "13px",
            }}
          />
          <button
            type="button"
            className={styles.btn}
            onClick={() => setShowAddPlan((v) => !v)}
          >
            + 绑定新方案
          </button>
          <button
            type="button"
            className={styles.btn}
            disabled={runningRealBatch || smartDiscovering || testingPlanId !== null}
            onClick={() => void runAllTargetPlansReal()}
            title="只测试当前目标已经绑定好的方案"
          >
            {runningRealBatch ? "⏳ 测速中…" : "▶ 测已绑定方案"}
          </button>
          <button
            type="button"
            className={`${styles.btn} ${styles.btnPrimary}`}
            disabled={runningRealBatch || smartDiscovering || testingPlanId !== null}
            onClick={() => void runSmartDiscovery()}
            title="同时测试已绑定方案 + 节点池中未建立的公共/自建候选节点，自动推荐最快方案"
          >
            {smartDiscovering ? "⏳ 智能寻优中…" : "✨ 智能寻优（含候选）"}
          </button>
        </div>

        {/* 智能诊断与一键采纳推荐横幅（测完自动出现，点 1 下直接切换最优线路） */}
        {smartHint && (
          <div
            style={{
              display: "flex",
              flexWrap: "wrap",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "10px",
              padding: "10px 14px",
              borderRadius: "10px",
              border:
                smartHint.level === "warn"
                  ? "1px solid rgba(248, 81, 73, 0.45)"
                  : "1px solid rgba(63, 185, 80, 0.55)",
              background:
                smartHint.level === "warn"
                  ? "rgba(248, 81, 73, 0.08)"
                  : "rgba(63, 185, 80, 0.1)",
            }}
          >
            <div style={{ display: "flex", flexDirection: "column", gap: "3px", flex: "1 1 320px" }}>
              <strong style={{ fontSize: "13px" }}>{smartHint.title}</strong>
              <span style={{ fontSize: "12px", opacity: 0.86 }}>{smartHint.detail}</span>
            </div>
            <div style={{ display: "flex", gap: "6px", flexShrink: 0 }}>
              {smartHint.candidateNode && (
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  onClick={() => void handleAdoptCandidate(smartHint.candidateNode!)}
                >
                  ⚡ 一键采纳并启用
                </button>
              )}
              {smartHint.boundPlan && confirmedPlanByTarget[targetKey] !== smartHint.boundPlan.planId && (
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  onClick={() => void handleConfirmPlan(smartHint.boundPlan!)}
                >
                  切换使用
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

        {/* 新增绑定方案表单（1 个目标可绑定任意多个方案） */}
        {showAddPlan && (
          <div
            style={{
              display: "flex",
              flexWrap: "wrap",
              gap: "8px",
              padding: "12px",
              borderRadius: "8px",
              background: "rgba(61, 126, 255, 0.08)",
              border: "1px solid rgba(61, 126, 255, 0.28)",
              alignItems: "center",
            }}
          >
            <input
              type="text"
              value={newPlanName}
              onChange={(e) => setNewPlanName(e.currentTarget.value)}
              placeholder="方案名（如：公司 Tailscale / 香港中转）"
              aria-label="新绑定方案名称"
              style={{
                flex: "1 1 160px",
                padding: "6px 10px",
                borderRadius: "6px",
                border: "1px solid rgba(255, 255, 255, 0.16)",
                background: "rgba(0, 0, 0, 0.3)",
                color: "inherit",
                fontSize: "12px",
              }}
            />
            <select
              className={styles.select}
              aria-label="新绑定方案类型"
              value={newPlanMode}
              onChange={(e) =>
                setNewPlanMode(e.target.value as RealTestPlanDto["mode"])
              }
            >
              <option value="direct">局域网 / 直连 (QUIC)</option>
              <option value="tailscale">Tailscale 跨网</option>
              <option value="punch">STUN / 打洞服务器</option>
              <option value="relay">自建中转服务器</option>
              <option value="easytier">EasyTier 组网</option>
            </select>
            {newPlanMode === "relay" || newPlanMode === "punch" ? (
              <>
                {serverPool.length > 0 && (
                  <select
                    className={styles.select}
                    aria-label="选择共用打洞或中转服务器"
                    value={serverPool.some((n) => n.addr === newPlanRelay) ? newPlanRelay : ""}
                    onChange={(e) => {
                      if (e.target.value) setNewPlanRelay(e.target.value);
                    }}
                  >
                    <option value="">从共用节点池选择…</option>
                    {serverPool.map((n) => (
                      <option key={n.id} value={n.addr}>
                        {n.name} ({n.addr}){n.lastRttMs !== null ? ` · ${n.lastRttMs}ms` : ""}
                      </option>
                    ))}
                  </select>
                )}
                <input
                  type="text"
                  value={newPlanRelay}
                  onChange={(e) => setNewPlanRelay(e.currentTarget.value)}
                  placeholder="打洞/中转服务器地址（如 1.2.3.4:21118 或 https://xxx.workers.dev）"
                  aria-label="新绑定方案中转地址"
                  style={{
                    flex: "1 1 200px",
                    padding: "6px 10px",
                    borderRadius: "6px",
                    border: "1px solid rgba(255, 255, 255, 0.16)",
                    background: "rgba(0, 0, 0, 0.3)",
                    color: "inherit",
                    fontSize: "12px",
                  }}
                />
                <button
                  type="button"
                  className={styles.btn}
                  onClick={() => setShowSshDeploy((v) => !v)}
                >
                  🚀 没有服务器？SSH 一键部署
                </button>
              </>
            ) : (
              <input
                type="text"
                value={newPlanAddr}
                onChange={(e) => setNewPlanAddr(e.currentTarget.value)}
                placeholder={`目标 IP（留空默认 ${baseTargetIp || "192.168.1.10"}）`}
                aria-label="新绑定方案目标地址"
                style={{
                  flex: "1 1 180px",
                  padding: "6px 10px",
                  borderRadius: "6px",
                  border: "1px solid rgba(255, 255, 255, 0.16)",
                  background: "rgba(0, 0, 0, 0.3)",
                  color: "inherit",
                  fontSize: "12px",
                }}
              />
            )}
            <button
              type="button"
              className={`${styles.btn} ${styles.btnPrimary}`}
              onClick={handleAddTargetPlan}
            >
              {editingPlanId ? "保存修改" : "保存绑定"}
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
        )}

        {/* ── 4 模式分标签配置台（点哪种方式，下方只展示它自己的节点、体检与配置） ── */}
        <div
          style={{
            padding: "10px 12px",
            borderRadius: "10px",
            background: "rgba(255, 255, 255, 0.025)",
            border: "1px solid rgba(255, 255, 255, 0.09)",
            display: "flex",
            flexDirection: "column",
            gap: "10px",
          }}
        >
          {/* 顶部 3 个真实连接方式标签 + 共用 NAT 体检按钮 + 收起开关 */}
          <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", justifyContent: "space-between", gap: "8px" }}>
            <div style={{ display: "flex", flexWrap: "wrap", gap: "6px" }} role="tablist" aria-label="连接与组网方式">
              {(
                [
                  ["direct", "直连 / Tailscale"],
                  ["easytier", `EasyTier 组网 (${serverPool.filter((n) => inferNodeKind(n) === "easytier").length})`],
                  ["relay", `自建中转 · 9位ID (${serverPool.filter((n) => inferNodeKind(n) === "relay").length})`],
                ] as const
              ).map(([tabId, label]) => {
                const active = activeModeTab === tabId && modeBodyOpen;
                return (
                  <button
                    key={tabId}
                    type="button"
                    role="tab"
                    aria-selected={active}
                    onClick={() => {
                      if (activeModeTab === tabId && modeBodyOpen) {
                        setModeBodyOpen(false);
                      } else {
                        setActiveModeTab(tabId);
                        setModeBodyOpen(true);
                        if (tabId === "easytier" || tabId === "relay") {
                          setPoolFilter(tabId);
                        }
                      }
                    }}
                    style={{
                      padding: "5px 12px",
                      borderRadius: "8px",
                      fontSize: "12.5px",
                      fontWeight: active ? 600 : 500,
                      cursor: "pointer",
                      border: active
                        ? "1px solid rgba(88, 166, 255, 0.75)"
                        : "1px solid rgba(255, 255, 255, 0.14)",
                      background: active ? "rgba(88, 166, 255, 0.18)" : "rgba(0, 0, 0, 0.22)",
                      color: "inherit",
                    }}
                  >
                    {label}
                  </button>
                );
              })}
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
              <button
                type="button"
                className={styles.btn}
                disabled={natBusy}
                onClick={() => void handleDiagnoseNat()}
                title="双 STUN 同端口检测本机 NAT 类型（EasyTier 与自建中转 P2P 打洞共用）"
                style={
                  natResult
                    ? {
                        borderColor:
                          natResult.natType === "cone"
                            ? "rgba(63, 185, 80, 0.55)"
                            : "rgba(229, 181, 103, 0.55)",
                        color: natResult.natType === "cone" ? "#3fb950" : "#e5b567",
                      }
                    : undefined
                }
              >
                {natBusy
                  ? "NAT 体检中…"
                  : natResult
                    ? natResult.natType === "cone"
                      ? "圆锥 NAT · 可打洞"
                      : natResult.natType === "symmetric"
                        ? "对称 NAT · 建议中转"
                        : "NAT 状态待确认"
                    : "NAT 穿透体检"}
              </button>
              <button
                type="button"
                className={styles.btn}
                aria-expanded={stunDrawerOpen}
                onClick={() => setStunDrawerOpen((v) => !v)}
                title="查看、拉取、测速或批量管理公共 STUN 源"
              >
                {stunDrawerOpen
                  ? "收起 STUN 源"
                  : `STUN 源 (${serverPool.filter((n) => inferNodeKind(n) === "stun").length})`}
              </button>
              <button
                type="button"
                className={styles.btn}
                aria-expanded={modeBodyOpen}
                onClick={() => setModeBodyOpen((v) => !v)}
              >
                {modeBodyOpen ? "收起配置区" : "展开配置区"}
              </button>
            </div>
          </div>

          {/* 轻量化公共 STUN 源管理抽屉（平时隐藏，点「STUN 源」展开查看/拉取/批量粘贴/单点测速） */}
          {stunDrawerOpen && (() => {
            const stunNodes = serverPool.filter((n) => inferNodeKind(n) === "stun");
            return (
              <div
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "8px",
                  padding: "10px",
                  borderRadius: "8px",
                  background: "rgba(0, 0, 0, 0.24)",
                  border: "1px solid rgba(255, 255, 255, 0.1)",
                }}
              >
                <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", justifyContent: "space-between", gap: "6px" }}>
                  <strong style={{ fontSize: "12px" }}>
                    公共 STUN 源管理（共用：NAT 体检 / 智能寻优，零流量经过）
                  </strong>
                  <div style={{ display: "flex", flexWrap: "wrap", gap: "6px" }}>
                    <button
                      type="button"
                      className={styles.btn}
                      disabled={fetchingPublic}
                      onClick={() => void handleFetchPublicNodes()}
                    >
                      {fetchingPublic ? "拉取中…" : "拉取公共 STUN"}
                    </button>
                    <button
                      type="button"
                      className={styles.btn}
                      disabled={probingPool || stunNodes.length === 0}
                      onClick={() => {
                        setPoolFilter("stun");
                        void handleProbeServerPool();
                      }}
                    >
                      {probingPool ? "测速中…" : "测全部 STUN"}
                    </button>
                    <button
                      type="button"
                      className={styles.btn}
                      onClick={() => {
                        setBatchNodeKind("stun");
                        setShowBatchAdd((v) => !v);
                      }}
                    >
                      + 批量粘贴 STUN
                    </button>
                  </div>
                </div>
                {showBatchAdd && batchNodeKind === "stun" && !modeBodyOpen && (
                  <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
                    <textarea
                      rows={3}
                      value={batchNodeText}
                      onChange={(e) => setBatchNodeText(e.currentTarget.value)}
                      placeholder={"一行一个 STUN 地址，例如：\nstun.miwifi.com:3478\nstun.chat.bilibili.com:3478"}
                      aria-label="批量 STUN 输入框"
                      style={{
                        width: "100%",
                        boxSizing: "border-box",
                        padding: "6px 8px",
                        borderRadius: "6px",
                        border: "1px solid rgba(255,255,255,0.16)",
                        background: "rgba(0,0,0,0.32)",
                        color: "inherit",
                        fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                        fontSize: "12px",
                      }}
                    />
                    <div style={{ display: "flex", justifyContent: "flex-end", gap: "6px" }}>
                      <button type="button" className={styles.btn} onClick={() => setShowBatchAdd(false)}>
                        取消
                      </button>
                      <button type="button" className={`${styles.btn} ${styles.btnPrimary}`} onClick={handleBatchAddNodes}>
                        批量加入
                      </button>
                    </div>
                  </div>
                )}
                {stunNodes.length === 0 ? (
                  <div style={{ fontSize: "12px", opacity: 0.7 }}>
                    暂无自定义 STUN 节点（体检时会自动使用内置国内大厂 STUN）。可点「拉取公共 STUN」或「+ 批量粘贴 STUN」管理。
                  </div>
                ) : (
                  <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(235px, 1fr))", gap: "6px", maxHeight: "155px", overflowY: "auto" }}>
                    {stunNodes.map((node) => (
                      <div
                        key={node.id}
                        style={{
                          display: "flex",
                          flexDirection: "column",
                          gap: "3px",
                          padding: "7px 9px",
                          borderRadius: "7px",
                          fontSize: "12px",
                          border: node.status === "ok" ? "1px solid rgba(63,185,80,0.45)" : "1px solid rgba(255,255,255,0.12)",
                          background: "rgba(0,0,0,0.28)",
                        }}
                      >
                        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: "6px" }}>
                          <span style={{ fontWeight: 600, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{node.name}</span>
                          <div style={{ display: "inline-flex", alignItems: "center", gap: "4px", flexShrink: 0 }}>
                            <button
                              type="button"
                              title="点击单独测速并刷新延迟"
                              disabled={probingPool || probingNodeId === node.id}
                              onClick={() => void handleProbeSingleNode(node)}
                              style={{
                                cursor: probingPool || probingNodeId === node.id ? "wait" : "pointer",
                                background: "rgba(255,255,255,0.06)",
                                border: "1px solid rgba(255,255,255,0.14)",
                                borderRadius: "4px",
                                padding: "1px 6px",
                                fontWeight: 700,
                                fontSize: "11px",
                                color: node.status === "ok" ? "#3fb950" : node.status === "fail" ? "#f85149" : "inherit",
                              }}
                            >
                              {probingNodeId === node.id ? "…" : node.lastRttMs !== null ? `${node.lastRttMs}ms` : node.status === "fail" ? "超时" : "测速"}
                            </button>
                            <button
                              type="button"
                              aria-label={`移除 STUN ${node.name}`}
                              onClick={() => saveServerPool(serverPool.filter((n) => n.id !== node.id))}
                              style={{ background: "none", border: "none", color: "inherit", opacity: 0.55, cursor: "pointer", padding: "0 2px" }}
                            >
                              ×
                            </button>
                          </div>
                        </div>
                        <code style={{ opacity: 0.78, fontSize: "11px" }}>{node.addr}</code>
                        {node.mappedAddr && <div style={{ fontSize: "11px", color: "#58a6ff" }}>公网映射：{node.mappedAddr}</div>}
                      </div>
                    ))}
                  </div>
                )}
              </div>
            );
          })()}

          {/* 共用 NAT 体检结论条（EasyTier 与自建中转打洞共用，点击 NAT 穿透体检后显示，可关闭） */}
          {natResult && (
            <div
              style={{
                display: "flex",
                flexWrap: "wrap",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "8px",
                padding: "7px 10px",
                borderRadius: "8px",
                border: natResult.natType === "cone" ? "1px solid rgba(63,185,80,0.45)" : "1px solid rgba(229,181,103,0.45)",
                background: natResult.natType === "cone" ? "rgba(63,185,80,0.08)" : "rgba(229,181,103,0.08)",
                fontSize: "12px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "2px", flex: "1 1 320px" }}>
                <strong>{natResult.label}</strong>
                <span style={{ opacity: 0.85 }}>{natResult.advice}</span>
                {natResult.mappedA && (
                  <span style={{ fontFamily: "monospace", fontSize: "11px", opacity: 0.78 }}>
                    公网映射：{natResult.mappedA} ({natResult.rttAMs}ms)
                    {natResult.mappedB ? ` / ${natResult.mappedB} (${natResult.rttBMs}ms)` : ""}
                  </span>
                )}
              </div>
              <button type="button" className={styles.btn} onClick={() => setNatResult(null)}>
                关闭
              </button>
            </div>
          )}

          {modeBodyOpen && (
            <div style={{ display: "flex", flexDirection: "column", gap: "10px", paddingTop: "2px" }}>
              {/* 公共复用：批量添加（一行一个）文本框，随当前标签页默认类型自动匹配 */}
              {showBatchAdd && activeModeTab !== "direct" && (
                <div style={{ display: "flex", flexDirection: "column", gap: "8px", padding: "10px", borderRadius: "8px", background: "rgba(88,166,255,0.06)", border: "1px solid rgba(88,166,255,0.28)" }}>
                  <div style={{ display: "flex", flexWrap: "wrap", justifyContent: "space-between", alignItems: "center", gap: "8px" }}>
                    <span style={{ fontSize: "12px", fontWeight: 600 }}>
                      批量粘贴节点（一行一个，支持纯地址或「名称,地址」，自动去重）
                    </span>
                    <select
                      className={styles.select}
                      aria-label="批量节点类型"
                      value={batchNodeKind}
                      onChange={(e) => setBatchNodeKind(e.target.value as "auto" | "stun" | "easytier" | "relay")}
                    >
                      <option value="auto">自动识别类型（推荐）</option>
                      <option value="stun">全部作为 公共 STUN</option>
                      <option value="easytier">全部作为 EasyTier</option>
                      <option value="relay">全部作为 自建中转</option>
                    </select>
                  </div>
                  <textarea
                    rows={4}
                    value={batchNodeText}
                    onChange={(e) => setBatchNodeText(e.currentTarget.value)}
                    placeholder={"一行一个节点，例如：\nstun.miwifi.com:3478\ntcp://public.easytier.top:11010\n阿里云上海, 45.x.x.x:21118"}
                    aria-label="批量节点输入框"
                    style={{
                      width: "100%",
                      boxSizing: "border-box",
                      padding: "8px 10px",
                      borderRadius: "6px",
                      border: "1px solid rgba(255,255,255,0.16)",
                      background: "rgba(0,0,0,0.32)",
                      color: "inherit",
                      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                      fontSize: "12px",
                      lineHeight: 1.5,
                      resize: "vertical",
                    }}
                  />
                  <div style={{ display: "flex", justifyContent: "flex-end", gap: "6px" }}>
                    <button type="button" className={styles.btn} onClick={() => setShowBatchAdd(false)}>
                      取消
                    </button>
                    <button type="button" className={`${styles.btn} ${styles.btnPrimary}`} onClick={handleBatchAddNodes}>
                      批量加入节点池
                    </button>
                  </div>
                </div>
              )}

              {/* ═══ TAB 1：直连 / Tailscale ═══ */}
              {activeModeTab === "direct" && (
                <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", justifyContent: "space-between", gap: "10px", padding: "8px 10px", borderRadius: "8px", background: "rgba(0,0,0,0.22)" }}>
                  <div style={{ fontSize: "12px", lineHeight: 1.6, opacity: 0.88 }}>
                    <div>
                      <strong>当前目标地址：</strong>
                      <code>{baseTargetIp || "尚未输入 IP"}</code>
                      {baseTargetIp.startsWith("100.")
                        ? "（Tailscale 虚拟 IP：若实测延迟 > 200ms 说明走了海外 DERP 中继，建议切到「EasyTier 组网」或「自建中转 · 9位ID」）"
                        : "（局域网 / 公网直连地址：同一局域网下支持 4K@144Hz 极限低延迟）"}
                    </div>
                  </div>
                  <button
                    type="button"
                    className={`${styles.btn} ${styles.btnPrimary}`}
                    onClick={() => {
                      setShowAddPlan(true);
                      setEditingPlanId(null);
                      setNewPlanMode(baseTargetIp.startsWith("100.") ? "tailscale" : "direct");
                      setNewPlanAddr(baseTargetIp);
                      setNewPlanName(baseTargetIp.startsWith("100.") ? "Tailscale 直连" : "局域网直连");
                    }}
                  >
                    + 添加直连方案
                  </button>
                </div>
              )}

              {/* ═══ TAB 2：EasyTier 组网（方案 B：勾选已测活节点 → 生成口令 → 绑定 10.126.x.x） ═══ */}
              {activeModeTab === "easytier" && (() => {
                const etNodes = serverPool.filter((n) => inferNodeKind(n) === "easytier");
                return (
                  <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
                    <div style={{ display: "flex", flexWrap: "wrap", gap: "6px", alignItems: "center", justifyContent: "space-between" }}>
                      <div style={{ display: "flex", flexWrap: "wrap", gap: "6px" }}>
                        <button
                          type="button"
                          className={styles.btn}
                          disabled={fetchingPublic}
                          onClick={() => void handleFetchPublicNodes()}
                        >
                          {fetchingPublic ? "拉取中…" : "实时拉取公共节点"}
                        </button>
                        <button
                          type="button"
                          className={styles.btn}
                          disabled={probingPool || etNodes.length === 0}
                          onClick={() => {
                            setPoolFilter("easytier");
                            void handleProbeServerPool();
                          }}
                        >
                          {probingPool ? "测速中…" : "测本组延迟"}
                        </button>
                        <button
                          type="button"
                          className={styles.btn}
                          onClick={() => {
                            setBatchNodeKind("easytier");
                            setShowBatchAdd((v) => !v);
                          }}
                        >
                          + 批量粘贴节点
                        </button>
                        <button
                          type="button"
                          className={styles.btn}
                          onClick={() => setShowSubInput((v) => !v)}
                        >
                          自定义订阅源
                        </button>
                      </div>
                      <span style={{ fontSize: "11.5px", opacity: 0.72 }}>
                        勾选 1~3 个绿灯节点后生成口令，双端粘贴即分配 10.126.x.x 直连
                      </span>
                    </div>

                    {showSubInput && (
                      <div style={{ display: "flex", flexWrap: "wrap", gap: "8px", alignItems: "center", padding: "8px", borderRadius: "8px", background: "rgba(0,0,0,0.22)" }}>
                        <input
                          type="text"
                          value={customSubUrl}
                          onChange={(e) => setCustomSubUrl(e.currentTarget.value)}
                          placeholder="填入 EasyTier 社区节点 API 或 txt 订阅地址（留空用内置源）"
                          aria-label="自定义公共节点订阅地址"
                          style={{ flex: "1 1 260px", padding: "6px 10px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }}
                        />
                        <button type="button" className={`${styles.btn} ${styles.btnPrimary}`} disabled={fetchingPublic} onClick={() => void handleFetchPublicNodes()}>
                          保存并拉取
                        </button>
                      </div>
                    )}

                    {/* EasyTier 节点勾选网格 */}
                    {etNodes.length === 0 ? (
                      <div style={{ fontSize: "12px", opacity: 0.7 }}>
                        暂无 EasyTier 引导节点：点上方「实时拉取公共节点」或「+ 批量粘贴节点」添加后即可勾选生成口令。
                      </div>
                    ) : (
                      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(250px, 1fr))", gap: "8px", maxHeight: "170px", overflowY: "auto" }}>
                        {etNodes.map((node) => {
                          const checked = selectedEtNodeIds.includes(node.id);
                          return (
                            <div
                              key={node.id}
                              style={{
                                display: "flex",
                                flexDirection: "column",
                                gap: "4px",
                                padding: "8px 10px",
                                borderRadius: "8px",
                                fontSize: "12px",
                                border: checked
                                  ? "1px solid rgba(88,166,255,0.75)"
                                  : node.status === "ok"
                                    ? "1px solid rgba(63,185,80,0.45)"
                                    : "1px solid rgba(255,255,255,0.12)",
                                background: checked ? "rgba(88,166,255,0.1)" : "rgba(0,0,0,0.28)",
                              }}
                            >
                              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: "6px" }}>
                                <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer", overflow: "hidden" }}>
                                  <input
                                    type="checkbox"
                                    checked={checked}
                                    onChange={() =>
                                      setSelectedEtNodeIds((prev) =>
                                        prev.includes(node.id)
                                          ? prev.filter((id) => id !== node.id)
                                          : [...prev, node.id],
                                      )
                                    }
                                  />
                                  <span style={{ fontWeight: 600, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                                    {node.name}
                                  </span>
                                </label>
                                <div style={{ display: "inline-flex", alignItems: "center", gap: "4px", flexShrink: 0 }}>
                                  <button
                                    type="button"
                                    title="点击单独测速并刷新延迟"
                                    disabled={probingPool || probingNodeId === node.id}
                                    onClick={() => void handleProbeSingleNode(node)}
                                    style={{
                                      cursor: probingPool || probingNodeId === node.id ? "wait" : "pointer",
                                      background: "rgba(255,255,255,0.06)",
                                      border: "1px solid rgba(255,255,255,0.14)",
                                      borderRadius: "4px",
                                      padding: "1px 6px",
                                      fontWeight: 700,
                                      fontSize: "11px",
                                      color: node.status === "ok" ? "#3fb950" : node.status === "fail" ? "#f85149" : "inherit",
                                    }}
                                  >
                                    {probingNodeId === node.id ? "…" : node.lastRttMs !== null ? `${node.lastRttMs}ms` : node.status === "fail" ? "超时" : "测速"}
                                  </button>
                                  <button
                                    type="button"
                                    aria-label={`移除节点 ${node.name}`}
                                    onClick={() => saveServerPool(serverPool.filter((n) => n.id !== node.id))}
                                    style={{ background: "none", border: "none", color: "inherit", opacity: 0.55, cursor: "pointer", padding: "0 2px" }}
                                  >
                                    ×
                                  </button>
                                </div>
                              </div>
                              <code style={{ opacity: 0.78, fontSize: "11px", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                                {node.addr}
                              </code>
                            </div>
                          );
                        })}
                      </div>
                    )}

                    {/* EasyTier 口令生成 / 粘贴加入 / 在线成员 */}
                    {etStatus && !etStatus.installed && (
                      <div style={{ display: "flex", flexDirection: "column", gap: "8px", fontSize: "12px", padding: "10px 12px", borderRadius: "8px", background: "rgba(229, 181, 103, 0.1)", border: "1px solid rgba(229, 181, 103, 0.35)" }}>
                        <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", justifyContent: "space-between", gap: "8px" }}>
                          <strong>本机尚未安装 easytier-core（组网核心引擎）</strong>
                          <div style={{ display: "flex", flexWrap: "wrap", gap: "6px" }}>
                            <button
                              type="button"
                              className={`${styles.btn} ${styles.btnPrimary}`}
                              disabled={etInstalling}
                              onClick={async () => {
                                setEtInstalling(true);
                                pushLog("info", "[EasyTier] 正在从 GitHub Releases 自动下载并安装当前平台对应的 easytier-core…");
                                try {
                                  const res = await easytierAutoInstall();
                                  showToast(res.detail);
                                  pushLog("ok", `[EasyTier] ${res.detail}`);
                                  const st = await easytierDetectStatus().catch(() => null);
                                  if (st) setEtStatus(st);
                                } catch (e) {
                                  const msg = e instanceof Error ? e.message : String(e);
                                  showToast(`自动下载失败：${msg}`);
                                  pushLog("warn", `[EasyTier] 自动下载失败：${msg}`);
                                } finally {
                                  setEtInstalling(false);
                                }
                              }}
                            >
                              {etInstalling ? "正在下载并解压…" : "一键自动下载安装"}
                            </button>
                            <a
                              href="https://github.com/EasyTier/EasyTier/releases/latest"
                              target="_blank"
                              rel="noreferrer"
                              className={styles.btn}
                              style={{ textDecoration: "none" }}
                            >
                              手动前往 GitHub 下载
                            </a>
                          </div>
                        </div>
                        <div style={{ opacity: 0.85, lineHeight: 1.6 }}>
                          手动下载说明（不要下带 <code>gui</code> 的版本）：
                          Windows 选 <code>easytier-windows-x86_64-v*.zip</code>（解压全部文件放入 <code>C:\tools\</code>）；
                          macOS M 芯片选 <code>easytier-macos-aarch64-v*.zip</code>，Intel 选 <code>easytier-macos-x86_64-v*.zip</code>（放入 <code>~/Library/Application Support/rd/app/tools/</code>）。
                        </div>
                      </div>
                    )}
                    <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: "8px" }}>
                      <button
                        type="button"
                        className={`${styles.btn} ${styles.btnPrimary}`}
                        disabled={etBusy}
                        onClick={() => void handleGenerateEtInvite()}
                      >
                        {etBusy
                          ? "处理中…"
                          : selectedEtNodeIds.length > 0
                            ? `用已选 ${selectedEtNodeIds.length} 个节点生成口令`
                            : "用最快可用节点生成口令"}
                      </button>
                      {etInviteCode && (
                        <>
                          <input
                            type="text"
                            readOnly
                            value={etInviteCode}
                            aria-label="当前 EasyTier 组网口令"
                            style={{ flex: "1 1 220px", padding: "6px 10px", borderRadius: "6px", border: "1px solid rgba(63,185,80,0.4)", background: "rgba(0,0,0,0.3)", color: "#3fb950", fontFamily: "monospace", fontSize: "11.5px" }}
                          />
                          <button
                            type="button"
                            className={styles.btn}
                            onClick={() => {
                              void navigator.clipboard?.writeText(etInviteCode);
                              showToast("已复制组网口令，发给对方粘贴即可");
                            }}
                          >
                            复制口令
                          </button>
                        </>
                      )}
                    </div>
                    <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: "8px" }}>
                      <input
                        type="text"
                        value={etJoinInput}
                        onChange={(e) => setEtJoinInput(e.currentTarget.value)}
                        placeholder="粘贴对方发来的 rd-et://... 口令加入同一内网"
                        aria-label="粘贴 EasyTier 组网口令"
                        style={{ flex: "1 1 240px", padding: "6px 10px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }}
                      />
                      <button type="button" className={styles.btn} disabled={etBusy} onClick={() => void handleJoinEtInvite()}>
                        一键加入
                      </button>
                      <button type="button" className={styles.btn} disabled={etBusy} onClick={() => void handleRefreshEtStatus()}>
                        刷新在线成员
                      </button>
                    </div>
                    {etStatus && etStatus.peers.length > 0 && (
                      <div style={{ display: "flex", flexWrap: "wrap", gap: "8px" }}>
                        {etStatus.peers.map((p) => (
                          <div
                            key={p.ipv4}
                            style={{ display: "inline-flex", alignItems: "center", gap: "8px", padding: "5px 10px", borderRadius: "8px", border: "1px solid rgba(63,185,80,0.4)", background: "rgba(0,0,0,0.28)", fontSize: "12px" }}
                          >
                            <strong>{p.hostname}</strong>
                            <code style={{ color: "#58a6ff" }}>{p.ipv4}</code>
                            <span style={{ opacity: 0.78 }}>{p.cost}{p.latencyMs !== null ? ` · ${p.latencyMs}ms` : ""}</span>
                            <button type="button" className={styles.btn} onClick={() => handleBindEtPeerIp(p.hostname, p.ipv4)}>
                              + 绑定此 IP
                            </button>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })()}

              {/* ═══ TAB 3：自建中转 · 9 位设备 ID 码（方案 A：双端注册 → 9 位 ID 打洞直连 + 中转兜底 + SSH 一键部署） ═══ */}
              {activeModeTab === "relay" && (() => {
                const relayNodes = serverPool.filter((n) => inferNodeKind(n) === "relay");
                return (
                  <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
                    {/* 本机 9 位 ID + 全局中转地址 */}
                    <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", justifyContent: "space-between", gap: "10px", padding: "10px 12px", borderRadius: "8px", background: "rgba(88,166,255,0.07)", border: "1px solid rgba(88,166,255,0.28)" }}>
                      <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: "10px" }}>
                        <span style={{ fontSize: "12px", opacity: 0.8 }}>本机 9 位 ID：</span>
                        <strong style={{ fontFamily: "monospace", fontSize: "15px", letterSpacing: "1px", color: "#58a6ff" }}>
                          {relayIdInfo?.numericId ?? "读取中…"}
                        </strong>
                        {relayIdInfo && (
                          <button
                            type="button"
                            className={styles.btn}
                            onClick={() => {
                              void navigator.clipboard?.writeText(relayIdInfo.numericId);
                              showToast(`已复制本机 9 位 ID：${relayIdInfo.numericId}`);
                            }}
                          >
                            复制本机 ID
                          </button>
                        )}
                      </div>
                      <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: "6px", flex: "1 1 280px", justifyContent: "flex-end" }}>
                        <input
                          type="text"
                          value={selectedRelayNodeAddr}
                          onChange={(e) => setSelectedRelayNodeAddr(e.currentTarget.value)}
                          placeholder="自建中转地址（如 45.x.x.x:21118）"
                          aria-label="自建中转服务器地址"
                          style={{ flex: "1 1 180px", maxWidth: "240px", padding: "6px 10px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }}
                        />
                        <button type="button" className={styles.btn} onClick={() => void handleSaveGlobalRelay(selectedRelayNodeAddr)}>
                          设为本机中转
                        </button>
                      </div>
                    </div>

                    {/* 输入对方 9 位 ID 实时查在线 & 绑定方案 */}
                    <div style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: "8px" }}>
                      <input
                        type="text"
                        value={peerNumericIdInput}
                        onChange={(e) => setPeerNumericIdInput(e.currentTarget.value)}
                        placeholder="输入对方 9 位设备 ID（如 482 916 305）"
                        aria-label="对方 9 位设备 ID"
                        style={{ flex: "1 1 220px", padding: "6px 10px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px", fontFamily: "monospace" }}
                      />
                      <button
                        type="button"
                        className={`${styles.btn} ${styles.btnPrimary}`}
                        disabled={relayLookupBusy}
                        onClick={() => void handleResolvePeerId()}
                      >
                        {relayLookupBusy ? "查询中…" : "查询对方 ID 在线"}
                      </button>
                      <button type="button" className={styles.btn} onClick={handleBindRelayIdPlan}>
                        + 绑定 9位ID 方案
                      </button>
                      <button type="button" className={styles.btn} onClick={() => setShowSshDeploy((v) => !v)}>
                        {showSshDeploy ? "收起 SSH 部署" : "SSH 一键部署 VPS"}
                      </button>
                      <button
                        type="button"
                        className={styles.btn}
                        onClick={() => {
                          setBatchNodeKind("relay");
                          setShowBatchAdd((v) => !v);
                        }}
                      >
                        + 批量添加中转
                      </button>
                    </div>

                    {relayLookupRes && (
                      <div
                        style={{
                          fontSize: "12px",
                          padding: "7px 10px",
                          borderRadius: "6px",
                          border: relayLookupRes.online ? "1px solid rgba(63,185,80,0.5)" : "1px solid rgba(229,181,103,0.45)",
                          background: relayLookupRes.online ? "rgba(63,185,80,0.09)" : "rgba(229,181,103,0.08)",
                        }}
                      >
                        {relayLookupRes.detail}
                      </div>
                    )}

                    {/* 自建中转节点网格 */}
                    {relayNodes.length > 0 && (
                      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(245px, 1fr))", gap: "8px", maxHeight: "150px", overflowY: "auto" }}>
                        {relayNodes.map((node) => (
                          <div
                            key={node.id}
                            style={{
                              display: "flex",
                              flexDirection: "column",
                              gap: "4px",
                              padding: "8px 10px",
                              borderRadius: "8px",
                              fontSize: "12px",
                              border: node.status === "ok" ? "1px solid rgba(63,185,80,0.45)" : "1px solid rgba(255,255,255,0.12)",
                              background: "rgba(0,0,0,0.28)",
                            }}
                          >
                            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: "6px" }}>
                              <span style={{ fontWeight: 600 }}>{node.name}</span>
                              <div style={{ display: "inline-flex", alignItems: "center", gap: "4px" }}>
                                <button
                                  type="button"
                                  title="点击单独测速并刷新延迟"
                                  disabled={probingPool || probingNodeId === node.id}
                                  onClick={() => void handleProbeSingleNode(node)}
                                  style={{
                                    cursor: probingPool || probingNodeId === node.id ? "wait" : "pointer",
                                    background: "rgba(255,255,255,0.06)",
                                    border: "1px solid rgba(255,255,255,0.14)",
                                    borderRadius: "4px",
                                    padding: "1px 6px",
                                    fontWeight: 700,
                                    fontSize: "11px",
                                    color: node.status === "ok" ? "#3fb950" : node.status === "fail" ? "#f85149" : "inherit",
                                  }}
                                >
                                  {probingNodeId === node.id ? "…" : node.lastRttMs !== null ? `${node.lastRttMs}ms` : node.status === "fail" ? "超时" : "测速"}
                                </button>
                                <button
                                  type="button"
                                  aria-label={`移除节点 ${node.name}`}
                                  onClick={() => saveServerPool(serverPool.filter((n) => n.id !== node.id))}
                                  style={{ background: "none", border: "none", color: "inherit", opacity: 0.55, cursor: "pointer", padding: "0 2px" }}
                                >
                                  ×
                                </button>
                              </div>
                            </div>
                            <code style={{ opacity: 0.78, fontSize: "11px" }}>{node.addr}</code>
                            <div style={{ display: "flex", gap: "6px", marginTop: "2px" }}>
                              <button type="button" className={styles.btn} onClick={() => setSelectedRelayNodeAddr(node.addr)}>
                                选用此节点
                              </button>
                              <button type="button" className={styles.btn} onClick={() => handleBindNodeToTarget(node)}>
                                + 绑定方案
                              </button>
                            </div>
                          </div>
                        ))}
                      </div>
                    )}

                    {/* SSH 一键上传部署自建中转 */}
                    {showSshDeploy && (
                      <div style={{ display: "flex", flexWrap: "wrap", gap: "8px", padding: "10px", borderRadius: "8px", background: "rgba(63,185,80,0.06)", border: "1px solid rgba(63,185,80,0.3)", alignItems: "center" }}>
                        <input type="text" value={sshName} onChange={(e) => setSshName(e.currentTarget.value)} placeholder="节点名（如：香港 VPS）" aria-label="新节点名称" style={{ flex: "1 1 120px", padding: "6px 8px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }} />
                        <input type="text" value={sshHost} onChange={(e) => setSshHost(e.currentTarget.value)} placeholder="SSH 主机 IP（如 45.x.x.x）" aria-label="部署节点 SSH 主机" style={{ flex: "1 1 150px", padding: "6px 8px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }} />
                        <input type="text" value={sshPort} onChange={(e) => setSshPort(e.currentTarget.value)} placeholder="端口 22" aria-label="部署节点 SSH 端口" style={{ width: "68px", padding: "6px 8px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }} />
                        <input type="text" value={sshUser} onChange={(e) => setSshUser(e.currentTarget.value)} placeholder="root" aria-label="部署节点 SSH 用户" style={{ width: "80px", padding: "6px 8px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }} />
                        <input type="text" value={sshKeyPath} onChange={(e) => setSshKeyPath(e.currentTarget.value)} placeholder="私钥路径（留空用 ~/.ssh）" aria-label="部署节点 SSH 私钥" style={{ flex: "1 1 150px", padding: "6px 8px", borderRadius: "6px", border: "1px solid rgba(255,255,255,0.16)", background: "rgba(0,0,0,0.3)", color: "inherit", fontSize: "12px" }} />
                        <select className={styles.select} aria-label="部署模式" value={sshMode} onChange={(e) => setSshMode(e.target.value as "udp" | "docker")}>
                          <option value="udp">UDP 直连 (systemd 21118)</option>
                          <option value="docker">Docker 容器化部署</option>
                        </select>
                        <button type="button" className={`${styles.btn} ${styles.btnPrimary}`} disabled={sshBusy} onClick={() => void handleLabSshDeploy()}>
                          {sshBusy ? "正在上传并启动…" : "上传启动并测速"}
                        </button>
                      </div>
                    )}
                  </div>
                );
              })()}
            </div>
          )}
        </div>

        {/* 2) 3 节点真实链路拓扑图（展示每个节点的实测延迟） */}
        <div
          aria-label="控制端到被控端网络拓扑图"
          style={{
            display: "grid",
            gridTemplateColumns: "1fr auto 1.2fr auto 1fr",
            alignItems: "center",
            gap: "8px",
            padding: "10px",
            borderRadius: "8px",
            background: "rgba(0, 0, 0, 0.24)",
          }}
        >
          <div
            style={{
              padding: "8px 10px",
              borderRadius: "8px",
              border: "1px solid rgba(88, 166, 255, 0.45)",
              background: "rgba(88, 166, 255, 0.08)",
            }}
          >
            <div style={{ fontSize: "12px", fontWeight: 600 }}>💻 控制端（本机）</div>
            <div style={{ fontSize: "11px", opacity: 0.78, marginTop: "2px" }}>
              真实多轮探测（3 轮）
            </div>
          </div>

          <div style={{ textAlign: "center", fontSize: "11px", opacity: 0.85 }}>
            <div>━━►</div>
            <div>{testingPlanId ? "实测中…" : "QUIC/TCP"}</div>
          </div>

          <div
            style={{
              padding: "8px 10px",
              borderRadius: "8px",
              border: bestRealPlanId
                ? "1px solid rgba(63, 185, 80, 0.55)"
                : "1px solid rgba(255, 255, 255, 0.15)",
              background: bestRealPlanId
                ? "rgba(63, 185, 80, 0.09)"
                : "rgba(255, 255, 255, 0.03)",
              textAlign: "center",
            }}
          >
            <div style={{ fontSize: "12px", fontWeight: 600 }}>
              🌐 {bestRealPlanId ? `最优：${realResults[bestRealPlanId]?.planName}` : `已绑定 ${activePlans.length} 个方案`}
            </div>
            <div style={{ fontSize: "11px", opacity: 0.85, marginTop: "2px" }}>
              {bestRealPlanId
                ? `实测均延迟 ${realResults[bestRealPlanId]?.avgRttMs}ms · 抖动 ${realResults[bestRealPlanId]?.jitterMs ?? 0}ms · 丢包 ${realResults[bestRealPlanId]?.lossPct}%`
                : "点击「一键自动实测全部方案」获取真实网络延迟与丢包"}
            </div>
          </div>

          <div style={{ textAlign: "center", fontSize: "11px", opacity: 0.85 }}>
            <div>━━►</div>
            <div>真实链路</div>
          </div>

          <div
            style={{
              padding: "8px 10px",
              borderRadius: "8px",
              border: "1px solid rgba(188, 140, 255, 0.45)",
              background: "rgba(188, 140, 255, 0.08)",
            }}
          >
            <div style={{ fontSize: "12px", fontWeight: 600 }}>🖥️ 被控端目标</div>
            <div style={{ fontSize: "11px", opacity: 0.85, marginTop: "2px" }}>
              {effectiveTargetLabel}
            </div>
          </div>
        </div>

        {/* 3) 当前目标绑定的多个方案卡片列表 */}
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))",
            gap: "10px",
          }}
        >
          {activePlans.map((plan) => {
            const res = realResults[plan.planId];
            const isBest = bestRealPlanId === plan.planId;
            const isConfirmed = confirmedPlanId === plan.planId;
            const isRunning = testingPlanId === plan.planId;
            return (
              <div
                key={plan.planId}
                className={styles.planCard}
                style={{
                  padding: "12px",
                  borderRadius: "10px",
                  border: isConfirmed
                    ? "1px solid rgba(88, 166, 255, 0.75)"
                    : isBest
                      ? "1px solid rgba(63, 185, 80, 0.65)"
                      : isRunning
                        ? "1px solid rgba(88, 166, 255, 0.7)"
                        : "1px solid rgba(255, 255, 255, 0.1)",
                  background: isConfirmed
                    ? "rgba(88, 166, 255, 0.08)"
                    : isBest
                      ? "rgba(63, 185, 80, 0.07)"
                      : "rgba(0, 0, 0, 0.22)",
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                }}
              >
                <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: "6px" }}>
                  <strong style={{ fontSize: "13px" }}>
                    {isBest ? "🏆 " : ""}
                    {plan.planName}
                  </strong>
                  <div style={{ display: "flex", gap: "4px", alignItems: "center" }}>
                    {isConfirmed && (
                      <span
                        style={{
                          fontSize: "11px",
                          padding: "2px 6px",
                          borderRadius: "999px",
                          background: "rgba(63, 185, 80, 0.2)",
                          color: "#3fb950",
                          fontWeight: 600,
                        }}
                      >
                        ★ 当前生效
                      </span>
                    )}
                    <span
                      style={{
                        fontSize: "11px",
                        padding: "2px 6px",
                        borderRadius: "999px",
                        background: "rgba(255, 255, 255, 0.08)",
                      }}
                    >
                      {plan.mode.toUpperCase()}
                    </span>
                  </div>
                </div>
                <div style={{ fontSize: "12px", opacity: 0.78, fontFamily: "monospace" }}>
                  {plan.mode === "relay" || plan.mode === "punch"
                    ? `节点: ${plan.relayAddr || "未配置"}`
                    : `地址: ${plan.targetAddr}`}
                </div>
                <div style={{ fontSize: "12px", minHeight: "34px" }}>
                  {isRunning ? (
                    <span style={{ color: "#58a6ff" }}>⏳ 正在真实探测（3 轮）…</span>
                  ) : res ? (
                    res.ok ? (
                      <span style={{ color: "#3fb950" }}>
                        ✓ 可达 {res.reachableCount}/{res.totalProbes} ·{" "}
                        {res.nodeRttMs !== null ? `节点延迟 ${res.nodeRttMs}ms` : `均延迟 ${res.avgRttMs}ms`}
                        {res.targetRttMs !== null && res.nodeRttMs !== null
                          ? ` · 目标直连 ${res.targetRttMs}ms`
                          : ""}
                        （抖动 {res.jitterMs ?? 0}ms / 丢包 {res.lossPct}%）
                      </span>
                    ) : (
                      <span style={{ color: "#f85149" }}>✗ {res.routeDetail}</span>
                    )
                  ) : (
                    <span style={{ opacity: 0.55 }}>尚未实测（鼠标悬停卡片可单测 / 切换）</span>
                  )}
                </div>
                <div
                  className={`${styles.planCardActions} ${isRunning ? styles.planCardActionsVisible : ""}`}
                >
                  <button
                    type="button"
                    className={styles.planIconBtn}
                    data-tip={isRunning ? "实测中…" : "单测"}
                    aria-label={isRunning ? `正在实测 ${plan.planName}` : `单测 ${plan.planName}`}
                    disabled={runningRealBatch || testingPlanId !== null}
                    onClick={() => void runSinglePlanReal(plan)}
                  >
                    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                      <polygon points="6 4 20 12 6 20 6 4" />
                    </svg>
                  </button>
                  <button
                    type="button"
                    className={`${styles.planIconBtn} ${isConfirmed ? styles.planIconBtnActive : ""}`}
                    data-tip={
                      applyingPlanId === plan.planId
                        ? "切换中…"
                        : isConfirmed
                          ? "使用中"
                          : "切换使用"
                    }
                    aria-label={isConfirmed ? `${plan.planName} 使用中` : `切换使用 ${plan.planName}`}
                    disabled={applyingPlanId === plan.planId}
                    onClick={() => void handleConfirmPlan(plan)}
                  >
                    {isConfirmed ? (
                      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                        <path d="M20 6 9 17l-5-5" />
                      </svg>
                    ) : (
                      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                        <path d="M8 3 4 7l4 4" />
                        <path d="M4 7h16" />
                        <path d="m16 21 4-4-4-4" />
                        <path d="M20 17H4" />
                      </svg>
                    )}
                  </button>
                  <button
                    type="button"
                    className={styles.planIconBtn}
                    data-tip="编辑"
                    aria-label={`编辑 ${plan.planName}`}
                    disabled={runningRealBatch || testingPlanId !== null}
                    onClick={() => handleStartEditPlan(plan)}
                  >
                    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                      <path d="M12 20h9" />
                      <path d="M16.5 3.5a2.121 2.121 0 1 1 3 3L7 19l-4 1 1-4Z" />
                    </svg>
                  </button>
                  {activePlans.length > 1 && (
                    <button
                      type="button"
                      className={`${styles.planIconBtn} ${styles.planIconBtnDanger}`}
                      data-tip="删除"
                      aria-label={`删除 ${plan.planName}`}
                      disabled={runningRealBatch || testingPlanId !== null}
                      onClick={() => handleDeleteTargetPlan(plan.planId)}
                    >
                      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                        <path d="M3 6h18" />
                        <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" />
                        <path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                      </svg>
                    </button>
                  )}
                </div>
              </div>
            );
          })}
        </div>

        {/* 4) 实时动态测试日志（固定高度内部滚动 + 可一键折叠，绝不拉长挤压页面） */}
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "6px",
          }}
        >
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: "8px" }}>
            <strong style={{ fontSize: "13px" }}>
              📋 实时动态测试日志（{eventLogs.length} 条 · 固定高度可滚动）
            </strong>
            <div style={{ display: "flex", gap: "6px" }}>
              <button
                type="button"
                className={styles.btn}
                aria-expanded={logsExpanded}
                onClick={() => setLogsExpanded((v) => !v)}
              >
                {logsExpanded ? "▲ 收起日志" : "▼ 展开日志"}
              </button>
              <button
                type="button"
                className={styles.btn}
                onClick={() => setEventLogs([])}
              >
                清空日志
              </button>
            </div>
          </div>
          {logsExpanded && (
            <div
              ref={logBoxRef}
              role="log"
              aria-live="polite"
              style={{
                height: "150px",
                maxHeight: "150px",
                overflowY: "auto",
                fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                fontSize: "12px",
                lineHeight: 1.55,
                background: "rgba(0, 0, 0, 0.32)",
                border: "1px solid rgba(255, 255, 255, 0.08)",
                borderRadius: "8px",
                padding: "8px 12px",
                display: "flex",
                flexDirection: "column",
                gap: "3px",
              }}
            >
              {eventLogs.length === 0 ? (
                <span style={{ opacity: 0.55 }}>暂无日志，点击「✨ 智能寻优（含候选）」或「▶ 测已绑定方案」开始真实探测。</span>
              ) : (
                eventLogs.map((entry) => (
                  <div
                    key={entry.id}
                    style={{
                      color:
                        entry.level === "ok"
                          ? "#3fb950"
                          : entry.level === "warn"
                            ? "#f85149"
                            : "inherit",
                    }}
                  >
                    <span style={{ opacity: 0.55 }}>[{entry.time}]</span> {entry.text}
                  </div>
                ))
              )}
            </div>
          )}
        </div>
      </div>

      {/* ── 高级底层调优与协议微操（默认折叠收起，不干扰日常真实测速） ── */}
      <div style={{ marginBottom: "10px" }}>
        <button
          type="button"
          className={styles.btn}
          aria-expanded={advancedOpen}
          onClick={() => setAdvancedOpen((v) => !v)}
          style={{ width: "100%", justifyContent: "space-between", padding: "10px 14px" }}
        >
          <span>
            ⚙️ 高级底层调优与协议微操（编码码控 / RS-FEC / Netem 弱网模拟 / 1:1 微距镜）
          </span>
          <span>{advancedOpen ? "▲ 收起高级区" : "▼ 展开高级调优"}</span>
        </button>
      </div>

      {advancedOpen && (
        <>
      {/* ── 高级编码/FEC 预设条（仅极客底层调优使用，与上方连接方案独立） ── */}
      <div
        style={{
          display: "flex",
          flexWrap: "wrap",
          gap: "8px",
          alignItems: "center",
          justifyContent: "space-between",
          padding: "10px 12px",
          marginBottom: "10px",
          borderRadius: "8px",
          background: "rgba(255, 255, 255, 0.03)",
          border: "1px solid rgba(255, 255, 255, 0.08)",
        }}
      >
        <span style={{ fontSize: "12px", opacity: 0.85 }}>
          🎛️ 底层编码/FEC 预设模板（仅控制码率/帧率/FEC 旋钮）：
        </span>
        <div className={styles.actionGroup}>
          <select
            className={styles.select}
            aria-label="加载底层编码预设模板"
            value={selectedProfileId}
            onChange={(e) => void handleSelectProfile(e.target.value)}
          >
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
          <button
            type="button"
            className={styles.btn}
            onClick={() => void handleLockProfile()}
          >
            保存为编码模板
          </button>
          <button
            type="button"
            className={`${styles.btn} ${styles.btnPanic}`}
            onClick={() => void handlePanicReset()}
          >
            ↺ Panic Reset
          </button>
        </div>
      </div>

      {/* ── 实时链路解剖拓扑条 (Live Pipeline Anatomy) ── */}
      <div className={styles.anatomyBar} role="status" aria-label="实时链路拓扑解剖">
        <div className={styles.stageBox}>
          <span className={styles.stageLabel}>1. CAPTURE &amp; ENCODE</span>
          <span className={styles.stageVal}>
            {live?.encoderName ?? "h264_nvenc"} ·{" "}
            {live?.is444 ? "YUV444" : "YUV420"} · {live?.fpsCap ?? codec.maxFps}fps
          </span>
        </div>
        <span className={styles.stageArrow}>──►</span>
        <div className={styles.stageBox}>
          <span className={styles.stageLabel}>2. LOSSLESS TILE REFINER</span>
          <span className={styles.stageVal}>
            {codec.tileEnabled
              ? `QOI ${live?.tileKbps ?? 0} kbps (${live?.tilesSent ?? 0} tiles · ${codec.tileStillMs}ms)`
              : "OFF (仅 H.264 基底)"}
          </span>
        </div>
        <span className={styles.stageArrow}>──►</span>
        <div className={styles.stageBox}>
          <span className={styles.stageLabel}>3. FRAMING &amp; FEC</span>
          <span className={styles.stageVal}>
            {transport.mediaFec
              ? `Datagram RS-FEC +${transport.fecOverheadPct}% (救回 ${live?.fecRecovered ?? 0} 帧)`
              : "QUIC Reliable Stream"}
          </span>
        </div>
        <span className={styles.stageArrow}>──►</span>
        <div
          className={`${styles.stageBox} ${
            netem.enabled ? styles.stageBoxAlert : ""
          }`}
        >
          <span className={styles.stageLabel}>4. NETEM VALVE</span>
          <span className={styles.stageVal}>
            {netem.enabled
              ? `+${netem.delayMs}ms / ${netem.lossPct}% (Burst ${netem.burstLen} · 丢 ${live?.netemDropped ?? 0})`
              : "BYPASS (0 开销直通)"}
          </span>
        </div>
        <span className={styles.stageArrow}>──►</span>
        <div className={styles.stageBox}>
          <span className={styles.stageLabel}>5. QUIC TRANSPORT</span>
          <span className={styles.stageVal}>
            {live?.routeTag ?? transport.routeMode} ·{" "}
            {transport.congestion.toUpperCase()} · RTT {live?.rttMs ?? 1}ms · Cwnd{" "}
            {live?.quicCwndKb ?? 256}KB
          </span>
        </div>
        <span className={styles.stageArrow}>──►</span>
        <div className={styles.stageBox}>
          <span className={styles.stageLabel}>6. WEBCODECS</span>
          <span className={styles.stageVal}>
            {decodeStats.decodeMs.toFixed(1)}ms · IDR #{live?.idrCount ?? 1}
          </span>
        </div>
      </div>

      {/* ── 主工作区：左侧四组真旋钮 + 右侧示波器/微距镜/A-B对决 ── */}
      <div className={styles.workspace}>
        <aside className={styles.knobsPane} aria-label="底层协议与编码操控旋钮">
          {/* 旋钮组 ①：选路与传输层强制接管 */}
          <div className={styles.knobCard}>
            <div className={styles.knobHeader}>
              <h2 className={styles.knobTitle}>① 选路与传输强制接管</h2>
              <span className={styles.knobTag}>ROUTE &amp; CC</span>
            </div>
            <p className={styles.oscSub} style={{ margin: "0 0 8px" }}>
              决定流量走局域网直连、打洞还是中继；BBR 适合跨网防卡顿，Cubic 适合极简内网。
            </p>

            <div className={styles.fieldRow}>
              <span className={styles.fieldLabel}>物理路径锁定</span>
              <span className={styles.fieldValue}>{transport.routeMode.toUpperCase()}</span>
            </div>
            <div className={styles.segGroup} role="group" aria-label="物理路径选择">
              {ROUTE_OPTIONS.map((r) => (
                <button
                  key={r.key}
                  type="button"
                  className={`${styles.segBtn} ${
                    transport.routeMode === r.key ? styles.segBtnActive : ""
                  }`}
                  onClick={() =>
                    void applyTransportNow({ ...transport, routeMode: r.key })
                  }
                >
                  {r.label}
                </button>
              ))}
            </div>

            <div className={styles.fieldRow}>
              <span className={styles.fieldLabel}>QUIC 拥塞控制算法</span>
              <span className={styles.fieldValue}>{transport.congestion.toUpperCase()}</span>
            </div>
            <div className={styles.segGroup} role="group" aria-label="拥塞算法选择">
              {(["bbr", "cubic"] as const).map((cc) => (
                <button
                  key={cc}
                  type="button"
                  className={`${styles.segBtn} ${
                    transport.congestion === cc ? styles.segBtnActive : ""
                  }`}
                  onClick={() =>
                    void applyTransportNow({ ...transport, congestion: cc })
                  }
                >
                  {cc === "bbr" ? "BBR (抗丢包带宽驱动)" : "Cubic (传统丢包退避)"}
                </button>
              ))}
            </div>
          </div>

          {/* 旋钮组 ②：抗丢包与 FEC 分片通道 */}
          <div className={styles.knobCard}>
            <div className={styles.knobHeader}>
              <h2 className={styles.knobTitle}>② 抗丢包与 RS-FEC 通道</h2>
              <span className={styles.knobTag}>MOONLIGHT FEC-RS</span>
            </div>
            <p className={styles.oscSub} style={{ margin: "0 0 8px" }}>
              跨网/WiFi 丢包时开启 RS-FEC 可瞬间救回丢帧避免卡死；冗余越高越抗丢包，但会多占相应带宽。
            </p>

            <div className={styles.segGroup} role="group" aria-label="视频承载通道">
              <button
                type="button"
                className={`${styles.segBtn} ${
                  !transport.mediaFec ? styles.segBtnActive : ""
                }`}
                onClick={() =>
                  void applyTransportNow({ ...transport, mediaFec: false })
                }
              >
                QUIC Stream (可靠流)
              </button>
              <button
                type="button"
                className={`${styles.segBtn} ${
                  transport.mediaFec ? styles.segBtnActive : ""
                }`}
                onClick={() =>
                  void applyTransportNow({ ...transport, mediaFec: true })
                }
              >
                Datagram + RS-FEC (零队头阻塞)
              </button>
            </div>

            <label className={styles.fieldRow}>
              <span className={styles.fieldLabel}>RS-FEC 冗余校验比例</span>
              <span className={styles.fieldValue}>+{transport.fecOverheadPct}%</span>
            </label>
            <input
              type="range"
              className={styles.slider}
              aria-label="RS-FEC 冗余校验比例"
              min={5}
              max={60}
              step={5}
              value={transport.fecOverheadPct}
              onChange={(e) => {
                const pct = Number(e.target.value);
                setTransport((t) => ({ ...t, fecOverheadPct: pct }));
                void labApplyTuning(
                  live?.activeDeviceId ?? deviceId ?? null,
                  codec,
                  pct,
                  false,
                );
              }}
            />
          </div>

          {/* 旋钮组 ③：编码器与无损瓦片微操 */}
          <div className={styles.knobCard}>
            <div className={styles.knobHeader}>
              <h2 className={styles.knobTitle}>③ 编码与 1:1 无损瓦片微操</h2>
              <span className={styles.knobTag}>CODEC &amp; QOI TILE</span>
            </div>
            <p className={styles.oscSub} style={{ margin: "0 0 8px" }}>
              局域网可拉高码率至 50~200 Mbps + 144fps；开启 QOI 瓦片会在画面静止时自动把文字补成 1:1 像素无损。
            </p>

            <div className={styles.segGroup} role="group" aria-label="码率控制模式">
              <button
                type="button"
                className={`${styles.segBtn} ${
                  !codec.manualLock ? styles.segBtnActive : ""
                }`}
                onClick={() => void applyCodecNow({ ...codec, manualLock: false })}
              >
                Baseline-Adaptive 自动
              </button>
              <button
                type="button"
                className={`${styles.segBtn} ${
                  codec.manualLock ? styles.segBtnActive : ""
                }`}
                onClick={() => void applyCodecNow({ ...codec, manualLock: true })}
              >
                Manual Lock 手动锁死
              </button>
            </div>

            <label className={styles.fieldRow}>
              <span className={styles.fieldLabel}>目标视频码率</span>
              <span className={styles.fieldValue}>
                {(codec.bitrateKbps / 1000).toFixed(1)} Mbps ({codec.bitrateKbps} kbps)
              </span>
            </label>
            <input
              type="range"
              className={styles.slider}
              aria-label="目标视频码率"
              min={500}
              max={200000}
              step={500}
              value={codec.bitrateKbps}
              onChange={(e) =>
                void applyCodecNow({
                  ...codec,
                  manualLock: true,
                  bitrateKbps: Number(e.target.value),
                })
              }
            />

            <label className={styles.fieldRow}>
              <span className={styles.fieldLabel}>帧率上限 (FPS Cap)</span>
              <span className={styles.fieldValue}>{codec.maxFps} fps</span>
            </label>
            <input
              type="range"
              className={styles.slider}
              aria-label="帧率上限"
              min={15}
              max={144}
              step={1}
              value={codec.maxFps}
              onChange={(e) =>
                void applyCodecNow({
                  ...codec,
                  manualLock: true,
                  maxFps: Number(e.target.value),
                })
              }
            />

            <div className={styles.segGroup} role="group" aria-label="编码器与瓦片开关">
              <button
                type="button"
                className={`${styles.segBtn} ${
                  codec.preferHw ? styles.segBtnActive : ""
                }`}
                onClick={() =>
                  void applyCodecNow({ ...codec, preferHw: !codec.preferHw }, true)
                }
              >
                {codec.preferHw ? "硬编优先 (NVENC/MFT)" : "强制 OpenH264 软编"}
              </button>
              <button
                type="button"
                className={`${styles.segBtn} ${
                  codec.request444 ? styles.segBtnActive : ""
                }`}
                onClick={() =>
                  void applyCodecNow({ ...codec, request444: !codec.request444 }, true)
                }
              >
                {codec.request444 ? "YUV 4:4:4" : "YUV 4:2:0"}
              </button>
              <button
                type="button"
                className={`${styles.segBtn} ${
                  codec.tileEnabled ? styles.segBtnActive : ""
                }`}
                onClick={() =>
                  void applyCodecNow({ ...codec, tileEnabled: !codec.tileEnabled }, true)
                }
              >
                {codec.tileEnabled ? "QOI 瓦片: ON" : "QOI 瓦片: OFF"}
              </button>
            </div>

            <label className={styles.fieldRow}>
              <span className={styles.fieldLabel}>瓦片静止触发阈值 / 预算比</span>
              <span className={styles.fieldValue}>
                {codec.tileStillMs} ms / {codec.tileBudgetPct}%
              </span>
            </label>
            <input
              type="range"
              className={styles.slider}
              aria-label="瓦片静止触发阈值"
              min={60}
              max={1000}
              step={20}
              value={codec.tileStillMs}
              onChange={(e) =>
                void applyCodecNow({ ...codec, tileStillMs: Number(e.target.value) })
              }
            />

            <button
              type="button"
              className={styles.btn}
              onClick={() => void applyCodecNow(codec, true)}
            >
              ⚡ 立即强制发送 IDR 关键帧 &amp; 重刷无损瓦片
            </button>
          </div>

          {/* 旋钮组 ④：可编程 Netem 弱网故障注入阀门 */}
          <div className={styles.knobCard}>
            <div className={styles.knobHeader}>
              <h2 className={styles.knobTitle}>④ 可编程 Netem 弱网阀门</h2>
              <span className={styles.knobTag}>
                {netem.enabled ? "ACTIVE SHAPING" : "BYPASS"}
              </span>
            </div>

            <div className={styles.presetRow}>
              {NETEM_PRESETS.map((p) => (
                <button
                  key={p.name}
                  type="button"
                  className={styles.presetChip}
                  onClick={() => void applyNetemNow(p.cfg)}
                >
                  {p.name}
                </button>
              ))}
            </div>

            <label className={styles.fieldRow}>
              <span className={styles.fieldLabel}>注入单向延迟 / 抖动</span>
              <span className={styles.fieldValue}>
                +{netem.delayMs} ms (±{netem.jitterMs} ms)
              </span>
            </label>
            <input
              type="range"
              className={styles.slider}
              aria-label="注入单向延迟"
              min={0}
              max={400}
              step={10}
              value={netem.delayMs}
              onChange={(e) => {
                const d = Number(e.target.value);
                void applyNetemNow({
                  ...netem,
                  enabled: d > 0 || netem.lossPct > 0 || netem.rateKbps > 0,
                  delayMs: d,
                  jitterMs: Math.round(d * 0.2),
                });
              }}
            />

            <label className={styles.fieldRow}>
              <span className={styles.fieldLabel}>随机丢包率 (Loss %)</span>
              <span className={styles.fieldValue}>{netem.lossPct.toFixed(1)}%</span>
            </label>
            <input
              type="range"
              className={styles.slider}
              aria-label="随机丢包率"
              min={0}
              max={25}
              step={0.5}
              value={netem.lossPct}
              onChange={(e) => {
                const loss = Number(e.target.value);
                void applyNetemNow({
                  ...netem,
                  enabled: loss > 0 || netem.delayMs > 0 || netem.rateKbps > 0,
                  lossPct: loss,
                });
              }}
            />

            <label className={styles.fieldRow}>
              <span className={styles.fieldLabel}>连续突发丢包长 (Burst)</span>
              <span className={styles.fieldValue}>{netem.burstLen} 包</span>
            </label>
            <input
              type="range"
              className={styles.slider}
              aria-label="连续突发丢包长"
              min={1}
              max={10}
              step={1}
              value={netem.burstLen}
              onChange={(e) =>
                void applyNetemNow({
                  ...netem,
                  enabled: true,
                  burstLen: Number(e.target.value),
                })
              }
            />
          </div>
        </aside>

        {/* ── 右侧遥测示波器 + 1:1 微距像素检视镜 + A/B 矩阵对决 ── */}
        <div className={styles.telemetryPane}>
          {toast && (
            <div className={styles.toastBar} role="status">
              {toast}
            </div>
          )}

          {/* 60Hz 四通道极客示波器 */}
          <div className={styles.oscGrid}>
            <div className={styles.oscCard}>
              <div className={styles.oscHead}>
                <span className={styles.oscName}>QUIC RTT &amp; 拥塞窗口</span>
                <span className={styles.oscBig}>{live?.rttMs ?? 1} ms</span>
              </div>
              <span className={styles.oscSub}>
                Cwnd: {live?.quicCwndKb ?? 256} KB · 累计发包 {live?.quicSentPkts ?? 0}
              </span>
              <OscilloscopeLine data={rttHist} color="#58a6ff" />
            </div>

            <div className={styles.oscCard}>
              <div className={styles.oscHead}>
                <span className={styles.oscName}>视频流 vs QOI 瓦片码率</span>
                <span className={styles.oscBig}>
                  {((live?.videoKbps ?? 0) / 1000).toFixed(2)} Mbps
                </span>
              </div>
              <span className={styles.oscSub}>
                H.264: {live?.videoKbps ?? 0} kbps (绿) · QOI 瓦片:{" "}
                {live?.tileKbps ?? 0} kbps (紫虚线)
              </span>
              <OscilloscopeLine
                data={videoKbpsHist}
                color="#46d17e"
                secondary={tileKbpsHist}
                secColor="#bc8cff"
              />
            </div>

            <div className={styles.oscCard}>
              <div className={styles.oscHead}>
                <span className={styles.oscName}>帧率与 WebCodecs 解码耗时</span>
                <span className={styles.oscBig}>
                  {decodeStats.fps || live?.fpsCap || 60} fps
                </span>
              </div>
              <span className={styles.oscSub}>
                解码: {decodeStats.decodeMs.toFixed(1)} ms · 累计无损瓦片:{" "}
                {live?.tilesSent ?? 0}
              </span>
              <OscilloscopeLine data={fpsHist} color="#e5b567" />
            </div>

            <div className={styles.oscCard}>
              <div className={styles.oscHead}>
                <span className={styles.oscName}>RS-FEC 救回帧 vs 丢包统计</span>
                <span className={styles.oscBig}>
                  救回 {live?.fecRecovered ?? 0} 帧
                </span>
              </div>
              <span className={styles.oscSub}>
                Netem 已丢弃: {live?.netemDropped ?? 0} 包 · 彻底丢帧:{" "}
                {live?.fecLost ?? 0}
              </span>
              <OscilloscopeLine data={fecRecHist} color="#bc8cff" />
            </div>
          </div>

          {/* 1:1 微距像素检视镜 (400% Pixel Magnifier) */}
          <div className={styles.magCard}>
            <div className={styles.magHeader}>
              <div>
                <h2 className={styles.knobTitle}>
                  🔍 1:1 微距像素检视镜 (400% Pixel Magnifier)
                </h2>
                <span className={styles.oscSub}>
                  移动鼠标悬停标尺画面（或框选右侧 9pt 代码字模区），亲眼对比 H.264 4:2:0 色度涂抹 vs QOI 无损瓦片像素级还原
                </span>
              </div>
              <div className={styles.magMetrics}>
                <span>
                  实时 PSNR:{" "}
                  <strong className={styles.metricGood}>
                    {(live?.sandboxPsnrDb ?? 46.8).toFixed(1)} dB
                  </strong>
                </span>
                <span>
                  文字边缘保真率:{" "}
                  <strong
                    className={
                      codec.tileEnabled ? styles.metricGood : styles.metricWarn
                    }
                  >
                    {(live?.sandboxTextFidelityPct ?? 99.9).toFixed(1)}%
                  </strong>
                </span>
              </div>
            </div>

            <div className={styles.magSplit}>
              <div
                className={styles.sandboxStage}
                onMouseMove={onStageMouseMove}
              >
                <div className={styles.sandboxCanvasWrap}>
                  <canvas ref={videoCanvasRef} className={styles.layerCanvas} />
                  <canvas ref={tileCanvasRef} className={styles.layerCanvas} />
                </div>
              </div>

              <div className={styles.zoomBox}>
                <canvas ref={zoomCanvasRef} className={styles.zoomCanvas} />
                <span className={styles.zoomCaption}>
                  400% 最近邻像素直出
                  <br />
                  {codec.tileEnabled
                    ? "🟢 QOI 无损瓦片覆盖中"
                    : "🟡 仅 H.264 基底 (瓦片已关)"}
                </span>
              </div>
            </div>
          </div>

          {/* ⚔️ 真协议栈 A/B 矩阵对决台 */}
          <div className={styles.abCard}>
            <div className={styles.magHeader}>
              <div>
                <h2 className={styles.knobTitle}>⚔️ 真协议栈 A/B 矩阵对决台</h2>
                <span className={styles.oscSub}>
                  在当前 Netem 弱网阀门参数下，用真编码器 + 真分片通道连续压测两套 Profile 并输出硬核指标裁决
                </span>
              </div>
              <div className={styles.abControls}>
                <select
                  className={styles.select}
                  aria-label="对决方案 A"
                  value={abIdA}
                  onChange={(e) => setAbIdA(e.target.value)}
                >
                  {profiles.map((p) => (
                    <option key={p.id} value={p.id}>
                      A: {p.name}
                    </option>
                  ))}
                </select>
                <span>VS</span>
                <select
                  className={styles.select}
                  aria-label="对决方案 B"
                  value={abIdB}
                  onChange={(e) => setAbIdB(e.target.value)}
                >
                  {profiles.map((p) => (
                    <option key={p.id} value={p.id}>
                      B: {p.name}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className={`${styles.btn} ${styles.btnPrimary}`}
                  disabled={abRunning}
                  onClick={() => void handleRunAb()}
                >
                  {abRunning ? "⏳ 真栈压测对决中…" : "▶ 启动 A/B 真栈对决"}
                </button>
                <button
                  type="button"
                  className={styles.btn}
                  disabled={allBenchRunning}
                  onClick={() => void handleBenchmarkAll()}
                >
                  {allBenchRunning ? "⏳ 全方案测速中…" : "🚀 全方案一键选最优"}
                </button>
              </div>
            </div>

            {allBenchResult && (
              <div style={{ marginTop: "10px" }}>
                <div className={styles.abVerdict}>
                  <strong>🎯 最优推荐：{allBenchResult.bestName}</strong> ·{" "}
                  {allBenchResult.verdict}
                  <button
                    type="button"
                    className={`${styles.btn} ${styles.btnPrimary}`}
                    style={{ marginLeft: "10px" }}
                    onClick={() => void handleSelectProfile(allBenchResult.bestProfileId)}
                  >
                    ✨ 一键应用最优方案
                  </button>
                </div>
                <table className={styles.abTable}>
                  <thead>
                    <tr>
                      <th>排名</th>
                      <th>技术方案</th>
                      <th>路由模式</th>
                      <th>有效 FPS</th>
                      <th>文字保真</th>
                      <th>P99 延迟</th>
                      <th>综合分</th>
                    </tr>
                  </thead>
                  <tbody>
                    {allBenchResult.ranked.map((r, idx) => (
                      <tr key={r.profileId}>
                        <td>#{idx + 1}</td>
                        <td>{r.metrics.label}</td>
                        <td>{r.routeMode.toUpperCase()}</td>
                        <td>{r.metrics.avgFps.toFixed(1)} fps</td>
                        <td>{r.metrics.textFidelityPct.toFixed(1)}%</td>
                        <td>{r.metrics.p99FrameMs.toFixed(1)} ms</td>
                        <td>{r.score.toFixed(1)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}

            {abResult && (
              <>
                <div className={styles.abVerdict}>
                  <strong>🏆 胜出：{abResult.winner}</strong> · {abResult.verdict}
                  <div className={styles.oscSub}>工况：{abResult.netemSummary}</div>
                </div>
                <table className={styles.abTable}>
                  <thead>
                    <tr>
                      <th>硬核指标</th>
                      <th>{abResult.runA.label}</th>
                      <th>{abResult.runB.label}</th>
                    </tr>
                  </thead>
                  <tbody>
                    <tr>
                      <td>实际编码器</td>
                      <td>{abResult.runA.encoderName}</td>
                      <td>{abResult.runB.encoderName}</td>
                    </tr>
                    <tr>
                      <td>文字边缘保真率 (%)</td>
                      <td>{abResult.runA.textFidelityPct.toFixed(1)}%</td>
                      <td>{abResult.runB.textFidelityPct.toFixed(1)}%</td>
                    </tr>
                    <tr>
                      <td>客观清晰度 PSNR (dB)</td>
                      <td>{abResult.runA.psnrDb.toFixed(1)} dB</td>
                      <td>{abResult.runB.psnrDb.toFixed(1)} dB</td>
                    </tr>
                    <tr>
                      <td>P99 帧传输延迟 (ms)</td>
                      <td>{abResult.runA.p99FrameMs.toFixed(1)} ms</td>
                      <td>{abResult.runB.p99FrameMs.toFixed(1)} ms</td>
                    </tr>
                    <tr>
                      <td>有效交付帧率 (FPS)</td>
                      <td>{abResult.runA.avgFps.toFixed(1)} fps</td>
                      <td>{abResult.runB.avgFps.toFixed(1)} fps</td>
                    </tr>
                    <tr>
                      <td>RS-FEC 救回帧 / 丢帧</td>
                      <td>
                        救 {abResult.runA.fecRecovered} / 丢{" "}
                        {abResult.runA.framesDropped}
                      </td>
                      <td>
                        救 {abResult.runB.fecRecovered} / 丢{" "}
                        {abResult.runB.framesDropped}
                      </td>
                    </tr>
                    <tr>
                      <td>视频 / 瓦片码率 (kbps)</td>
                      <td>
                        {abResult.runA.videoKbps} / {abResult.runA.tileKbps} kbps
                      </td>
                      <td>
                        {abResult.runB.videoKbps} / {abResult.runB.tileKbps} kbps
                      </td>
                    </tr>
                  </tbody>
                </table>
              </>
            )}
          </div>
        </div>
      </div>
        </>
      )}
    </section>
  );
}
