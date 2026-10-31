/**
 * 输入映射单测（T-022）：坐标换算（dpr 1/2、zoom 150%）、子窗口命中、
 * 滚轮归一化（含 shift 转水平与 Windows 符号约定）、IME 期间抑制按键、
 * 失焦释放（防卡键）、Cmd→Ctrl 映射与设置持久化、鼠标按钮映射、rAF 合并、
 * api/input 命令接线（invoke 桩）。
 *
 * 运行于 vitest 默认 node 环境：InputMapper 全部为纯逻辑（无 DOM 依赖），
 * rAF 调度注入替身；localStorage 用注入 stub 验证。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

// api 层经 bridge.call 转发：桩成直接走 invokeMock（Tauri 运行时同路径）
vi.mock("../api/bridge", () => ({
  call: invokeMock,
  isTauriRuntime: () => true,
  listenStub: vi.fn().mockResolvedValue(() => undefined),
}));

import {
  hitTest,
  hitWindow,
  InputMapper,
  isReservedCombo,
  loadCmdToCtrl,
  mapButton,
  mapGroupPoint,
  MouseMovePacer,
  normalizeWheel,
  saveCmdToCtrl,
  CMD_TO_CTRL_KEY,
  type KeyEventOut,
  type KeyInput,
} from "./InputMapper";
import { effectiveDpr } from "../session/SessionStore";
import {
  keyEvent,
  mouseButton,
  mouseMove,
  mouseWheel,
  textInput,
} from "../api/input";

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

/** 收集 mapper 产物。 */
function collector() {
  const keys: KeyEventOut[] = [];
  const texts: string[] = [];
  return {
    keys,
    texts,
    sink: {
      key: (k: KeyEventOut) => {
        keys.push(k);
      },
      text: (t: string) => {
        texts.push(t);
      },
    },
  };
}

/** 构造 KeyInput。 */
function key(code: string, extra?: Partial<KeyInput>): KeyInput {
  return {
    code,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    isComposing: false,
    ...extra,
  };
}

/** 内存版 localStorage stub。 */
function fakeStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (k: string) => (map.has(k) ? (map.get(k) as string) : null),
    setItem: (k: string, v: string) => {
      map.set(k, v);
    },
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

// ---------------------------------------------------------------------------
// 坐标换算（任务卡：dpr 1/2，zoom 150%）
// ---------------------------------------------------------------------------

describe("mapGroupPoint 坐标换算", () => {
  it("dpr=1：CSS 坐标 × 1 = 物理坐标", () => {
    // Fit 1:1：rectCss = 800 = phys / effectiveDpr(1, 1)
    const dpr = 1;
    const eff = effectiveDpr(dpr, 1);
    const rectCssW = 800 / eff;
    expect(mapGroupPoint(400, 300, rectCssW, 600 / eff, 800, 600)).toEqual({
      x: 400,
      y: 300,
    });
  });

  it("dpr=2：CSS × 2 = 物理（等价 offsetX × effectiveDpr）", () => {
    const dpr = 2;
    const eff = effectiveDpr(dpr, 1);
    const rectCssW = 800 / eff; // 400
    // offsetX=200（CSS）→ 物理应为 400
    expect(mapGroupPoint(200, 150, rectCssW, 600 / eff, 800, 600)).toEqual({
      x: 400,
      y: 300,
    });
  });

  it("zoom 150%（dpr 1.5）：effectiveDpr = 1，CSS 即物理", () => {
    const eff = effectiveDpr(1.5, 1.5); // 1
    const rectCssW = 900 / eff; // 900
    expect(mapGroupPoint(450, 225, rectCssW, 450 / eff, 900, 450)).toEqual({
      x: 450,
      y: 225,
    });
  });

  it("zoom 150% 且 dpr 2：CSS = 物理 / (2/1.5)，含非整除舍入", () => {
    const eff = effectiveDpr(2, 1.5); // 4/3
    const rectCssW = 800 / eff; // 600
    // CSS 300 → 物理 400 整除；CSS 301 → 401.33 → 401（误差 < 1 物理像素）
    expect(mapGroupPoint(300, 0, rectCssW, 600, 800, 600).x).toBe(400);
    expect(mapGroupPoint(301, 0, rectCssW, 600, 800, 600).x).toBe(401);
  });

  it("Scale 变换（rect 已含缩放）：按比例吸收", () => {
    // 800 物理窗口本地等比缩到 rect 400：CSS 100 → 物理 200
    expect(mapGroupPoint(100, 50, 400, 300, 800, 600)).toEqual({
      x: 200,
      y: 100,
    });
  });

  it("rect 为 0（无帧）退化为原点", () => {
    expect(mapGroupPoint(50, 50, 0, 0, 800, 600)).toEqual({ x: 0, y: 0 });
  });
});

// ---------------------------------------------------------------------------
// 子窗口命中（任务卡：子窗口在上层优先）
// ---------------------------------------------------------------------------

describe("hitTest 子窗口命中", () => {
  const rootId = "root";
  const children = [
    { id: "c1", x: 100, y: 100, w: 200, h: 100 },
    { id: "c2", x: 150, y: 120, w: 200, h: 100 }, // 与 c1 重叠，后加入在上
  ];

  it("命中子窗口矩形", () => {
    expect(hitTest(120, 110, rootId, 800, 600, children)).toBe("c1");
  });

  it("重叠区取后加入者（上层）", () => {
    expect(hitTest(200, 150, rootId, 800, 600, children)).toBe("c2");
  });

  it("未命中子窗口但落在根窗口内 → 根窗口", () => {
    expect(hitTest(50, 50, rootId, 800, 600, children)).toBe("root");
    expect(hitTest(799, 599, rootId, 800, 600, children)).toBe("root");
  });

  it("完全界外 → null（不发送）", () => {
    expect(hitTest(800, 300, rootId, 800, 600, children)).toBeNull();
    expect(hitTest(-1, 10, rootId, 800, 600, children)).toBeNull();
  });

  it("hitWindow 携带窗口偏移：子窗口命中返回子窗口原点，根返回 0,0", () => {
    // 根系坐标 (200,150) 命中 c2（原点 150,120）→ 发送坐标应减去原点
    expect(hitWindow(200, 150, rootId, 800, 600, children)).toEqual({
      id: "c2",
      x: 150,
      y: 120,
      w: 200,
      h: 100,
    });
    expect(hitWindow(50, 50, rootId, 800, 600, children)).toEqual({
      id: rootId,
      x: 0,
      y: 0,
      w: 800,
      h: 600,
    });
    expect(hitWindow(900, 0, rootId, 800, 600, children)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// 滚轮归一化（任务卡：deltaMode 归一化为 120 单位；shift 转水平）
// ---------------------------------------------------------------------------

describe("normalizeWheel 滚轮归一化", () => {
  it("像素模式：100px ≈ 120", () => {
    expect(normalizeWheel(0, 0, 100, false)).toEqual({ dx: 0, dy: -120 });
  });

  it("行模式：3 行 ≈ 一格 120", () => {
    expect(normalizeWheel(1, 0, 3, false)).toEqual({ dx: 0, dy: -120 });
  });

  it("页模式：一页 ≈ 1200", () => {
    expect(normalizeWheel(2, 0, 1, false)).toEqual({ dx: 0, dy: -1200 });
  });

  it("垂直符号取反（DOM 向下为正，Windows 正为向上）", () => {
    // 240px × 1.2 = 288 单位
    expect(normalizeWheel(0, 0, 240, false).dy).toBe(-288);
    expect(normalizeWheel(0, 0, -100, false).dy).toBe(120);
  });

  it("水平符号不变（两侧同为右正）", () => {
    expect(normalizeWheel(0, 100, 0, false).dx).toBe(120);
    expect(normalizeWheel(0, -50, 0, false).dx).toBe(-60);
  });

  it("shift 时垂直转水平（并仍按 Windows 符号换算）", () => {
    // DOM 向下 100px（正）→ 120 单位 → 转水平应向右（Windows 正 = 右）
    expect(normalizeWheel(0, 0, 100, true)).toEqual({ dx: 120, dy: 0 });
  });

  it("微小抖动舍入为 0", () => {
    expect(normalizeWheel(0, 0.2, 0.2, false)).toEqual({ dx: 0, dy: 0 });
  });
});

// ---------------------------------------------------------------------------
// 鼠标按钮映射
// ---------------------------------------------------------------------------

describe("mapButton DOM → 协议", () => {
  it("左 0→0、中 1→2、右 2→1、X1 3→3、X2 4→4", () => {
    expect([0, 1, 2, 3, 4].map(mapButton)).toEqual([0, 2, 1, 3, 4]);
  });
});

// ---------------------------------------------------------------------------
// 键盘：扫描码 / IME 抑制 / 失焦释放
// ---------------------------------------------------------------------------

describe("InputMapper 键盘与 IME", () => {
  it("可打印字符走扫描码（无组合）", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    expect(m.handleKeyDown(key("KeyA"))).toBe("forwarded");
    expect(c.keys).toEqual([
      { scancode: 0x1e, extended: false, down: true, modifiers: 0 },
    ]);
    expect(m.handleKeyUp(key("KeyA"))).toBe("forwarded");
    expect(c.keys[1]).toEqual({
      scancode: 0x1e,
      extended: false,
      down: false,
      modifiers: 0,
    });
    expect(m.pressedCount).toBe(0);
  });

  it("modifiers 位图随事件状态透传", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    m.handleKeyDown(key("KeyP", { ctrlKey: true }));
    expect(c.keys[0]?.modifiers).toBe(2); // MOD_CTRL
    m.handleKeyDown(key("KeyP", { ctrlKey: true, shiftKey: true }));
    expect(c.keys[1]?.modifiers).toBe(3); // CTRL | SHIFT
  });

  it("extended 标志随表传递（方向键 / 小键盘回车）", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    m.handleKeyDown(key("ArrowUp"));
    m.handleKeyDown(key("NumpadEnter"));
    expect(c.keys[0]).toMatchObject({ scancode: 0x48, extended: true });
    expect(c.keys[1]).toMatchObject({ scancode: 0x1c, extended: true });
  });

  it("IME 组合期间抑制按键转发，组合结束发送 text_input", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    m.composing = true;
    expect(m.handleKeyDown(key("KeyN", { isComposing: true }))).toBe("ignored");
    expect(m.handleKeyUp(key("KeyN", { isComposing: true }))).toBe("ignored");
    expect(c.keys).toHaveLength(0);
    expect(m.pressedCount).toBe(0);

    m.handleCompositionEnd("你好");
    expect(m.composing).toBe(false);
    expect(c.texts).toEqual(["你好"]);
    expect(c.keys).toHaveLength(0);

    // 组合结束后按键恢复转发
    expect(m.handleKeyDown(key("KeyA"))).toBe("forwarded");
    expect(c.keys).toHaveLength(1);
  });

  it("空上屏文本不发送 text_input", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    m.handleCompositionEnd("");
    expect(c.texts).toHaveLength(0);
  });

  it("失焦释放全部按下键（防卡键）", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    m.handleKeyDown(key("ControlLeft"));
    m.handleKeyDown(key("ShiftLeft"));
    m.handleKeyDown(key("KeyA"));
    expect(m.pressedCount).toBe(3);
    m.releaseAll();
    expect(m.pressedCount).toBe(0);
    const ups = c.keys.filter((k) => !k.down);
    // ControlLeft=0x1D、ShiftLeft=0x2A、KeyA=0x1E
    expect(ups.map((k) => k.scancode).sort((a, b) => a - b)).toEqual([
      0x1d,
      0x1e,
      0x2a,
    ]);
    for (const k of ups) {
      expect(k.down).toBe(false);
      expect(k.modifiers).toBe(0);
    }
  });

  it("自动重复 keydown 持续转发但只记录一次按下态", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    m.handleKeyDown(key("KeyA"));
    m.handleKeyDown(key("KeyA"));
    expect(c.keys.filter((k) => k.down)).toHaveLength(2);
    expect(m.pressedCount).toBe(1);
  });

  it("未收录的键（Pause）忽略", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    expect(m.handleKeyDown(key("Pause"))).toBe("ignored");
    expect(c.keys).toHaveLength(0);
  });

  it("保留组合（V/L/C/X 与 Ctrl+Alt+digit）不下发远端（R11 泄露回归锁定）", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    expect(m.handleKeyDown(key("KeyV", { ctrlKey: true, shiftKey: true }))).toBe("reserved");
    expect(m.handleKeyDown(key("KeyL", { ctrlKey: true, shiftKey: true }))).toBe("reserved");
    expect(m.handleKeyDown(key("KeyC", { ctrlKey: true, shiftKey: true }))).toBe("reserved");
    expect(m.handleKeyDown(key("KeyX", { ctrlKey: true, shiftKey: true }))).toBe("reserved");
    expect(m.handleKeyDown(key("Digit1", { ctrlKey: true, altKey: true }))).toBe("reserved");
    expect(m.handleKeyDown(key("Digit9", { ctrlKey: true, altKey: true }))).toBe("reserved");
    // 非保留形态必须照常下发：Ctrl+1（无 Alt）是远端应用快捷键，不抢
    expect(m.handleKeyDown(key("Digit1", { ctrlKey: true }))).toBe("forwarded");
    expect(m.handleKeyDown(key("KeyC", { ctrlKey: true }))).toBe("forwarded");
    // 只看保留组合（前面 reserved 的 6 次）未产生任何下发
    expect(c.keys).toHaveLength(2);
  });

  it("Ctrl+Shift+H/S/L/V 为保留组合（不下发远端——R11 双入口回归锁定）", () => {
    expect(isReservedCombo(key("KeyH", { ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isReservedCombo(key("KeyS", { ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isReservedCombo(key("KeyL", { ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isReservedCombo(key("KeyV", { ctrlKey: true, shiftKey: true }))).toBe(true);
    // 不带修饰键的 V/L 是普通按键，必须照常下发远端
    expect(isReservedCombo(key("KeyV", {}))).toBe(false);
    expect(isReservedCombo(key("KeyL", {}))).toBe(false);
    expect(isReservedCombo(key("KeyH", { shiftKey: true }))).toBe(false);
    expect(isReservedCombo(key("KeyH", { ctrlKey: true }))).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Cmd→Ctrl 映射与设置持久化
// ---------------------------------------------------------------------------

describe("Cmd→Ctrl（任务卡步骤 7）", () => {
  it("开启：Meta 映射为 Ctrl 扫描码（左右各自对应）", () => {
    const c = collector();
    const m = new InputMapper(c.sink, { cmdToCtrl: true });
    m.handleKeyDown(key("MetaLeft"));
    m.handleKeyDown(key("MetaRight"));
    expect(c.keys[0]).toMatchObject({ scancode: 0x1d, extended: false });
    expect(c.keys[1]).toMatchObject({ scancode: 0x1d, extended: true });
  });

  it("关闭：Meta 映射为徽标键扫描码", () => {
    const c = collector();
    const m = new InputMapper(c.sink, { cmdToCtrl: false });
    m.handleKeyDown(key("MetaLeft"));
    expect(c.keys[0]).toMatchObject({ scancode: 0x5b, extended: true });
  });

  it("默认开", () => {
    const c = collector();
    const m = new InputMapper(c.sink);
    m.handleKeyDown(key("MetaLeft"));
    expect(c.keys[0]).toMatchObject({ scancode: 0x1d });
  });

  it("设置持久化：默认开，存 '1'/'0'，非法值回退开", () => {
    expect(loadCmdToCtrl(fakeStorage())).toBe(true);
    expect(loadCmdToCtrl(fakeStorage({ [CMD_TO_CTRL_KEY]: "0" }))).toBe(false);
    expect(loadCmdToCtrl(fakeStorage({ [CMD_TO_CTRL_KEY]: "1" }))).toBe(true);
    expect(loadCmdToCtrl(fakeStorage({ [CMD_TO_CTRL_KEY]: "junk" }))).toBe(true);

    const s = fakeStorage();
    saveCmdToCtrl(false, s);
    expect(s.getItem(CMD_TO_CTRL_KEY)).toBe("0");
    saveCmdToCtrl(true, s);
    expect(s.getItem(CMD_TO_CTRL_KEY)).toBe("1");
  });
});

// ---------------------------------------------------------------------------
// 鼠标移动 rAF 合并
// ---------------------------------------------------------------------------

describe("MouseMovePacer rAF 合并", () => {
  it("一帧内的多次 submit 只发送最后一次", () => {
    const sent: string[] = [];
    const holder: { flush: (() => void) | null } = { flush: null };
    const pacer = new MouseMovePacer(
      (m) => {
        sent.push(`${m.windowId}:${m.x},${m.y}`);
      },
      (cb) => {
        holder.flush = cb;
        return cb;
      },
      () => undefined,
    );
    pacer.submit({ windowId: "r", x: 1, y: 1 });
    pacer.submit({ windowId: "r", x: 2, y: 2 });
    pacer.submit({ windowId: "c", x: 3, y: 3 });
    expect(sent).toHaveLength(0);
    holder.flush?.();
    expect(sent).toEqual(["c:3,3"]);
  });

  it("跨帧逐帧发送；cancel 丢弃挂起帧", () => {
    const sent: number[] = [];
    const queue: Array<() => void> = [];
    const pacer = new MouseMovePacer(
      (m) => {
        sent.push(m.x);
      },
      (cb) => {
        queue.push(cb);
        return queue.length - 1;
      },
      () => undefined,
    );
    pacer.submit({ windowId: "r", x: 1, y: 0 });
    pacer.submit({ windowId: "r", x: 2, y: 0 });
    queue.shift()?.();
    pacer.submit({ windowId: "r", x: 3, y: 0 });
    queue.shift()?.();
    expect(sent).toEqual([2, 3]);

    pacer.submit({ windowId: "r", x: 4, y: 0 });
    pacer.cancel();
    while (queue.length > 0) {
      queue.shift()?.();
    }
    expect(sent).toEqual([2, 3]);
  });
});

// ---------------------------------------------------------------------------
// api/input 命令接线
// ---------------------------------------------------------------------------

describe("api/input 命令封装", () => {
  it("mouse_move 等命令名与参数透传", async () => {
    await mouseMove("d1", "7", 12, 34);
    expect(invokeMock).toHaveBeenCalledWith("mouse_move", {
      deviceId: "d1",
      windowId: "7",
      x: 12,
      y: 34,
    });

    await mouseButton("d1", "7", 1, 2, 1, true);
    expect(invokeMock).toHaveBeenLastCalledWith("mouse_button", {
      deviceId: "d1",
      windowId: "7",
      x: 1,
      y: 2,
      button: 1,
      down: true,
    });

    await mouseWheel("d1", "7", 0, 0, -120, 120);
    expect(invokeMock).toHaveBeenLastCalledWith("mouse_wheel", {
      deviceId: "d1",
      windowId: "7",
      x: 0,
      y: 0,
      dx: -120,
      dy: 120,
    });

    await keyEvent("d1", 0x1e, false, true, 2);
    expect(invokeMock).toHaveBeenLastCalledWith("key_event", {
      deviceId: "d1",
      scancode: 0x1e,
      extended: false,
      down: true,
      modifiers: 2,
    });

    await textInput("d1", "你好");
    expect(invokeMock).toHaveBeenLastCalledWith("text_input", {
      deviceId: "d1",
      text: "你好",
    });
  });
});
