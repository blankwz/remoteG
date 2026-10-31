/**
 * 输入映射（T-022）：把 DOM 输入事件换算为协议输入消息的纯逻辑层。
 *
 * 职责（任务卡步骤 2~8）：
 * - 坐标：组容器 CSS 坐标 → 远端窗口物理像素（含 dpr / zoom / Scale 变换）；
 * - 命中：子窗口在上层优先（后加入者层级更高），未命中子窗口回退根窗口；
 * - 滚轮：deltaMode 归一化为 ±120 的 wheel delta（协议单位），符号换算为
 *   Windows 约定（垂直正 = 向上，水平正 = 向右）；shiftKey 转水平；
 * - 键盘：`event.code` → PS/2 Set 1 扫描码（scancodes.ts）；组合期间
 *   （IME）抑制按键转发；组合结束发送 text_input；无组合的可打印字符仍走
 *   扫描码（快捷键与游戏类按键行为一致）；
 * - 失焦：`releaseAll` 抬起全部已按下键，防远端卡键；
 * - 鼠标移动 rAF 合并：每帧最多发送一次（MouseMovePacer）。
 *
 * 全部逻辑与 DOM 解耦（入参为纯数据），node 环境可单测；DOM 接线在
 * WindowGroupView（本卡挂载点）。
 */
import {
  mapCmdToCtrl,
  modifierBits,
  scancodeOf,
  type ScanCode,
} from "./scancodes";

/** 协议滚轮单位（Windows WHEEL_DELTA）。 */
export const WHEEL_DELTA = 120;

/** 组内窗口的命中矩形（远端物理像素；x/y 为相对根客户区原点的偏移）。 */
export interface HitWindow {
  readonly id: string;
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
}

/**
 * 组容器 CSS 坐标 → 根窗口物理像素坐标。
 *
 * 按比例映射（`px / rectCss * rectPhys`）：Fit/Native 下 rectCss =
 * phys / effectiveDpr，严格等价于任务卡步骤 2 的 `offsetX × effectiveDpr`；
 * Scale 模式容器带 transform 缩放，比例式天然吸收该变换，故统一用它。
 * 结果四舍五入（误差 ≤ 0.5 远端像素，满足 DoD「误差 ≤ 1 像素」）。
 */
export function mapGroupPoint(
  cssX: number,
  cssY: number,
  rectCssW: number,
  rectCssH: number,
  rootPhysW: number,
  rootPhysH: number,
): { x: number; y: number } {
  if (rectCssW <= 0 || rectCssH <= 0) {
    return { x: 0, y: 0 };
  }
  return {
    x: Math.round((cssX / rectCssW) * rootPhysW),
    y: Math.round((cssY / rectCssH) * rootPhysH),
  };
}

/**
 * 物理坐标命中的完整记录：子窗口自后向前（后加入者绘制在上）取首个包含
 * 点者；未命中子窗口但落在根窗口内 → 根窗口（偏移 0,0）；界外 → null。
 */
export function hitWindow(
  px: number,
  py: number,
  rootId: string,
  rootW: number,
  rootH: number,
  children: readonly HitWindow[],
): HitWindow | null {
  for (let i = children.length - 1; i >= 0; i -= 1) {
    const c = children[i];
    if (c !== undefined && px >= c.x && py >= c.y && px < c.x + c.w && py < c.y + c.h) {
      return c;
    }
  }
  if (px >= 0 && py >= 0 && px < rootW && py < rootH) {
    return { id: rootId, x: 0, y: 0, w: rootW, h: rootH };
  }
  return null;
}

/** 命中窗口 id（`hitWindow` 的 id 投影）。 */
export function hitTest(
  px: number,
  py: number,
  rootId: string,
  rootW: number,
  rootH: number,
  children: readonly HitWindow[],
): string | null {
  return hitWindow(px, py, rootId, rootW, rootH, children)?.id ?? null;
}

/** deltaMode → 每 DOM 单位折算的 wheel delta 系数。 */
function wheelFactor(deltaMode: number): number {
  // DOM_DELTA_PIXEL：任务卡给定 100px ≈ 120；DOM_DELTA_LINE：约 3 行/格
  // → 40；DOM_DELTA_PAGE：一页按约 10 格（≈30 行）→ 1200。
  if (deltaMode === 1) {
    return 40;
  }
  if (deltaMode === 2) {
    return 1200;
  }
  return 1.2;
}

/**
 * 滚轮归一化（任务卡步骤 4）：deltaMode 折算为 ±120 单位；`shift` 时垂直
 * 转水平；符号换算为 Windows 约定——DOM `deltaY > 0` 为向下滚动而 Windows
 * `MOUSEEVENTF_WHEEL` 正值向上，故**垂直取反**；水平两侧同为右正，不变。
 */
export function normalizeWheel(
  deltaMode: number,
  dx: number,
  dy: number,
  shift: boolean,
): { dx: number; dy: number } {
  const f = wheelFactor(deltaMode);
  let nx = Math.round(dx * f);
  let ny = Math.round(dy * f);
  if (shift) {
    nx += ny;
    ny = 0;
  }
  // 归一 -0（Math.round(-0.x) 产生 -0）
  return { dx: nx === 0 ? 0 : nx, dy: ny === 0 ? 0 : -ny };
}

/**
 * DOM `MouseEvent.button` → 协议 `MouseButton.button`。
 * DOM：0 左 / 1 中 / 2 右 / 3 X1 / 4 X2；协议：0 左 / 1 右 / 2 中 / 3 X1 / 4 X2。
 */
export function mapButton(domButton: number): number {
  if (domButton === 1) {
    return 2;
  }
  if (domButton === 2) {
    return 1;
  }
  return domButton; // 0 / 3 / 4 两边一致
}

// ---------------------------------------------------------------------------
// 键盘 / IME 映射（有状态：组合抑制 + 按下集合跟踪）
// ---------------------------------------------------------------------------

/** 需要下发的键盘事件（协议 KeyEvent）。 */
export interface KeyEventOut {
  scancode: number;
  extended: boolean;
  down: boolean;
  modifiers: number;
}

/** 映射产物去向（由视图接线到 api/input）。 */
export interface InputSink {
  key(k: KeyEventOut): void;
  text(s: string): void;
}

/** DOM 键盘事件的纯数据投影（React/原生 KeyboardEvent 均可映射）。 */
export interface KeyInput {
  code: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  isComposing: boolean;
  /** AltGr（欧陆布局）：Chrome 上报为 Ctrl+Alt 同按——保留组合必须让路。 */
  readonly altGraph?: boolean;
}

/** 键事件处理结果，视图据此决定 preventDefault。 */
export type KeyAction =
  /** 已下发到远端（应 preventDefault 阻止浏览器快捷键/字符插入）。 */
  | "forwarded"
  /** 本应用保留（Ctrl+Shift+H/S：HUD/截图），不转发也不 preventDefault。 */
  | "reserved"
  /** 忽略（未收录的键 / IME 组合中），不转发也不 preventDefault。 */
  | "ignored";

/**
 * 控制端保留快捷键（唯一出口，页面层与本层共用同一份判定）：
 * - Ctrl+Shift+H（HUD）/ S（导出 PNG）/ L（锁屏）/ V（只看切换）
 *   / C（取回剪贴板）/ X（发送剪贴板）
 * - Ctrl+Alt+1..9（切会话标签；用 Ctrl+Alt 而非 Ctrl+digit——后者是远端
 *   应用常用快捷键，不抢）
 * 保留组合不下发远端——否则页面层热键与远端注入各走一次（R11 双入口回归）。
 */
export function isReservedCombo(k: KeyInput): boolean {
  if (k.altGraph) {
    return false; // AltGr+数字是欧陆布局的 @#~ 等字符，不劫持
  }
  if (k.metaKey || k.altKey) {
    // Ctrl+Shift+* 组合不含 Alt；Ctrl+Alt+digit 见下（Shift 必须未按）
    return k.ctrlKey && k.altKey && !k.shiftKey && RESERVED_CTRL_ALT_DIGIT.test(k.code);
  }
  return (
    k.ctrlKey &&
    k.shiftKey &&
    (k.code === "KeyH" ||
      k.code === "KeyS" ||
      k.code === "KeyL" ||
      k.code === "KeyV" ||
      k.code === "KeyC" ||
      k.code === "KeyX")
  );
}

/** Ctrl+Alt+1..9（切标签保留组合）。 */
const RESERVED_CTRL_ALT_DIGIT = /^(Digit|Numpad)[1-9]$/;

/** 键盘 / IME 映射器：跟踪按下集合，失焦时全部抬起防远端卡键。 */
export class InputMapper {
  /** IME 组合中：true 时按键转发被抑制（compositionend 恢复）。 */
  composing = false;
  /** Cmd→Ctrl 映射开关（任务卡步骤 7，localStorage 持久化，视图同步）。 */
  cmdToCtrl: boolean;

  private readonly sink: InputSink;
  /** 已下发按下态的键：event.code → 下发扫描码（keyup 原样抬起）。 */
  private readonly pressed = new Map<string, ScanCode>();

  constructor(sink: InputSink, opts: { cmdToCtrl?: boolean } = {}) {
    this.sink = sink;
    this.cmdToCtrl = opts.cmdToCtrl ?? true;
  }

  /** keydown：组合中忽略；保留组合交还页面；否则查表下发并记录按下态。 */
  handleKeyDown(k: KeyInput): KeyAction {
    if (this.composing || k.isComposing) {
      return "ignored";
    }
    if (isReservedCombo(k)) {
      return "reserved";
    }
    const sc = scancodeOf(mapCmdToCtrl(k.code, this.cmdToCtrl));
    if (sc === null) {
      return "ignored";
    }
    // 自动重复的 keydown：转发（注入侧按键不触发远端系统自动重复，
    // 按住持续输出依赖重复下发），但按下集合只记录一次。
    if (!this.pressed.has(k.code)) {
      this.pressed.set(k.code, sc);
    }
    this.sink.key({
      scancode: sc.sc,
      extended: sc.ext,
      down: true,
      modifiers: modifierBits(k.ctrlKey, k.shiftKey, k.altKey, k.metaKey),
    });
    return "forwarded";
  }

  /** keyup：按下集合命中（或可查表）即下发抬起；组合中忽略。 */
  handleKeyUp(k: KeyInput): KeyAction {
    if (this.composing || k.isComposing) {
      return "ignored";
    }
    const known = this.pressed.get(k.code);
    this.pressed.delete(k.code);
    const sc = known ?? scancodeOf(mapCmdToCtrl(k.code, this.cmdToCtrl));
    if (sc === null) {
      return "ignored";
    }
    this.sink.key({
      scancode: sc.sc,
      extended: sc.ext,
      down: false,
      modifiers: modifierBits(k.ctrlKey, k.shiftKey, k.altKey, k.metaKey),
    });
    return "forwarded";
  }

  /** 组合结束：解除抑制，把最终上屏文本发 text_input（空串不发送）。 */
  handleCompositionEnd(data: string): void {
    this.composing = false;
    if (data.length > 0) {
      this.sink.text(data);
    }
  }

  /** 失焦 / 断连防卡键：把全部已按下键抬起（modifiers 归零）。 */
  releaseAll(): void {
    for (const sc of this.pressed.values()) {
      this.sink.key({ scancode: sc.sc, extended: sc.ext, down: false, modifiers: 0 });
    }
    this.pressed.clear();
  }

  /** 当前记录为按下的键数（测试 / 调试用）。 */
  get pressedCount(): number {
    return this.pressed.size;
  }
}

// ---------------------------------------------------------------------------
// Cmd→Ctrl 设置持久化（任务卡步骤 7：默认开，localStorage）
// ---------------------------------------------------------------------------

/** localStorage 键。 */
export const CMD_TO_CTRL_KEY = "rd.cmdToCtrl";

/** 读取设置；仅显式 "0" 为关，缺省或值非法时为开（默认开）。可注入以便单测。 */
export function loadCmdToCtrl(
  storage: Pick<Storage, "getItem"> = window.localStorage,
): boolean {
  try {
    return storage.getItem(CMD_TO_CTRL_KEY) !== "0";
  } catch {
    return true;
  }
}

/** 保存设置（storage 可注入以便单测；异常静默——设置不致命）。 */
export function saveCmdToCtrl(
  value: boolean,
  storage: Pick<Storage, "setItem"> = window.localStorage,
): void {
  try {
    storage.setItem(CMD_TO_CTRL_KEY, value ? "1" : "0");
  } catch {
    // localStorage 不可用时忽略（隐私模式等），本次会话内仍生效
  }
}

// ---------------------------------------------------------------------------
// 鼠标移动 rAF 合并（任务卡步骤 3：每帧最多一次）
// ---------------------------------------------------------------------------

/** 一次待发移动（命中窗口 + 远端物理坐标）。 */
export interface PendingMove {
  windowId: string;
  x: number;
  y: number;
}

/**
 * 默认调度：固定 8ms 定时器（125Hz，2026-10-23 用户反馈鼠标移动发糊/顿挫，
 * rAF 的 60Hz 与显示刷新对齐反而造成注入-视频采样的相位抖动；datagram 单条
 * 21B，125Hz 带宽可忽略）。node 测试注入替身。
 */
const defaultSchedule = (cb: () => void): unknown => setTimeout(cb, 8);

const defaultCancel = (handle: unknown): void => {
  if (typeof cancelAnimationFrame === "function") {
    cancelAnimationFrame(handle as number);
  } else {
    clearTimeout(handle as ReturnType<typeof setTimeout>);
  }
};

/**
 * 移动节流：submit 只保留最新一条；每个动画帧最多发送一次；cancel 丢弃
 * 未发送的挂起（卸载 / 断连），不发送尾帧。
 */
export class MouseMovePacer {
  private pending: PendingMove | null = null;
  private scheduled = false;
  private handle: unknown = null;

  constructor(
    private readonly send: (m: PendingMove) => void,
    private readonly schedule: (cb: () => void) => unknown = defaultSchedule,
    private readonly cancelHandle: (handle: unknown) => void = defaultCancel,
  ) {}

  submit(m: PendingMove): void {
    this.pending = m;
    if (this.scheduled) {
      return;
    }
    this.scheduled = true;
    this.handle = this.schedule(() => {
      this.scheduled = false;
      this.handle = null;
      const p = this.pending;
      this.pending = null;
      if (p !== null) {
        this.send(p);
      }
    });
  }

  /** 丢弃挂起帧并取消已排定的回调（卸载 / 断连时调用）。 */
  cancel(): void {
    this.pending = null;
    if (this.scheduled && this.handle !== null) {
      this.cancelHandle(this.handle);
    }
    this.scheduled = false;
    this.handle = null;
  }
}
