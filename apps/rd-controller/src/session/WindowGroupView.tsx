/**
 * 窗口组视图（T-021）：单标签内容。每组一个 Channel（root + 子窗口帧都路由
 * 到此），视频层之上叠瓦片层；Fit 模式 ResizeObserver 去抖上报视口实现远端
 * 1:1 适配；Scale 本地等比缩放（contain）；Native 1:1 可滚动；zoom 经
 * effectiveDpr 生效。断线遮罩 + 3 次自动重连；Ctrl+Shift+S 导出函数注册到
 * exportFnRef 由页面层快捷键触发。
 *
 * 帧分发（模块级纯函数 dispatchFrame）：按首字节类型 + 帧头 window_id
 * 路由到对应窗口的 H264Decoder / TileLayer，未知窗口的帧安全忽略。
 *
 * 处理面注册表不变量：surfacesRef 只能经 registerSurface 增删（WindowSurface
 * 挂载注册/卸载注销），主会话 effect 重建（断线重连 epoch++）不得清空——
 * 否则注册 effect 依赖未变、无人重新注册，帧被 dispatchFrame 静默丢弃（黑屏）。
 */
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type CompositionEvent,
  type CSSProperties,
  type FocusEvent,
  type KeyboardEvent,
  type PointerEvent,
  type RefObject,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { connectDevice } from "../api/devices";
import {
  keyEvent,
  mouseButton,
  mouseMove,
  mouseWheel,
  textInput,
} from "../api/input";
import {
  closeWindow,
  openWindow,
  requestClipboard,
  savePngToDownloads,
  sendClipboard,
  sessionStateText,
  setViewport,
  subscribeGroupEvents,
  subscribeSessionState,
} from "../api/session";
import {
  hitWindow,
  InputMapper,
  loadCmdToCtrl,
  mapButton,
  mapGroupPoint,
  MouseMovePacer,
  normalizeWheel,
  saveCmdToCtrl,
  type KeyInput,
} from "../input/InputMapper";
import {
  codecStringFor,
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
/** 断线自动重连退避基数（第 n 次失败后等待 min(n×基数, 上限) 再试）。 */
const RECONNECT_BACKOFF_MS = 800;
/** 重连退避上限（D-013 包2：睡眠唤醒/Agent 重启场景持续重试直到恢复）。 */
const RECONNECT_BACKOFF_MAX_MS = 30_000;

/** 隐藏的键盘捕获 textarea（任务卡步骤 5）：1px 透明，不参与指针命中。 */
const CAPTURE_STYLE: CSSProperties = {
  position: "absolute",
  left: 0,
  top: 0,
  width: 1,
  height: 1,
  opacity: 0,
  border: "none",
  padding: 0,
  margin: 0,
  resize: "none",
  outline: "none",
  overflow: "hidden",
  pointerEvents: "none",
};

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
    handleTile(
      header: TileHeader,
      rgba: Uint8ClampedArray<ArrayBuffer>,
    ): boolean;
    handleInvalidate(header: TileHeader): boolean;
  } | null;
}

/**
 * 处理面注册表的唯一增删入口：handle 为 null 注销（WindowSurface 卸载），
 * 否则按 winId 注册/覆盖。条目只能由此增删——主会话 effect 重建（断线重连
 * epoch++）不清空注册表，否则重连后注册 effect 依赖未变、无人重新注册，
 * 帧会被 dispatchFrame 静默丢弃（重连黑屏，切标签重挂载才恢复）。
 */
export function registerSurface<H>(
  surfaces: Map<string, H>,
  id: string,
  handle: H | null,
): void {
  if (handle === null) {
    surfaces.delete(id);
  } else {
    surfaces.set(id, handle);
  }
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
  /** WebCodecs codec string（按 ack.codec 选定，组内窗口一致）。 */
  codec: string;
  /** 会话重开代数：变更时注销旧处理面并重建（重连成功后帧路由随之恢复）。 */
  sessionEpoch: number;
}

/** 单窗口叠层：{视频 canvas, 瓦片 canvas}，瓦片在上、透明背景。 */
function WindowSurface({
  winId,
  register,
  onStats,
  style,
  effDpr,
  pixelated,
  codec,
  sessionEpoch,
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
    }, codec);
    register(winId, handle);
    return () => {
      handle.decoder?.close();
      register(winId, null);
    };
    // codec 变化（ack 到达后 444 协商命中）→ 关闭旧解码器、按协商 profile
    // 重建；420（默认值不变）时不重跑，零开销。sessionEpoch 变化（断线重连
    // 成功后 epoch++ 重建会话）→ 同样注销并重建，保证解码器/瓦片层不跨会话
    // 复用。React 子 effect 先于父主 effect 运行：重建后的注册先于新会话的
    // openWindow，首帧不丢；若重建时已有帧到达（竞争窗口极小），新解码器
    // 等待下一个 keyframe。
  }, [winId, register, codec, sessionEpoch]);

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
  /** 当前是第几次重连尝试（遮罩文案用；0 = 不在重连中）。 */
  const [reconnectAttempt, setReconnectAttempt] = useState(0);
  /** 只看不控（v0.1/D-013 包4）：变更即重开会话。 */
  const [viewOnly, setViewOnly] = useState(false);
  /** Agent 会话级状态横幅（锁屏/暂停/退出；null = 不显示）。 */
  const [banner, setBanner] = useState<string | null>(null);
  /** 被控机本机有人在用（D-013 包5）：注入已被 Agent 挂起，显示禁止光标。 */
  const [hostBusy, setHostBusy] = useState(false);
  /** 重开代数：重连成功/模式切换需重建会话时递增。 */
  const [epoch, setEpoch] = useState(0);
  const [rootStats, setRootStats] = useState<DecodeStats | null>(null);
  const [containerSize, setContainerSize] = useState({ w: 0, h: 0 });
  // 回调走 ref，避免主 effect 因回调身份变化被反复重建
  const onRootGoneRef = useRef(onRootGone);
  useEffect(() => {
    onRootGoneRef.current = onRootGone;
  });

  /** 处理面注册表（子组件挂载注册、卸载注销）；条目只能经 registerSurface 增删。 */
  const register = useCallback((id: string, handle: SurfaceHandle | null) => {
    registerSurface(surfacesRef.current, id, handle);
  }, []);

  const onSurfaceStats = useCallback(
    (winId: string, stats: DecodeStats) => {
      if (winId === windowId) {
        setRootStats(stats);
      }
    },
    [windowId],
  );

  // ---- 输入映射（T-022 挂载）----

  const groupRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  /** Cmd→Ctrl（任务卡步骤 7，默认开，localStorage 持久化）。 */
  const [cmdToCtrl, setCmdToCtrl] = useState<boolean>(() => loadCmdToCtrl());
  /** 发送时读取最新 deviceId，避免闭包捕获旧连接。 */
  const deviceIdRef = useRef(deviceId);
  useEffect(() => {
    deviceIdRef.current = deviceId;
  }, [deviceId]);
  /** 根窗口视频物理尺寸（坐标映射基准；无帧时不发送）。 */
  const rootStatsRef = useRef(rootStats);
  rootStatsRef.current = rootStats;

  /** 吞掉发送错误：输入丢失不弹错，断连由会话层遮罩与重连兜底。 */
  const sendInput = useCallback((p: Promise<void>) => {
    void p.catch(() => undefined);
  }, []);

  /** 键盘 / IME 映射器（生命周期 = 本标签；失焦抬起全部按下键）。 */
  const mapperRef = useRef<InputMapper | null>(null);
  if (mapperRef.current === null) {
    mapperRef.current = new InputMapper(
      {
        key: (k) => {
          sendInput(
            keyEvent(deviceIdRef.current, k.scancode, k.extended, k.down, k.modifiers),
          );
        },
        text: (t) => {
          sendInput(textInput(deviceIdRef.current, t));
        },
      },
      { cmdToCtrl },
    );
  }
  useEffect(() => {
    const mapper = mapperRef.current;
    if (mapper !== null) {
      mapper.cmdToCtrl = cmdToCtrl;
    }
  }, [cmdToCtrl]);

  /** 鼠标移动 rAF 合并器（任务卡步骤 3；卸载丢弃挂起帧）。 */
  const pacerRef = useRef<MouseMovePacer | null>(null);
  if (pacerRef.current === null) {
    pacerRef.current = new MouseMovePacer((m) => {
      sendInput(mouseMove(deviceIdRef.current, m.windowId, m.x, m.y));
    });
  }
  useEffect(
    () => () => {
      pacerRef.current?.cancel();
      mapperRef.current?.releaseAll();
    },
    [sendInput],
  );

  /** 组容器 CSS 坐标 → 根窗口物理像素（无容器/无帧返回 null：不发送）。 */
  const toPhys = useCallback((clientX: number, clientY: number) => {
    const el = groupRef.current;
    if (el === null) {
      return null;
    }
    const rect = el.getBoundingClientRect();
    const rs = rootStatsRef.current;
    if (
      rect.width <= 0 ||
      rect.height <= 0 ||
      rs === null ||
      rs.width <= 0 ||
      rs.height <= 0
    ) {
      return null;
    }
    return mapGroupPoint(
      clientX - rect.left,
      clientY - rect.top,
      rect.width,
      rect.height,
      rs.width,
      rs.height,
    );
  }, []);

  /** 物理坐标命中的窗口记录（子窗口上层优先，回退根窗口）。 */
  const hitAt = useCallback(
    (p: { x: number; y: number }) => {
      const rs = rootStatsRef.current;
      if (rs === null) {
        return null;
      }
      return hitWindow(p.x, p.y, windowId, rs.width, rs.height, snapshot.children);
    },
    [windowId, snapshot.children],
  );

  /**
   * 按下中的鼠标按钮：按下时命中的窗口（含其在根坐标系中的偏移）与
   * **该窗口相对坐标**（协议 x/y 为目标窗口客户区坐标，Agent 按窗口自身
   * 原点换算）。抬起按原窗原位发送。
   */
  const downWinsRef = useRef(
    new Map<number, { id: string; ox: number; oy: number; x: number; y: number }>(),
  );

  const onPointerMove = (e: PointerEvent<HTMLDivElement>): void => {
    if (phase !== "open") {
      return;
    }
    const p = toPhys(e.clientX, e.clientY);
    if (p === null) {
      return;
    }
    // 有按钮按下（拖拽/拖选）：移动继续发给按下的窗口（捕获语义），
    // 不随光标跨越子窗口边界换目标
    const downs = [...downWinsRef.current.values()];
    if (downs.length > 0) {
      const target = downs[downs.length - 1];
      if (target !== undefined) {
        target.x = p.x - target.ox;
        target.y = p.y - target.oy;
        pacerRef.current?.submit({ windowId: target.id, x: target.x, y: target.y });
      }
      return;
    }
    const hit = hitAt(p);
    if (hit === null) {
      return;
    }
    pacerRef.current?.submit({
      windowId: hit.id,
      x: p.x - hit.x,
      y: p.y - hit.y,
    });
  };

  const onPointerDown = (e: PointerEvent<HTMLDivElement>): void => {
    if (phase !== "open") {
      return;
    }
    textareaRef.current?.focus({ preventScroll: true });
    const p = toPhys(e.clientX, e.clientY);
    if (p !== null) {
      const hit = hitAt(p);
      if (hit !== null) {
        const btn = mapButton(e.button);
        const rel = { x: p.x - hit.x, y: p.y - hit.y };
        downWinsRef.current.set(btn, {
          id: hit.id,
          ox: hit.x,
          oy: hit.y,
          x: rel.x,
          y: rel.y,
        });
        sendInput(mouseButton(deviceIdRef.current, hit.id, rel.x, rel.y, btn, true));
      }
    }
    e.preventDefault();
    try {
      e.currentTarget.setPointerCapture(e.pointerId);
    } catch {
      // 捕获失败不影响本次点击（指针未离开容器时事件仍按几何命中）
    }
  };

  /** 按下按钮抬起（pointerup / pointercancel 共用）。 */
  const releaseButton = (e: PointerEvent<HTMLDivElement>): void => {
    if (phase !== "open") {
      return;
    }
    const btn = mapButton(e.button);
    const down = downWinsRef.current.get(btn);
    if (down === undefined) {
      return;
    }
    downWinsRef.current.delete(btn);
    sendInput(mouseButton(deviceIdRef.current, down.id, down.x, down.y, btn, false));
  };

  /** 捕获丢失（Alt+Tab / 系统手势）：抬起全部按下按钮防远端卡键。 */
  const onLostPointerCapture = (): void => {
    for (const [btn, down] of downWinsRef.current) {
      sendInput(mouseButton(deviceIdRef.current, down.id, down.x, down.y, btn, false));
    }
    downWinsRef.current.clear();
  };

  // 滚轮：原生监听（passive:false 才能 preventDefault 阻止页面滚动）
  useEffect(() => {
    const el = groupRef.current;
    if (el === null) {
      return undefined;
    }
    const onWheel = (e: WheelEvent): void => {
      if (phase !== "open") {
        return;
      }
      e.preventDefault();
      const p = toPhys(e.clientX, e.clientY);
      if (p === null) {
        return;
      }
      const hit = hitAt(p);
      if (hit === null) {
        return;
      }
      const { dx, dy } = normalizeWheel(e.deltaMode, e.deltaX, e.deltaY, e.shiftKey);
      if (dx !== 0 || dy !== 0) {
        sendInput(
          mouseWheel(deviceIdRef.current, hit.id, p.x - hit.x, p.y - hit.y, dx, dy),
        );
      }
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
    };
  }, [phase, toPhys, hitAt, sendInput]);

  const keyInputOf = (e: KeyboardEvent<HTMLTextAreaElement>): KeyInput => ({
    code: e.code,
    ctrlKey: e.ctrlKey,
    shiftKey: e.shiftKey,
    altKey: e.altKey,
    metaKey: e.metaKey,
    // React 合成事件类型未暴露 isComposing，从原生事件读取
    isComposing: e.nativeEvent.isComposing,
  });

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>): void => {
    const action = mapperRef.current?.handleKeyDown(keyInputOf(e)) ?? "ignored";
    if (action === "forwarded") {
      e.preventDefault();
    }
  };

  const onKeyUp = (e: KeyboardEvent<HTMLTextAreaElement>): void => {
    const action = mapperRef.current?.handleKeyUp(keyInputOf(e)) ?? "ignored";
    if (action === "forwarded") {
      e.preventDefault();
    }
  };

  const onCompositionStart = (): void => {
    const mapper = mapperRef.current;
    if (mapper !== null) {
      mapper.composing = true;
    }
  };

  const onCompositionEnd = (e: CompositionEvent<HTMLTextAreaElement>): void => {
    const mapper = mapperRef.current;
    if (mapper !== null) {
      mapper.handleCompositionEnd(e.data);
    }
    e.currentTarget.value = "";
  };

  const onCaptureBlur = (e: FocusEvent<HTMLTextAreaElement>): void => {
    e.currentTarget.value = "";
    mapperRef.current?.releaseAll();
  };

  // 会话失活（断线重连 / 失败）：抬起远端按键与鼠标按钮，防切走后卡键
  useEffect(() => {
    if (phase !== "open") {
      mapperRef.current?.releaseAll();
      downWinsRef.current.clear();
    }
  }, [phase]);

  // 窗口失焦：发送已按下键的抬起（任务卡步骤 8）
  useEffect(() => {
    const onWinBlur = (): void => {
      mapperRef.current?.releaseAll();
    };
    window.addEventListener("blur", onWinBlur);
    return () => {
      window.removeEventListener("blur", onWinBlur);
    };
  }, []);

  // 会话打开后聚焦键盘捕获（键盘即生效；点 HUD 控件可正常夺走焦点）
  useEffect(() => {
    if (phase === "open") {
      textareaRef.current?.focus({ preventScroll: true });
    }
  }, [phase]);

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

  // 主会话 effect：订阅组事件 + 打开窗口；epoch 变更（重连/重开）时整体重建。
  // 不变量：此处不得清空 surfacesRef——注册表由 WindowSurface 的注册 effect
  // 持有，会话重建时其依赖随 sessionEpoch 变化自行注销重注册；在此清空会使
  // 重连后的帧路由指向空表（帧被 dispatchFrame 静默丢弃 → 黑屏，切标签才恢复）。
  useEffect(() => {
    let cancelled = false;
    setPhase("connecting");
    setError(null);
    setRootStats(null);

    const onFrame = (frame: ArrayBuffer) => {
      // 临时探针：帧到达计数（诊断会话黑屏，定位后移除）
      const w = window as unknown as { __fc?: number; __fb?: number };
      w.__fc = (w.__fc ?? 0) + 1;
      if (w.__fc === 1) console.log("[probe] first onFrame bytes=", frame.byteLength);
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
    // 上报并本地缩放，切回 Fit 无需重开（open.fit 恒为 FIT）。
    // viewOnly（v0.1）变更需重开会话（注入门在 Agent 侧按打开参数判定）。
    openWindow(deviceId, windowId, "FIT", 0, viewOnly, onFrame)
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
      // 注：不清空 surfacesRef。条目随 WindowSurface 卸载/重建由其自身注销
      // （register(id, null)）；清理函数若在此 clear，主 effect 重跑（重连
      // epoch++）时会先抹掉全部注册，且无人重新注册 → 重连黑屏。
      void sub.catch(() => undefined);
      // Agent 对未打开窗口的 CloseWindow 静默忽略，重复发送安全
      void closeWindow(deviceId, windowId).catch(() => undefined);
    };
  }, [deviceId, windowId, epoch, store, viewOnly]);

  // 会话级状态（v0.1）：锁屏/暂停/Agent 退出 → 顶部横幅
  useEffect(() => {
    if (phase === "failed") {
      return undefined;
    }
    let alive = true;
    let unlisten: (() => void) | null = null;
    void subscribeSessionState(deviceId, (s) => {
      if (!alive) {
        return;
      }
      setBanner(sessionStateText(s));
      // 本机优先(D-013 包5):被控机有人在用 → 禁止光标;空闲恢复
      setHostBusy(s.kind === 6);
    })
      .then((fn) => {
        if (alive) {
          unlisten = fn;
        } else {
          fn();
        }
      })
      .catch(() => undefined);
    return () => {
      alive = false;
      unlisten?.();
    };
  }, [deviceId, phase]);

  /** 工具条:只看不控切换(重开会话)/剪贴板互传(v0.1)。 */
  const onToggleViewOnly = useCallback(() => {
    setViewOnly((v) => !v);
    setEpoch((n) => n + 1);
  }, []);

  const onCopyToRemote = useCallback(async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (text.length > 0) {
        await sendClipboard(deviceId, text);
      }
    } catch (e) {
      setError(`复制到远端失败：${errText(e)}`);
    }
  }, [deviceId]);

  const onCopyFromRemote = useCallback(async () => {
    try {
      const text = await requestClipboard(deviceId);
      if (text.length > 0) {
        await navigator.clipboard.writeText(text);
      }
    } catch (e) {
      setError(`从远端复制失败：${errText(e)}`);
    }
  }, [deviceId]);

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

  // 自动重连：持续指数退避（上限 30s），成功后递增 epoch 重建会话。
  // D-013 包2：不再 3 次放弃——被控端睡眠唤醒/Agent 重启需要等待对方恢复，
  // 放弃意味着必须有人守在控制端手动点重试。
  useEffect(() => {
    if (phase !== "reconnecting") {
      return undefined;
    }
    let cancelled = false;
    const run = async () => {
      for (let attempt = 1; ; attempt += 1) {
        try {
          await connectDevice(deviceId);
          if (cancelled) {
            return;
          }
          setEpoch((n) => n + 1);
          setPhase("open");
          return;
        } catch {
          if (cancelled) {
            return;
          }
          setReconnectAttempt(attempt);
          await delay(
            Math.min(RECONNECT_BACKOFF_MS * attempt, RECONNECT_BACKOFF_MAX_MS),
          );
        }
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [phase, deviceId]);

  // 重连成功回 open 时清零次数显示
  useEffect(() => {
    if (phase === "open") {
      setReconnectAttempt(0);
    }
  }, [phase]);

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

  // 按 ack.codec 选定解码 codec string（ack 前为 420 默认；444 协商命中时
  // ack 到达后变化，触发 WindowSurface 按正确 profile 重建解码器）
  const decoderCodec = codecStringFor(snapshot.ack?.codec);

  const rootSurface = (
    <WindowSurface
      winId={windowId}
      register={register}
      onStats={onSurfaceStats}
      effDpr={effDpr}
      pixelated={pixelated}
      style={{ left: 0, top: 0 }}
      codec={decoderCodec}
      sessionEpoch={epoch}
    />
  );

  const overlay =
    phase === "open" ? null : (
      <div className={styles.overlay}>
        <p className={styles.overlayText}>
          {phase === "connecting"
            ? "正在打开会话…"
            : phase === "reconnecting"
              ? reconnectAttempt > 3
                ? `连接不上，正在持续重试（第 ${reconnectAttempt} 次）…请确认对方电脑已开机、rd-agent 在运行`
                : "已断开，重连中…"
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
        <label className={styles.hudItem} title="macOS：Cmd 组合键映射为远端 Ctrl">
          <input
            type="checkbox"
            checked={cmdToCtrl}
            onChange={(e) => {
              const v = e.currentTarget.checked;
              saveCmdToCtrl(v);
              setCmdToCtrl(v);
            }}
          />{" "}
          Cmd→Ctrl
        </label>
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
        codec={decoderCodec}
        sessionEpoch={epoch}
      />
    );
  });

  return (
    <div
      className={styles.stage}
      ref={containerRef}
      style={hostBusy ? { cursor: "not-allowed" } : undefined}
    >
      {/* 会话工具条(v0.1/D-013 包4):只看不控 + 剪贴板互传 */}
      <div className={styles.sessionBar}>
        <label
          className={styles.sessionBarItem}
          title="开启后只能观看画面,键鼠注入被被控端拦截(防误操作/只监控场景)"
        >
          <input type="checkbox" checked={viewOnly} onChange={onToggleViewOnly} />
          只看不控
        </label>
        <button
          type="button"
          className={styles.sessionBarBtn}
          onClick={() => void onCopyToRemote()}
          title="把控制端剪贴板文本发送到远端(远端 Ctrl+V 粘贴)"
        >
          复制到远端
        </button>
        <button
          type="button"
          className={styles.sessionBarBtn}
          onClick={() => void onCopyFromRemote()}
          title="取回远端剪贴板文本到控制端"
        >
          从远端复制
        </button>
      </div>
      {banner !== null && <div className={styles.banner}>{banner}</div>}
      <div className={scaled ? styles.stageCenter : styles.stageTopLeft}>
        <div
          className={styles.group}
          style={wrapStyle}
          ref={groupRef}
          onPointerMove={onPointerMove}
          onPointerDown={onPointerDown}
          onPointerUp={releaseButton}
          onPointerCancel={releaseButton}
          onLostPointerCapture={onLostPointerCapture}
          onContextMenu={(e) => {
            e.preventDefault();
          }}
        >
          {rootSurface}
          {children}
          <textarea
            ref={textareaRef}
            style={CAPTURE_STYLE}
            tabIndex={-1}
            aria-hidden="true"
            spellCheck={false}
            autoComplete="off"
            onKeyDown={onKeyDown}
            onKeyUp={onKeyUp}
            onCompositionStart={onCompositionStart}
            onCompositionEnd={onCompositionEnd}
            onBlur={onCaptureBlur}
          />
        </div>
      </div>
      {overlay}
      {hud}
    </div>
  );
}
