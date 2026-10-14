/**
 * 窗口组视图（T-021）：单标签内容。每组一个 Channel（root + 子窗口帧都路由
 * 到此），视频层之上叠瓦片层；Fit 模式 ResizeObserver 去抖上报视口实现远端
 * 1:1 适配；Scale 本地等比缩放（contain）；Native 1:1 可滚动；zoom 经
 * effectiveDpr 生效。断线遮罩 + 3 次自动重连；Ctrl+Shift+S 导出函数注册到
 * exportFnRef 由页面层快捷键触发。
 *
 * 帧分发（模块级纯函数 dispatchFrame）：按首字节类型 + 帧头 window_id
 * 路由到对应窗口的 H264Decoder / TileLayer，未知窗口的帧安全忽略。
 */
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type RefObject,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { connectDevice } from "../api/devices";
import {
  closeWindow,
  openWindow,
  savePngToDownloads,
  setViewport,
  subscribeGroupEvents,
} from "../api/session";
import {
  H264Decoder,
  parseVideoHeader,
  VIDEO_HEADER_LEN,
  type DecodeStats,
} from "../video/decoder";
import {
  CanvasTileSurface,
  FRAME_INVALIDATE,
  FRAME_TILE,
  FRAME_VIDEO,
  parseTileHeader,
  TILE_HEADER_LEN,
  TileLayer,
  tilePayloadLen,
  type TileHeader,
} from "./TileLayer";
import {
  childOffset,
  effectiveDpr,
  useSessionSnapshot,
  type SessionStore,
} from "./SessionStore";
import styles from "./Session.module.css";

/** Fit 视口上报去抖（任务卡步骤 6；Agent 侧另有 150ms trailing 去抖）。 */
const VIEWPORT_DEBOUNCE_MS = 150;
/** 断线自动重连次数。 */
const RECONNECT_ATTEMPTS = 3;
/** 重连退避基数（第 n 次失败后等待 n 倍基数再试）。 */
const RECONNECT_BACKOFF_MS = 800;

/** 会话阶段。 */
type Phase = "connecting" | "open" | "reconnecting" | "failed";

/** 单窗口的帧处理面（视频解码器 + 瓦片层 + 对应 canvas 引用）。 */
interface SurfaceHandle {
  decoder: H264Decoder | null;
  tile: TileLayer | null;
  videoCanvas: HTMLCanvasElement | null;
  tileCanvas: HTMLCanvasElement | null;
}

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 等待毫秒。 */
function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    window.setTimeout(resolve, ms);
  });
}

/** 帧分发目标的最小结构（SurfaceHandle 结构兼容；测试可注入替身）。 */
export interface FrameSink {
  decoder: { push(buf: ArrayBuffer): void } | null;
  tile: {
    handleTile(header: TileHeader, rgba: Uint8ClampedArray<ArrayBuffer>): boolean;
    handleInvalidate(header: TileHeader): boolean;
  } | null;
}

/**
 * 帧分发：首字节为类型，视频/瓦片帧头自偏移 1 起解析，按 window_id
 * 路由到对应窗口的处理面；未知窗口的帧安全忽略。
 */
export function dispatchFrame(
  surfaces: ReadonlyMap<string, FrameSink>,
  buf: ArrayBuffer,
): void {
  if (buf.byteLength < 1 + TILE_HEADER_LEN) {
    return;
  }
  const type = new Uint8Array(buf)[0] ?? 0;
  if (type === FRAME_VIDEO) {
    if (buf.byteLength < 1 + VIDEO_HEADER_LEN) {
      return;
    }
    const header = parseVideoHeader(buf.slice(1));
    surfaces.get(header.windowId.toString())?.decoder?.push(buf.slice(1));
  } else if (type === FRAME_TILE) {
    const header = parseTileHeader(buf, 1);
    const len = tilePayloadLen(header);
    if (buf.byteLength < 1 + TILE_HEADER_LEN + len) {
      return;
    }
    const rgba = new Uint8ClampedArray(buf, 1 + TILE_HEADER_LEN, len);
    surfaces.get(header.windowId.toString())?.tile?.handleTile(header, rgba);
  } else if (type === FRAME_INVALIDATE) {
    const header = parseTileHeader(buf, 1);
    surfaces.get(header.windowId.toString())?.tile?.handleInvalidate(header);
  }
}

interface WindowSurfaceProps {
  /** 窗口 id（十进制字符串，帧路由键）。 */
  winId: string;
  /** 挂载时注册处理面、卸载时注销。 */
  register: (id: string, handle: SurfaceHandle | null) => void;
  /** 解码统计回调（父层只关心根窗口）。 */
  onStats: (winId: string, stats: DecodeStats) => void;
  /** 定位样式（absolute + left/top）。 */
  style: CSSProperties;
  /** 1:1 物理映射（CSS 像素 = 物理像素 / dpr）。 */
  effDpr: number;
  /** 整数倍缩放时启用 pixelated。 */
  pixelated: boolean;
}

/** 单窗口叠层：{视频 canvas, 瓦片 canvas}，瓦片在上、透明背景。 */
function WindowSurface({
  winId,
  register,
  onStats,
  style,
  effDpr,
  pixelated,
}: WindowSurfaceProps) {
  const videoRef = useRef<HTMLCanvasElement>(null);
  const tileRef = useRef<HTMLCanvasElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  // 回调走 ref，避免 effect 因回调身份变化反复重建 decoder
  const onStatsRef = useRef(onStats);
  useEffect(() => {
    onStatsRef.current = onStats;
  }, [onStats]);

  useEffect(() => {
    const video = videoRef.current;
    const tileEl = tileRef.current;
    if (video === null || tileEl === null) {
      return undefined;
    }
    const handle: SurfaceHandle = {
      decoder: null,
      tile: null,
      videoCanvas: video,
      tileCanvas: tileEl,
    };
    handle.tile = new TileLayer(new CanvasTileSurface(tileEl));
    handle.decoder = new H264Decoder(video, (stats) => {
      // 视频尺寸变化（重新 IDR）→ 瓦片层整层清空（任务卡步骤 4）
      handle.tile?.setSize(stats.width, stats.height);
      setSize({ width: stats.width, height: stats.height });
      onStatsRef.current(winId, stats);
    });
    register(winId, handle);
    return () => {
      handle.decoder?.close();
      register(winId, null);
    };
  }, [winId, register]);

  // canvas 像素尺寸 = 视频尺寸（decoder 内部设置）；CSS = 像素 / effectiveDpr
  const css: CSSProperties =
    size.width > 0
      ? {
          ...style,
          width: `${size.width / effDpr}px`,
          height: `${size.height / effDpr}px`,
        }
      : { ...style, visibility: "hidden" };
  const rendering = pixelated ? "pixelated" : "auto";
  return (
    <div className={styles.surface} style={css}>
      <canvas
        ref={videoRef}
        className={styles.videoCanvas}
        style={{ imageRendering: rendering }}
      />
      <canvas ref={tileRef} className={styles.tileCanvas} />
    </div>
  );
}

interface GroupViewProps {
  deviceId: string;
  windowId: string;
  store: SessionStore;
  /** HUD 显隐（Ctrl+Shift+H 切换）。 */
  showHud: boolean;
  /** 导出函数注册位（页面层 Ctrl+Shift+S 触发）。 */
  exportFnRef: RefObject<(() => Promise<string>) | null>;
  /** 根窗口被远端销毁（DESTROYED 事件命中根）→ 页面移除标签。 */
  onRootGone: () => void;
}

/** 窗口组视图：一个标签的全部内容。 */
export function WindowGroupView({
  deviceId,
  windowId,
  store,
  showHud,
  exportFnRef,
  onRootGone,
}: GroupViewProps) {
  const snapshot = useSessionSnapshot(store);
  const containerRef = useRef<HTMLDivElement>(null);
  const surfacesRef = useRef(new Map<string, SurfaceHandle>());
  const [phase, setPhase] = useState<Phase>("connecting");
  const [error, setError] = useState<string | null>(null);
  /** 重开代数：重连成功/模式切换需重建会话时递增。 */
  const [epoch, setEpoch] = useState(0);
  const [rootStats, setRootStats] = useState<DecodeStats | null>(null);
  const [containerSize, setContainerSize] = useState({ w: 0, h: 0 });
  // 回调走 ref，避免主 effect 因回调身份变化被反复重建
  const onRootGoneRef = useRef(onRootGone);
  useEffect(() => {
    onRootGoneRef.current = onRootGone;
  });

  /** 处理面注册表（子组件挂载注册、卸载注销）。 */
  const register = useCallback((id: string, handle: SurfaceHandle | null) => {
    if (handle === null) {
      surfacesRef.current.delete(id);
    } else {
      surfacesRef.current.set(id, handle);
    }
  }, []);

  const onSurfaceStats = useCallback(
    (winId: string, stats: DecodeStats) => {
      if (winId === windowId) {
        setRootStats(stats);
      }
    },
    [windowId],
  );

  // 容器尺寸跟踪（Scale 模式计算缩放比；Fit 视口上报另有观察器）
  useEffect(() => {
    const el = containerRef.current;
    if (el === null) {
      return undefined;
    }
    const update = () => {
      setContainerSize({ w: el.clientWidth, h: el.clientHeight });
    };
    update();
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => {
      ro.disconnect();
    };
  }, []);

  // 主会话 effect：订阅组事件 + 打开窗口；epoch 变更（重连/重开）时整体重建
  useEffect(() => {
    let cancelled = false;
    surfacesRef.current.clear();
    setPhase("connecting");
    setError(null);
    setRootStats(null);

    const onFrame = (frame: ArrayBuffer) => {
      dispatchFrame(surfacesRef.current, frame);
    };
    const sub = subscribeGroupEvents(deviceId, windowId, (ev) => {
      if (cancelled) {
        return;
      }
      store.applyGroupEvent(ev);
      if (ev.kind === "DESTROYED" && ev.info !== null && ev.info.id === windowId) {
        onRootGoneRef.current();
      }
    });
    sub.catch((e: unknown) => {
      if (!cancelled) {
        setError(errText(e));
        setPhase("failed");
      }
    });
    // M1 固定以 Fit 打开（远端 1:1 适配默认）；切 Scale/Native 只停止视口
    // 上报并本地缩放，切回 Fit 无需重开（open.fit 恒为 FIT）
    openWindow(deviceId, windowId, "FIT", 0, onFrame)
      .then((ack) => {
        if (cancelled) {
          return;
        }
        store.setAck(ack);
        setPhase("open");
      })
      .catch((e: unknown) => {
        if (!cancelled) {
          setError(errText(e));
          setPhase("failed");
        }
      });

    return () => {
      cancelled = true;
      surfacesRef.current.clear();
      void sub.catch(() => undefined);
      // Agent 对未打开窗口的 CloseWindow 静默忽略，重复发送安全
      void closeWindow(deviceId, windowId).catch(() => undefined);
    };
  }, [deviceId, windowId, epoch, store]);

  // 断线感知：device-disconnected 事件 → 遮罩 + 自动重连
  useEffect(() => {
    const un = listen<{ deviceId?: string }>("device-disconnected", (e) => {
      if (e.payload?.deviceId === deviceId) {
        setPhase((cur) => (cur === "open" ? "reconnecting" : cur));
      }
    });
    return () => {
      void un.then((finalize) => finalize());
    };
  }, [deviceId]);

  // 自动重连：最多 3 次，成功后递增 epoch 重建会话（重开窗口）
  useEffect(() => {
    if (phase !== "reconnecting") {
      return undefined;
    }
    let cancelled = false;
    const run = async () => {
      for (let attempt = 1; attempt <= RECONNECT_ATTEMPTS; attempt += 1) {
        try {
          await connectDevice(deviceId);
          if (cancelled) {
            return;
          }
          setEpoch((n) => n + 1);
          setPhase("open");
          return;
        } catch {
          await delay(RECONNECT_BACKOFF_MS * attempt);
        }
      }
      if (!cancelled) {
        setError("自动重连 3 次失败");
        setPhase("failed");
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [phase, deviceId]);

  // Fit 模式视口上报：初始一次 + ResizeObserver 去抖 + zoom 变化/重开后重报
  useEffect(() => {
    if (snapshot.fit !== "FIT") {
      return undefined;
    }
    const el = containerRef.current;
    if (el === null) {
      return undefined;
    }
    const send = () => {
      const dpr = window.devicePixelRatio || 1;
      setViewport(
        deviceId,
        windowId,
        Math.max(1, Math.round(el.clientWidth)),
        Math.max(1, Math.round(el.clientHeight)),
        dpr / snapshot.zoom,
      ).catch(() => undefined);
    };
    let timer: number | undefined;
    const schedule = () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(send, VIEWPORT_DEBOUNCE_MS);
    };
    send();
    const ro = new ResizeObserver(schedule);
    ro.observe(el);
    return () => {
      ro.disconnect();
      window.clearTimeout(timer);
    };
  }, [deviceId, windowId, snapshot.fit, snapshot.zoom, phase]);

  /** 导出当前根窗口 video+tile 合成画面（原始像素 PNG → 下载目录）。 */
  const exportPng = useCallback(async (): Promise<string> => {
    const root = surfacesRef.current.get(windowId);
    const video = root?.videoCanvas;
    if (video === undefined || video === null || video.width === 0 || video.height === 0) {
      throw new Error("尚无视频帧，无法导出");
    }
    const out = document.createElement("canvas");
    out.width = video.width;
    out.height = video.height;
    const ctx = out.getContext("2d");
    if (ctx === null) {
      throw new Error("无法创建导出画布");
    }
    ctx.drawImage(video, 0, 0);
    const tile = root?.tileCanvas;
    if (tile !== undefined && tile !== null) {
      ctx.drawImage(tile, 0, 0);
    }
    const blob = await new Promise<Blob | null>((resolve) => {
      out.toBlob(resolve, "image/png");
    });
    if (blob === null) {
      throw new Error("PNG 编码失败");
    }
    const bytes = new Uint8Array(await blob.arrayBuffer());
    const stamp = new Date().toISOString().replace(/[:.]/g, "-");
    return savePngToDownloads(`rd-${windowId}-${stamp}.png`, bytes);
  }, [windowId]);

  useEffect(() => {
    exportFnRef.current = exportPng;
    return () => {
      if (exportFnRef.current === exportPng) {
        exportFnRef.current = null;
      }
    };
  }, [exportFnRef, exportPng]);

  // ---- 渲染 ----

  const dpr = window.devicePixelRatio || 1;
  const effDpr = effectiveDpr(dpr, snapshot.zoom);
  const cssW = rootStats !== null && rootStats.width > 0 ? rootStats.width / effDpr : 0;
  const cssH = rootStats !== null && rootStats.height > 0 ? rootStats.height / effDpr : 0;
  // Scale：CSS 等比缩放到容器（contain），整数倍时 pixelated
  const scale =
    snapshot.fit === "SCALE" && cssW > 0 && containerSize.w > 0
      ? Math.min(containerSize.w / cssW, containerSize.h / cssH)
      : 1;
  const pixelated =
    snapshot.fit === "SCALE" ? Number.isInteger(scale) : Number.isInteger(snapshot.zoom);
  const scaled = snapshot.fit === "SCALE";
  const wrapStyle: CSSProperties = {
    width: cssW > 0 ? `${cssW}px` : undefined,
    height: cssH > 0 ? `${cssH}px` : undefined,
    transform: scaled ? `scale(${scale})` : undefined,
    transformOrigin: "top left",
  };

  const rootSurface = (
    <WindowSurface
      winId={windowId}
      register={register}
      onStats={onSurfaceStats}
      effDpr={effDpr}
      pixelated={pixelated}
      style={{ left: 0, top: 0 }}
    />
  );

  const overlay =
    phase === "open" ? null : (
      <div className={styles.overlay}>
        <p className={styles.overlayText}>
          {phase === "connecting"
            ? "正在打开会话…"
            : phase === "reconnecting"
              ? "已断开，重连中…"
              : (error ?? "会话失败")}
        </p>
        {phase === "failed" && (
          <button
            type="button"
            className={styles.retryBtn}
            onClick={() => {
              setError(null);
              setPhase("reconnecting");
            }}
          >
            重试
          </button>
        )}
      </div>
    );

  const hud =
    showHud === false ? null : (
      <div className={styles.hud}>
        <span className={styles.hudItem}>
          fps {rootStats !== null ? rootStats.fps : "-"}
        </span>
        <span className={styles.hudItem}>
          decode{" "}
          {rootStats !== null ? `${rootStats.decodeMs.toFixed(1)} ms` : "-"}
        </span>
        <span className={styles.hudItem}>
          {rootStats !== null && rootStats.width > 0
            ? `${rootStats.width}x${rootStats.height}`
            : "-"}
        </span>
        <span className={styles.hudItem}>
          RTT {snapshot.stats !== null ? `${snapshot.stats.rttMs} ms` : "-"}
        </span>
        <span className={styles.hudItem}>
          {snapshot.stats !== null ? `${snapshot.stats.kbps} kbps` : "-"}
        </span>
        <span className={styles.hudItem}>{snapshot.ack?.codec ?? "-"}</span>
        <span className={styles.hudItem}>
          {snapshot.fit} ×{Math.round(snapshot.zoom * 100)}%
        </span>
      </div>
    );

  const children = snapshot.children.map((child) => {
    const off = childOffset(child);
    return (
      <WindowSurface
        key={child.id}
        winId={child.id}
        register={register}
        onStats={onSurfaceStats}
        effDpr={effDpr}
        pixelated={pixelated}
        style={{
          left: `${off.x / effDpr}px`,
          top: `${off.y / effDpr}px`,
        }}
      />
    );
  });

  return (
    <div className={styles.stage} ref={containerRef}>
      <div className={scaled ? styles.stageCenter : styles.stageTopLeft}>
        <div className={styles.group} style={wrapStyle}>
          {rootSurface}
          {children}
        </div>
      </div>
      {overlay}
      {hud}
    </div>
  );
}
