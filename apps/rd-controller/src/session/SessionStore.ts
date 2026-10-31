/**
 * 会话状态（T-021）：每个窗口组一个 SessionStore。
 *
 * 状态：根尺寸/原点、子窗口表、fit 模式、zoom、退化标记、Agent 统计、ack。
 * 更新为不可变替换（纯函数 applyGroupEvent），React 经 useSyncExternalStore
 * 订阅快照（Windows.tsx 纯函数导出的同款测试策略）。
 */
import type {
  FitMode,
  GroupEventDto,
  GroupWindowDto,
  OpenWindowAckDto,
  StatsDto,
} from "../api/session";
import { useSyncExternalStore } from "react";

/** 可用缩放倍率（任务卡步骤 6）。 */
export const ZOOM_LEVELS: readonly number[] = [1, 1.5, 2];

/** 组内窗口条目（子窗口）。 */
export interface ChildWin {
  readonly id: string;
  readonly title: string;
  /** 相对根客户区原点的偏移（物理像素，T-027 事件直接携带）。 */
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
  readonly dpi: number;
}

/** 会话快照（不可变）。 */
export interface SessionSnapshot {
  readonly rootId: string;
  /** 根窗口标题（MOVED/TITLE 事件更新；未知为 null）。 */
  readonly title: string | null;
  readonly children: readonly ChildWin[];
  readonly fit: FitMode;
  readonly zoom: number;
  /** 收到 Error{12}（远端无法调整尺寸）已自动切 Scale。 */
  readonly degraded: boolean;
  /** 面向用户的提示（如 Fit 退化说明）。 */
  readonly notice: string | null;
  /** Agent StatsReport（连接级）。 */
  readonly stats: StatsDto | null;
  /** open_window 的 ack（codec/level/尺寸）。 */
  readonly ack: OpenWindowAckDto | null;
  /** 只看不控（R12：随标签持久——曾放组件 state，切标签重挂载即静默失效，
   * 画面外假锁、键鼠实际通向远端；快照驱动使工具栏/侧栏高亮即时同步）。 */
  readonly viewOnly: boolean;
}

/** 会话快照的只读空态（无激活标签时占位）。 */
export const EMPTY_SNAPSHOT: SessionSnapshot = {
  rootId: "",
  title: null,
  children: [],
  fit: "FIT",
  zoom: 1,
  degraded: false,
  notice: null,
  stats: null,
  ack: null,
  viewOnly: false,
};

/** effectiveDpr = devicePixelRatio / zoom（任务卡步骤 5）；zoom <= 0 视为 1。 */
export function effectiveDpr(deviceDpr: number, zoom: number): number {
  const z = zoom > 0 ? zoom : 1;
  return deviceDpr / z;
}

/**
 * 子窗口的绝对定位偏移（远端物理像素）。
 * T-027 的事件载荷 x/y 已是相对根客户区原点的偏移，直接可用（架构 §3.3）。
 */
export function childOffset(child: Pick<ChildWin, "x" | "y">): {
  x: number;
  y: number;
} {
  return { x: child.x, y: child.y };
}

/** 子窗口条目（来自事件的窗口信息）。 */
function toChild(info: GroupWindowDto): ChildWin {
  return {
    id: info.id,
    title: info.title,
    x: info.x,
    y: info.y,
    w: info.w,
    h: info.h,
    dpi: info.dpi,
  };
}

/**
 * 应用一条组事件，返回新快照（纯函数，不可变；无变化时返回原引用）。
 * - CHILD_ADDED / 子窗口 MOVED：info 原位 upsert（x/y 为相对根客户区偏移）；
 * - CHILD_REMOVED：按 info.id 移除；
 * - MOVED / TITLE 命中根：更新标题；
 * - FIT_DEGRADED：自动切 Scale 并挂提示；
 * - STATS：更新统计；DESTROYED 交由页面级处理，快照不变。
 */
export function applyGroupEvent(
  s: SessionSnapshot,
  ev: GroupEventDto,
): SessionSnapshot {
  const info = ev.info;
  switch (ev.kind) {
    case "CHILD_ADDED":
    case "MOVED":
    case "TITLE": {
      if (info === null) {
        return s;
      }
      if (info.id === s.rootId) {
        return {
          ...s,
          title: info.title.length > 0 ? info.title : s.title,
        };
      }
      if (ev.kind === "TITLE") {
        return s;
      }
      const child = toChild(info);
      const exists = s.children.some((c) => c.id === child.id);
      return {
        ...s,
        children: exists
          ? s.children.map((c) => (c.id === child.id ? child : c))
          : [...s.children, child],
      };
    }    case "CHILD_REMOVED": {
      if (info === null) {
        return s;
      }
      const next = s.children.filter((c) => c.id !== info.id);
      return next.length === s.children.length ? s : { ...s, children: next };
    }
    case "DESTROYED":
      return s;
    case "FIT_DEGRADED":
      return {
        ...s,
        degraded: true,
        fit: "SCALE",
        notice: ev.message ?? "该窗口无法调整尺寸，已切换为本地缩放",
      };
    case "STATS":
      return ev.stats === null ? s : { ...s, stats: ev.stats };
    default:
      return s;
  }
}

/** 每个窗口组一个会话状态；更新为不可变替换后通知订阅者。 */
export class SessionStore {
  private state: SessionSnapshot;
  private readonly listeners = new Set<() => void>();

  constructor(rootId: string) {
    this.state = { ...EMPTY_SNAPSHOT, rootId };
  }

  /** useSyncExternalStore 的 getSnapshot（快照引用在两次更新间保持稳定）。 */
  getSnapshot = (): SessionSnapshot => {
    return this.state;
  };

  /** 订阅变更，返回退订函数。 */
  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  private set(next: SessionSnapshot): void {
    if (next === this.state) {
      return;
    }
    this.state = next;
    for (const listener of this.listeners) {
      listener();
    }
  }

  /** 应用组事件（无变化时不触发通知）。 */
  applyGroupEvent(ev: GroupEventDto): void {
    this.set(applyGroupEvent(this.state, ev));
  }

  /** 记录 open_window ack。 */
  setAck(ack: OpenWindowAckDto): void {
    this.set({ ...this.state, ack });
  }

  /** 切换适配模式；切回 Fit 时清除退化标记允许重试远端适配。 */
  setFit(fit: FitMode): void {
    this.set(
      fit === "FIT"
        ? { ...this.state, fit, degraded: false, notice: null }
        : { ...this.state, fit },
    );
  }

  /** 设置缩放倍率。 */
  setZoom(zoom: number): void {
    this.set({ ...this.state, zoom });
  }

  /** 切换只看不控（随标签持久，快照通知订阅者）。 */
  setViewOnly(viewOnly: boolean): void {
    this.set({ ...this.state, viewOnly });
  }
}

/** React 订阅 hook：返回当前会话快照。 */
export function useSessionSnapshot(store: SessionStore): SessionSnapshot {
  return useSyncExternalStore(store.subscribe, store.getSnapshot);
}

