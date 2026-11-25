/**
 * RD Geek Tuning Deck · 画质与传输底层调优台。
 *
 * 独立承载真协议栈遥测、4K@144Hz/200Mbps 编解码与 QOI 无损瓦片旋钮、
 * 传输选路/BBR/RS-FEC、Netem 可编程弱网阀门、60Hz 示波器、
 * 1:1 400% 微距像素检视镜、A/B 矩阵对决与 Panic Reset 逃生舱。
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
  labGetState,
  labListProfiles,
  labPanicReset,
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
  type NetemConfigDto,
} from "../../api/lab";
import { isTauriRuntime } from "../../api/bridge";
import {
  CanvasTileSurface,
  FRAME_INVALIDATE,
  FRAME_TILE,
  FRAME_VIDEO,
  parseTileHeader,
  TILE_HEADER_LEN,
  TileLayer,
  tilePayloadLen,
} from "../../session/TileLayer";
import {
  H264Decoder,
  parseVideoHeader,
  VIDEO_HEADER_LEN,
  type DecodeStats,
} from "../../video/decoder";
import styles from "../Lab.module.css";

interface GeekTuningDeckProps {
  deviceId: string | null;
  drawer?: boolean;
  onToast: (msg: string) => void;
}

const DEFAULT_TRANSPORT: LabTransportConfig = {
  routeMode: "auto",
  congestion: "bbr",
  mediaFec: false,
  fecOverheadPct: 20,
};

const DEFAULT_CODEC: LabCodecConfig = {
  manualLock: false,
  bitrateKbps: 16000,
  maxFps: 144,
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

const NETEM_PRESETS: { name: string; cfg: NetemConfigDto }[] = [
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
  const toPts = (arr: number[]) =>
    arr
      .map(
        (v, i) =>
          `${((i / Math.max(arr.length - 1, 1)) * w).toFixed(1)},${(
            h -
            4 -
            (v / max) * (h - 10)
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
      <polyline points={toPts(data)} fill="none" stroke={color} strokeWidth="1.8" />
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

  ctx.font = '12px "Cascadia Mono", Consolas, monospace';
  const sharp = tileEnabled || kbps >= 15000;
  ctx.fillStyle = sharp ? "#eef4ff" : "#b8c0cc";
  const lines = [
    "fn quic_bbr_pump(conn: &Conn, fec: &FecRs) {",
    "  // 4K@144Hz + Lossless QOI Tile Refiner",
    "  let cwnd = conn.stats().path.cwnd;",
    "  assert!(text_clarity == 1.000 && rtt < 25);",
    "  tile_refiner.tick(raw, budget_bytes);",
    "}",
  ];
  lines.forEach((line, idx) => {
    ctx.fillText(line, 320, 28 + idx * 20);
  });

  ctx.strokeStyle = tileEnabled ? "#46d17e" : "#e5b567";
  ctx.lineWidth = 1;
  ctx.strokeRect(314, 10, 316, 130);
  ctx.fillStyle = tileEnabled ? "#46d17e" : "#e5b567";
  ctx.fillText(
    tileEnabled
      ? "QOI LOSSLESS TILE REFINED (100% PIXEL EXACT)"
      : "H.264 4:2:0 BASE ONLY (CHROMA SUBSAMPLED)",
    320,
    155,
  );

  const bx = (tick * 10) % (w - 40);
  ctx.fillStyle = "#1c222b";
  ctx.fillRect(10, 260, w - 20, 60);
  ctx.fillStyle = "#46d17e";
  ctx.fillRect(10 + bx, 264, 28, 52);
}

export function GeekTuningDeck({ deviceId, drawer = false, onToast }: GeekTuningDeckProps) {
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
  const [allBenchRunning, setAllBenchRunning] = useState(false);
  const [allBenchResult, setAllBenchResult] = useState<LabAllRoutesBenchDto | null>(null);
  const [decodeStats, setDecodeStats] = useState<DecodeStats>({
    fps: 60,
    decodeMs: 1.1,
    width: 640,
    height: 360,
  });

  const [rttHist, setRttHist] = useState<number[]>([]);
  const [videoKbpsHist, setVideoKbpsHist] = useState<number[]>([]);
  const [tileKbpsHist, setTileKbpsHist] = useState<number[]>([]);
  const [fpsHist, setFpsHist] = useState<number[]>([]);

  const videoCanvasRef = useRef<HTMLCanvasElement>(null);
  const tileCanvasRef = useRef<HTMLCanvasElement>(null);
  const zoomCanvasRef = useRef<HTMLCanvasElement>(null);
  const magPosRef = useRef<{ x: number; y: number }>({ x: 360, y: 42 });
  const hasRealFramesRef = useRef(false);

  useEffect(() => {
    void labListProfiles()
      .then((list) => {
        if (!Array.isArray(list)) return;
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

  useEffect(() => {
    let active = true;
    const poll = () => {
      void labGetState(deviceId)
        .then((st) => {
          if (!active || !st) return;
          setLive(st);
          setRttHist((h) => [...h.slice(-59), st.rttMs]);
          setVideoKbpsHist((h) => [...h.slice(-59), st.videoKbps]);
          setTileKbpsHist((h) => [...h.slice(-59), st.tileKbps]);
          setFpsHist((h) => [...h.slice(-59), st.fpsCap]);
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
      zctx.drawImage(srcVideo, sx, sy, 48, 48, 0, 0, 192, 192);
      if (tileEl && srcVideo === vidEl && tileEl.width > 0) {
        zctx.drawImage(tileEl, sx, sy, 48, 48, 0, 0, 192, 192);
      }
    } catch {
      // ignore resize race
    }
    zctx.strokeStyle = "rgba(70, 209, 126, 0.55)";
    zctx.lineWidth = 1;
    zctx.strokeRect(88, 88, 16, 16);
  }, [drawer]);

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

    let tick = 0;
    const anim = window.setInterval(() => {
      tick += 1;
      if (!hasRealFramesRef.current || !isTauriRuntime()) {
        drawSyntheticRulerOnCanvas(videoEl, tick, codec.tileEnabled, codec.bitrateKbps);
      }
      refreshMagnifier();
    }, 80);

    return () => {
      window.clearInterval(anim);
      decoder?.close();
      void labStopSandbox();
    };
  }, [codec.tileEnabled, codec.bitrateKbps, refreshMagnifier]);

  const applyTransport = useCallback(
    async (next: LabTransportConfig) => {
      setTransport(next);
      try {
        const msg = await labApplyTransport(deviceId, next);
        onToast(msg || "传输策略已生效");
      } catch (e) {
        onToast(`传输策略切换失败：${e instanceof Error ? e.message : String(e)}`);
      }
    },
    [deviceId, onToast],
  );

  const applyCodec = useCallback(
    async (next: LabCodecConfig, forceIdr = false) => {
      setCodec(next);
      try {
        await labApplyTuning(deviceId, next, transport.fecOverheadPct, forceIdr);
        if (forceIdr) onToast("已强制发送 IDR 关键帧并刷新无损瓦片");
      } catch (e) {
        onToast(`编码热调失败：${e instanceof Error ? e.message : String(e)}`);
      }
    },
    [deviceId, transport.fecOverheadPct, onToast],
  );

  const applyNetem = useCallback(
    async (next: NetemConfigDto) => {
      setNetem(next);
      try {
        await labSetNetem(next);
        onToast(
          next.enabled
            ? `弱网模拟开启：+${next.delayMs}ms · 丢包 ${next.lossPct}%`
            : "弱网模拟已关闭",
        );
      } catch (e) {
        onToast(`Netem 设置失败：${e instanceof Error ? e.message : String(e)}`);
      }
    },
    [onToast],
  );

  const handlePanicReset = useCallback(async () => {
    try {
      await labPanicReset(deviceId);
      setTransport(DEFAULT_TRANSPORT);
      setCodec(DEFAULT_CODEC);
      setNetem(DEFAULT_NETEM);
      onToast("已执行 Panic Reset：所有旋钮与弱网阀门恢复默认安全态");
    } catch (e) {
      onToast(`Panic Reset 失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }, [deviceId, onToast]);

  const handleApplyProfile = useCallback(
    async (id: string) => {
      setSelectedProfileId(id);
      const prof = profiles.find((p) => p.id === id);
      if (!prof) return;
      await applyTransport(prof.transport);
      await applyCodec(prof.codec, true);
      onToast(`已载入极客预设：${prof.name}`);
    },
    [applyCodec, applyTransport, onToast, profiles],
  );

  const handleSaveCurrentProfile = useCallback(async () => {
    const id = `custom-${Date.now().toString(36)}`;
    const prof: LabProfile = {
      id,
      name: `自定义 ${codec.maxFps}Hz · ${Math.round(codec.bitrateKbps / 1000)}M`,
      boundDeviceId: deviceId,
      transport,
      codec,
    };
    try {
      const next = await labSaveProfile(prof);
      if (Array.isArray(next)) setProfiles(next);
      setSelectedProfileId(id);
      onToast(`已固化预设「${prof.name}」`);
    } catch (e) {
      onToast(`保存预设失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }, [codec, deviceId, onToast, transport]);

  const handleRunAb = useCallback(async () => {
    const pA = profiles.find((p) => p.id === abIdA) ?? {
      id: "a",
      name: "方案 A",
      boundDeviceId: null,
      transport: DEFAULT_TRANSPORT,
      codec: DEFAULT_CODEC,
    };
    const pB = profiles.find((p) => p.id === abIdB) ?? {
      id: "b",
      name: "方案 B",
      boundDeviceId: null,
      transport: { ...DEFAULT_TRANSPORT, mediaFec: true },
      codec,
    };
    setAbRunning(true);
    try {
      const res = await labRunAbBench(pA, pB);
      setAbResult(res);
      onToast(`A/B 对决完成：${res.winner}`);
    } catch (e) {
      onToast(`A/B 对决失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setAbRunning(false);
    }
  }, [abIdA, abIdB, codec, onToast, profiles]);

  const handleBenchmarkAll = useCallback(async () => {
    if (profiles.length === 0) return;
    setAllBenchRunning(true);
    try {
      const res = await labBenchmarkAllRoutes(profiles);
      setAllBenchResult(res);
      onToast(`全预设压测完成：最优 ${res.bestName}`);
    } catch (e) {
      onToast(`全预设压测失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setAllBenchRunning(false);
    }
  }, [onToast, profiles]);

  const onCanvasMove = useCallback(
    (e: MouseEvent<HTMLCanvasElement>) => {
      const el = videoCanvasRef.current;
      if (!el) return;
      const rect = el.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return;
      magPosRef.current = {
        x: Math.round(((e.clientX - rect.left) / rect.width) * el.width),
        y: Math.round(((e.clientY - rect.top) / rect.height) * el.height),
      };
      refreshMagnifier();
    },
    [refreshMagnifier],
  );

  return (
    <div className={styles.tuningRoot}>
      {/* 顶部预设与 Panic Reset 栏 */}
      <div className={styles.tuningHeaderBar}>
        <div className={styles.tuningHeaderLeft}>
          <select
            className={styles.select}
            aria-label="选择编码调优预设"
            value={selectedProfileId}
            onChange={(e) => void handleApplyProfile(e.target.value)}
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
            onClick={() => void handleSaveCurrentProfile()}
          >
            保存当前参数为预设
          </button>
          <button
            type="button"
            className={styles.btn}
            disabled={allBenchRunning || profiles.length === 0}
            onClick={() => void handleBenchmarkAll()}
          >
            {allBenchRunning ? "压测中…" : "全预设自动评分"}
          </button>
        </div>
        <button
          type="button"
          className={`${styles.btn} ${styles.btnPanic}`}
          onClick={() => void handlePanicReset()}
          title="一键恢复默认安全参数并关闭弱网模拟"
        >
          Panic Reset
        </button>
      </div>

      {/* 实时链路解剖条 */}
      <div className={styles.telemetryStrip}>
        <div className={styles.metricCell}>
          <span className={styles.metricLabel}>当前通道</span>
          <strong className={styles.metricValue}>{live?.routeTag ?? "sandbox"}</strong>
        </div>
        <div className={styles.metricCell}>
          <span className={styles.metricLabel}>编码器</span>
          <strong className={styles.metricValue}>{live?.encoderName ?? "auto"}</strong>
        </div>
        <div className={styles.metricCell}>
          <span className={styles.metricLabel}>RTT / 拥塞</span>
          <strong className={styles.metricValue}>
            {live ? `${live.rttMs}ms · ${live.congestion.toUpperCase()}` : "—"}
          </strong>
        </div>
        <div className={styles.metricCell}>
          <span className={styles.metricLabel}>视频 / 瓦片码率</span>
          <strong className={styles.metricValue}>
            {live ? `${live.videoKbps} / ${live.tileKbps} kbps` : "—"}
          </strong>
        </div>
        <div className={styles.metricCell}>
          <span className={styles.metricLabel}>RS-FEC 救回</span>
          <strong className={styles.metricValue}>
            {live ? `${live.fecRecovered} 帧` : "0"}
          </strong>
        </div>
        <div className={styles.metricCell}>
          <span className={styles.metricLabel}>文字保真 / PSNR</span>
          <strong className={styles.metricValue}>
            {live
              ? `${live.sandboxTextFidelityPct.toFixed(1)}% · ${live.sandboxPsnrDb.toFixed(1)}dB`
              : "—"}
          </strong>
        </div>
      </div>

      {/* 四组真协议栈旋钮 */}
      <div className={styles.knobGrid}>
        {/* ① 选路与传输 */}
        <div className={styles.knobCard}>
          <h2 className={styles.knobTitle}>① 选路与传输强制接管</h2>
          <div className={styles.pillRow}>
            {ROUTE_OPTIONS.map((opt) => (
              <button
                key={opt.key}
                type="button"
                className={
                  transport.routeMode === opt.key
                    ? `${styles.btn} ${styles.btnPrimary}`
                    : styles.btn
                }
                onClick={() => void applyTransport({ ...transport, routeMode: opt.key })}
              >
                {opt.label}
              </button>
            ))}
          </div>
          <div className={styles.knobRow}>
            <span>拥塞控制算法</span>
            <div className={styles.pillRow}>
              {(["bbr", "cubic"] as const).map((cc) => (
                <button
                  key={cc}
                  type="button"
                  className={
                    transport.congestion === cc
                      ? `${styles.btn} ${styles.btnPrimary}`
                      : styles.btn
                  }
                  onClick={() => void applyTransport({ ...transport, congestion: cc })}
                >
                  {cc.toUpperCase()}
                </button>
              ))}
            </div>
          </div>
        </div>

        {/* ② 抗丢包与 RS-FEC */}
        <div className={styles.knobCard}>
          <h2 className={styles.knobTitle}>② 抗丢包与 RS-FEC 通道</h2>
          <label className={styles.checkRow}>
            <input
              type="checkbox"
              checked={transport.mediaFec}
              onChange={(e) =>
                void applyTransport({ ...transport, mediaFec: e.target.checked })
              }
            />
            <span>启用 QUIC Datagram + RS-FEC 前向纠错</span>
          </label>
          <label className={styles.sliderRow}>
            <span>FEC 冗余比例：{transport.fecOverheadPct}%</span>
            <input
              type="range"
              min={5}
              max={100}
              step={5}
              value={transport.fecOverheadPct}
              onChange={(e) =>
                void applyTransport({
                  ...transport,
                  fecOverheadPct: Number(e.target.value),
                })
              }
            />
          </label>
        </div>

        {/* ③ 编码与 1:1 无损瓦片（支持 4K@144Hz / 200Mbps） */}
        <div className={styles.knobCard}>
          <h2 className={styles.knobTitle}>③ 编码与 1:1 无损瓦片微操</h2>
          <div className={styles.checkGrid}>
            <label className={styles.checkRow}>
              <input
                type="checkbox"
                checked={codec.manualLock}
                onChange={(e) =>
                  void applyCodec({ ...codec, manualLock: e.target.checked })
                }
              />
              <span>手动锁定码率/帧率</span>
            </label>
            <label className={styles.checkRow}>
              <input
                type="checkbox"
                checked={codec.preferHw}
                onChange={(e) =>
                  void applyCodec({ ...codec, preferHw: e.target.checked })
                }
              />
              <span>优先硬件编码</span>
            </label>
            <label className={styles.checkRow}>
              <input
                type="checkbox"
                checked={codec.request444}
                onChange={(e) =>
                  void applyCodec({ ...codec, request444: e.target.checked })
                }
              />
              <span>YUV 4:4:4 无色度抽样</span>
            </label>
            <label className={styles.checkRow}>
              <input
                type="checkbox"
                checked={codec.tileEnabled}
                onChange={(e) =>
                  void applyCodec({ ...codec, tileEnabled: e.target.checked })
                }
              />
              <span>QOI 文字无损瓦片</span>
            </label>
          </div>
          <label className={styles.sliderRow}>
            <span>目标码率：{(codec.bitrateKbps / 1000).toFixed(1)} Mbps</span>
            <input
              type="range"
              min={2000}
              max={200000}
              step={2000}
              value={codec.bitrateKbps}
              onChange={(e) =>
                void applyCodec({ ...codec, bitrateKbps: Number(e.target.value) })
              }
            />
          </label>
          <label className={styles.sliderRow}>
            <span>帧率上限：{codec.maxFps} fps（支持 144Hz）</span>
            <input
              type="range"
              min={15}
              max={144}
              step={1}
              value={codec.maxFps}
              onChange={(e) =>
                void applyCodec({ ...codec, maxFps: Number(e.target.value) })
              }
            />
          </label>
          <button
            type="button"
            className={styles.btn}
            onClick={() => void applyCodec(codec, true)}
          >
            ⚡ 立即强制发送 IDR 关键帧 &amp; 重刷瓦片
          </button>
        </div>

        {/* ④ 可编程 Netem 弱网阀门 */}
        <div className={styles.knobCard}>
          <h2 className={styles.knobTitle}>④ 可编程 Netem 弱网阀门</h2>
          <div className={styles.pillRow}>
            {NETEM_PRESETS.map((p) => (
              <button
                key={p.name}
                type="button"
                className={
                  netem.enabled === p.cfg.enabled &&
                  netem.delayMs === p.cfg.delayMs &&
                  netem.lossPct === p.cfg.lossPct
                    ? `${styles.btn} ${styles.btnPrimary}`
                    : styles.btn
                }
                onClick={() => void applyNetem(p.cfg)}
              >
                {p.name}
              </button>
            ))}
          </div>
          <label className={styles.sliderRow}>
            <span>附加延迟：{netem.delayMs} ms · 丢包：{netem.lossPct}%</span>
            <input
              type="range"
              min={0}
              max={300}
              step={10}
              value={netem.delayMs}
              onChange={(e) =>
                void applyNetem({
                  ...netem,
                  enabled: Number(e.target.value) > 0 || netem.lossPct > 0,
                  delayMs: Number(e.target.value),
                })
              }
            />
          </label>
        </div>
      </div>

      {/* 示波器 + 1:1 微距像素检视镜 */}
      <div className={styles.visualGrid}>
        <div className={styles.knobCard}>
          <h2 className={styles.knobTitle}>
            1:1 微距像素检视镜（400% 最近邻 · {decodeStats.fps} fps）
          </h2>
          <div className={styles.magWrap}>
            <div className={styles.stageBox}>
              <canvas
                ref={videoCanvasRef}
                className={styles.rulerCanvas}
                onMouseMove={onCanvasMove}
              />
              <canvas ref={tileCanvasRef} className={styles.tileOverlayCanvas} />
            </div>
            <canvas ref={zoomCanvasRef} className={styles.zoomCanvas} />
          </div>
        </div>

        <div className={styles.knobCard}>
          <h2 className={styles.knobTitle}>60Hz 实时链路示波器</h2>
          <div className={styles.oscBlock}>
            <span className={styles.metricLabel}>RTT 延迟趋势 (ms)</span>
            <OscilloscopeLine data={rttHist} color="#58a6ff" />
          </div>
          <div className={styles.oscBlock}>
            <span className={styles.metricLabel}>视频码率 (绿) / 瓦片码率 (紫)</span>
            <OscilloscopeLine
              data={videoKbpsHist}
              color="#46d17e"
              secondary={tileKbpsHist}
              secColor="#bc8cff"
            />
          </div>
          <div className={styles.oscBlock}>
            <span className={styles.metricLabel}>帧率上限 (fps)</span>
            <OscilloscopeLine data={fpsHist} color="#e5b567" />
          </div>
        </div>
      </div>

      {/* A/B 矩阵对决台 */}
      <div className={styles.knobCard}>
        <h2 className={styles.knobTitle}>⚔️ 真协议栈 A/B 矩阵对决台</h2>
        <div className={styles.abControls}>
          <select
            className={styles.select}
            aria-label="方案 A"
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
            aria-label="方案 B"
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
            {abRunning ? "对决测算中…" : "启动 A/B 真栈对决"}
          </button>
        </div>

        {abResult && (
          <div className={styles.abResultBox}>
            <div className={styles.abWinner}>
              🏆 胜出：{abResult.winner} · {abResult.verdict}
            </div>
            <div className={styles.abCompareRow}>
              <div>
                <strong>{abResult.runA.label}</strong>：{abResult.runA.avgFps.toFixed(1)} fps ·
                P99 {abResult.runA.p99FrameMs.toFixed(1)}ms · PSNR {abResult.runA.psnrDb.toFixed(1)}dB ·
                文字 {abResult.runA.textFidelityPct.toFixed(1)}%
              </div>
              <div>
                <strong>{abResult.runB.label}</strong>：{abResult.runB.avgFps.toFixed(1)} fps ·
                P99 {abResult.runB.p99FrameMs.toFixed(1)}ms · PSNR {abResult.runB.psnrDb.toFixed(1)}dB ·
                文字 {abResult.runB.textFidelityPct.toFixed(1)}%
              </div>
            </div>
          </div>
        )}

        {allBenchResult && (
          <div className={styles.abResultBox}>
            <div className={styles.abWinner}>
              📊 全预设综合最优：{allBenchResult.bestName} · {allBenchResult.verdict}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
