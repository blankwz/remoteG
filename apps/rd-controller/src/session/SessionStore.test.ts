/**
 * 会话状态单测（T-021）：effectiveDpr 计算、子窗口偏移、组事件应用
 * （子窗口增删改 / 根原点与标题 / Fit 退化 / 统计）、SessionStore 订阅通知
 * 与模式切换（切回 Fit 清除退化标记）。
 */
import { describe, expect, it, vi } from "vitest";
import {
  applyGroupEvent,
  childOffset,
  effectiveDpr,
  EMPTY_SNAPSHOT,
  SessionStore,
  ZOOM_LEVELS,
  type SessionSnapshot,
} from "./SessionStore";
import type { GroupEventDto, GroupWindowDto } from "../api/session";

/** 构造组窗口信息。 */
function win(overrides: Partial<GroupWindowDto> & { id: string }): GroupWindowDto {
  return {
    title: `窗口 ${overrides.id}`,
    process: "notepad.exe",
    pid: 100,
    x: 0,
    y: 0,
    w: 400,
    h: 300,
    dpi: 96,
    resizable: true,
    ...overrides,
  };
}

/** 构造组事件。 */
function ev(
  kind: GroupEventDto["kind"],
  info: GroupWindowDto | null,
  extra?: Partial<Pick<GroupEventDto, "message" | "stats">>,
): GroupEventDto {
  return { kind, info, message: null, stats: null, rootId: "1", ...extra };
}

const BASE: SessionSnapshot = { ...EMPTY_SNAPSHOT, rootId: "1" };

describe("effectiveDpr", () => {
  it("devicePixelRatio / zoom；zoom <= 0 视为 1", () => {
    expect(effectiveDpr(2, 1)).toBe(2);
    expect(effectiveDpr(1.5, 1.5)).toBe(1);
    expect(effectiveDpr(2, 2)).toBe(1);
    expect(effectiveDpr(1.25, 1)).toBe(1.25);
    expect(effectiveDpr(2, 0)).toBe(2);
    expect(effectiveDpr(2, -1)).toBe(2);
  });

  it("zoom 取值即任务卡定义的 100%/150%/200%", () => {
    expect(ZOOM_LEVELS).toEqual([1, 1.5, 2]);
  });
});

describe("childOffset", () => {
  it("T-027 事件的 x/y 已是相对根客户区偏移，直接用于定位", () => {
    expect(childOffset({ x: 300, y: 400 })).toEqual({ x: 300, y: 400 });
    expect(childOffset({ x: 0, y: 0 })).toEqual({ x: 0, y: 0 });
    expect(childOffset({ x: -30, y: -30 })).toEqual({ x: -30, y: -30 });
  });
});

describe("applyGroupEvent", () => {
  it("CHILD_ADDED 追加子窗口；同 id 原位更新（不重复）", () => {
    const a = win({ id: "9", x: 200, y: 150 });
    let s = applyGroupEvent(BASE, ev("CHILD_ADDED", a));
    expect(s.children).toEqual([
      { id: "9", title: "窗口 9", x: 200, y: 150, w: 400, h: 300, dpi: 96 },
    ]);
    s = applyGroupEvent(s, ev("CHILD_ADDED", win({ id: "10" })));
    expect(s.children).toHaveLength(2);
    s = applyGroupEvent(s, ev("MOVED", win({ id: "9", x: 1, y: 2 })));
    expect(s.children.map((c) => c.id)).toEqual(["9", "10"]);
    expect(s.children[0]).toMatchObject({ x: 1, y: 2 });
  });

  it("CHILD_REMOVED 按 id 移除；未知 id 无操作", () => {
    let s = applyGroupEvent(BASE, ev("CHILD_ADDED", win({ id: "9" })));
    s = applyGroupEvent(s, ev("CHILD_REMOVED", win({ id: "9" })));
    expect(s.children).toEqual([]);
    // 未知 id：过滤不掉任何条目 → 返回原引用（无变化）
    const s2 = applyGroupEvent(s, ev("CHILD_REMOVED", win({ id: "99" })));
    expect(s2.children).toEqual([]);
    expect(s2).toBe(s);
  });

  it("根自身的 MOVED/TITLE 更新标题；子窗口 TITLE 无操作", () => {
    const root = win({ id: "1", x: 120, y: 80, title: "记事本" });
    const s = applyGroupEvent(BASE, ev("MOVED", root));
    expect(s.title).toBe("记事本");
    expect(s.children).toEqual([]);

    const s2 = applyGroupEvent(s, ev("TITLE", win({ id: "9", title: "子窗口标题" })));
    expect(s2.children).toEqual([]);
    expect(s2).toBe(s);
  });

  it("标题为空字符串时保留旧标题", () => {
    const s = applyGroupEvent(BASE, ev("TITLE", win({ id: "1", title: "记事本" })));
    const s2 = applyGroupEvent(s, ev("TITLE", win({ id: "1", title: "" })));
    expect(s2.title).toBe("记事本");
  });

  it("FIT_DEGRADED 自动切 Scale、挂退化标记与提示", () => {
    const s = applyGroupEvent(BASE, ev("FIT_DEGRADED", null, { message: "窗口不可调整尺寸" }));
    expect(s.fit).toBe("SCALE");
    expect(s.degraded).toBe(true);
    expect(s.notice).toBe("窗口不可调整尺寸");
    // 无文案时用默认提示
    const s2 = applyGroupEvent(BASE, ev("FIT_DEGRADED", null));
    expect(s2.notice).not.toBeNull();
  });

  it("STATS 更新统计；stats 为 null 时不变", () => {
    const stats = { fps: 58, rttMs: 12, kbps: 7800, decodeMs: 3 };
    const s = applyGroupEvent(BASE, ev("STATS", null, { stats }));
    expect(s.stats).toEqual(stats);
    expect(applyGroupEvent(s, ev("STATS", null))).toBe(s);
  });

  it("DESTROYED 不改快照（页面级处理）", () => {
    expect(applyGroupEvent(BASE, ev("DESTROYED", win({ id: "1" })))).toBe(BASE);
  });

  it("info 缺失的安全事件不变更快照", () => {
    expect(applyGroupEvent(BASE, ev("CHILD_ADDED", null))).toBe(BASE);
    expect(applyGroupEvent(BASE, ev("MOVED", null))).toBe(BASE);
    expect(applyGroupEvent(BASE, ev("CHILD_REMOVED", null))).toBe(BASE);
  });

  it("不可变：输入快照不被修改", () => {
    const before = BASE;
    applyGroupEvent(BASE, ev("CHILD_ADDED", win({ id: "9" })));
    applyGroupEvent(BASE, ev("FIT_DEGRADED", null));
    expect(before).toBe(BASE);
    expect(BASE.children).toEqual([]);
    expect(BASE.fit).toBe("FIT");
  });
});

describe("SessionStore", () => {
  it("setAck 记录 ack；初始快照为空态（含 rootId）", () => {
    const store = new SessionStore("42");
    expect(store.getSnapshot().rootId).toBe("42");
    expect(store.getSnapshot().ack).toBeNull();
    store.setAck({
      windowId: "42",
      level: "L0_PIXEL",
      codec: "H264_420",
      w: 1920,
      h: 1080,
      videoStreamId: 3,
      tileStreamId: 5,
    });
    expect(store.getSnapshot().ack?.w).toBe(1920);
  });

  it("applyGroupEvent 通知订阅者；无变化不通知", () => {
    const store = new SessionStore("1");
    const listener = vi.fn();
    const unsubscribe = store.subscribe(listener);
    store.applyGroupEvent(ev("CHILD_ADDED", win({ id: "9" })));
    expect(listener).toHaveBeenCalledTimes(1);
    store.applyGroupEvent(ev("CHILD_ADDED", null));
    expect(listener).toHaveBeenCalledTimes(1);
    unsubscribe();
    store.applyGroupEvent(ev("CHILD_ADDED", win({ id: "10" })));
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("getSnapshot 引用稳定（两次更新间不变化）", () => {
    const store = new SessionStore("1");
    const a = store.getSnapshot();
    expect(store.getSnapshot()).toBe(a);
    store.setZoom(1.5);
    const b = store.getSnapshot();
    expect(b).not.toBe(a);
    expect(store.getSnapshot()).toBe(b);
  });

  it("setFit 切回 Fit 清除退化标记与提示", () => {
    const store = new SessionStore("1");
    store.applyGroupEvent(ev("FIT_DEGRADED", null));
    expect(store.getSnapshot().fit).toBe("SCALE");
    store.setFit("FIT");
    expect(store.getSnapshot().fit).toBe("FIT");
    expect(store.getSnapshot().degraded).toBe(false);
    expect(store.getSnapshot().notice).toBeNull();
  });

  it("setZoom 更新倍率", () => {
    const store = new SessionStore("1");
    store.setZoom(2);
    expect(store.getSnapshot().zoom).toBe(2);
  });
});
