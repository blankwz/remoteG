/**
 * 窗口列表页单测（T-020）。
 *
 * 环境说明：项目未引入 jsdom / testing-library（任务卡提及的渲染层依赖
 * 不在本次可写范围内），测试运行于 vitest 默认 node 环境。因此：
 * - 事件增删改与搜索过滤逻辑以纯函数从 Windows.tsx 导出（applyWindowEvent /
 *   filterWindows / windowEventId），本文件直接驱动验证列表正确性；
 * - @tauri-apps/api 的 invoke / Channel 用 vi.mock 打桩，校验 api 层的
 *   命令名、参数与 Channel 接线（onmessage ↔ 回调）。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
  Channel: class {
    onmessage: ((msg: unknown) => void) | null = null;
  },
}));

import { applyWindowEvent, filterWindows, windowEventId } from "./Windows";
import {
  listWindows,
  subscribeWindowEvents,
  type WindowDto,
  type WindowEventDto,
} from "../api/windows";

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

/** 构造 WindowDto（按 id 覆盖，其余字段给默认值）。 */
function win(overrides: Partial<WindowDto> & { id: string }): WindowDto {
  return {
    title: `窗口 ${overrides.id}`,
    process: "explorer.exe",
    pid: 1000 + Number(overrides.id),
    icon: null,
    x: 0,
    y: 0,
    w: 800,
    h: 600,
    dpi: 96,
    resizable: true,
    minimized: false,
    ...overrides,
  };
}

/** 构造 WindowEventDto。 */
function ev(
  kind: WindowEventDto["kind"],
  info: WindowDto | null,
  rootId = "0",
): WindowEventDto {
  return { kind, info, rootId };
}

// ---------------------------------------------------------------------------
// 事件增删改（applyWindowEvent）
// ---------------------------------------------------------------------------

describe("applyWindowEvent", () => {
  const a = win({ id: "1", title: "记事本", process: "notepad.exe" });
  const b = win({ id: "2", title: "浏览器", process: "chrome.exe" });

  it("CREATED 追加新窗口", () => {
    const next = applyWindowEvent([], ev("CREATED", a));
    expect(next).toEqual([a]);
  });

  it("CREATED 已存在同 id 时原位替换且顺序不变", () => {
    const updated = win({ id: "1", title: "记事本 - 改名" });
    const next = applyWindowEvent([a, b], ev("CREATED", updated));
    expect(next).toEqual([updated, b]);
    expect(next).not.toContain(a);
  });

  it("TITLE 事件原位更新标题", () => {
    const next = applyWindowEvent([a, b], ev("TITLE", win({ id: "1", title: "新标题" })));
    expect(next.map((w) => w.title)).toEqual(["新标题", "浏览器"]);
  });

  it("MOVED 事件原位更新（含 minimized 等字段）", () => {
    const next = applyWindowEvent([a], ev("MOVED", win({ id: "1", minimized: true })));
    expect(next).toHaveLength(1);
    expect(next[0]?.minimized).toBe(true);
  });

  it("DESTROYED 移除对应窗口", () => {
    const next = applyWindowEvent([a, b], ev("DESTROYED", a));
    expect(next).toEqual([b]);
  });

  it("DESTROYED info 缺省时按 rootId 移除", () => {
    const next = applyWindowEvent([a, b], ev("DESTROYED", null, "2"));
    expect(next).toEqual([a]);
  });

  it("DESTROYED info 与 rootId 均不可定位时列表不变", () => {
    const next = applyWindowEvent([a], ev("DESTROYED", null, "0"));
    expect(next).toEqual([a]);
  });

  it("CHILD_ADDED / CHILD_REMOVED 不影响顶层列表", () => {
    const child = win({ id: "9", title: "子窗口" });
    expect(applyWindowEvent([a], ev("CHILD_ADDED", child, "1"))).toEqual([a]);
    expect(applyWindowEvent([a], ev("CHILD_REMOVED", child, "1"))).toEqual([a]);
  });

  it("不可变：不修改原数组，返回新数组", () => {
    const original = [a];
    const next = applyWindowEvent(original, ev("CREATED", b));
    expect(original).toEqual([a]);
    expect(next).not.toBe(original);
  });

  it("事件序列：创建 → 改名 → 销毁后列表正确", () => {
    let list: WindowDto[] = [];
    list = applyWindowEvent(list, ev("CREATED", a));
    list = applyWindowEvent(list, ev("CREATED", b));
    list = applyWindowEvent(list, ev("TITLE", win({ id: "2", title: "浏览器 - 文档" })));
    list = applyWindowEvent(list, ev("DESTROYED", a));
    expect(list.map((w) => [w.id, w.title])).toEqual([["2", "浏览器 - 文档"]]);
  });
});

// ---------------------------------------------------------------------------
// 搜索过滤（filterWindows）
// ---------------------------------------------------------------------------

describe("filterWindows", () => {
  const list = [
    win({ id: "1", title: "项目报告.docx - Word", process: "winword.exe" }),
    win({ id: "2", title: "终端", process: "pwsh.exe" }),
    win({ id: "3", title: "WPS 表格", process: "chrome.exe" }),
  ];

  it("空查询（含纯空白）返回全部", () => {
    expect(filterWindows(list, "")).toEqual(list);
    expect(filterWindows(list, "   ")).toEqual(list);
  });

  it("按标题子串匹配且大小写不敏感", () => {
    expect(filterWindows(list, "word").map((w) => w.id)).toEqual(["1"]);
    expect(filterWindows(list, "DOCX").map((w) => w.id)).toEqual(["1"]);
  });

  it("按进程名匹配", () => {
    expect(filterWindows(list, "chrome").map((w) => w.id)).toEqual(["3"]);
  });

  it("无匹配返回空数组", () => {
    expect(filterWindows(list, "不存在")).toEqual([]);
  });

  it("不可变：不改入参", () => {
    filterWindows(list, "终端");
    expect(list).toHaveLength(3);
  });
});

// ---------------------------------------------------------------------------
// windowEventId：事件目标定位
// ---------------------------------------------------------------------------

describe("windowEventId", () => {
  it("优先 info.id", () => {
    expect(windowEventId(ev("TITLE", win({ id: "7" }), "0"))).toBe("7");
  });

  it("info 缺省时回退 rootId", () => {
    expect(windowEventId(ev("DESTROYED", null, "42"))).toBe("42");
  });

  it("info 为零值且 rootId 为 0 时返回 null", () => {
    expect(windowEventId(ev("DESTROYED", win({ id: "0" }), "0"))).toBeNull();
    expect(windowEventId(ev("DESTROYED", null, "0"))).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// api 层：mock @tauri-apps/api 的 invoke / Channel，校验命令名、参数与接线
// ---------------------------------------------------------------------------

describe("api/windows", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("listWindows 以 deviceId 调用 list_windows 并透传结果", async () => {
    const windows = [win({ id: "5", icon: "data:image/png;base64,AQID" })];
    invokeMock.mockResolvedValueOnce(windows);
    await expect(listWindows("dev-1")).resolves.toEqual(windows);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("list_windows", {
      deviceId: "dev-1",
    });
  });

  it("subscribeWindowEvents 传 deviceId 并把回调接到 Channel.onmessage", async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    const onEvent = vi.fn();
    const done = subscribeWindowEvents("dev-1", onEvent);

    const [command, args] = invokeMock.mock.calls[0] as [
      string,
      { deviceId: string; onEvent: { onmessage: unknown } },
    ];
    expect(command).toBe("subscribe_window_events");
    expect(args.deviceId).toBe("dev-1");

    // 模拟 Rust 侧经 Channel 推送一条事件 → onEvent 收到
    const pushed = ev("CREATED", win({ id: "8" }));
    (args.onEvent.onmessage as (m: WindowEventDto) => void)(pushed);
    expect(onEvent).toHaveBeenCalledWith(pushed);

    await expect(done).resolves.toBeUndefined();
  });
});

