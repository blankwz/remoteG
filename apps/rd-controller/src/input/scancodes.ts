/**
 * 扫描码表（T-022 步骤 5）：`KeyboardEvent.code` → PS/2 Set 1 make code。
 *
 * 约定与 Agent 侧（rd-input-win `KEYEVENTF_SCANCODE` + `KEYEVENTF_EXTENDEDKEY`）
 * 对齐：
 * - `sc` 为基本 make code（不含 0xE0 前缀）；`ext` 为该键在 PS/2 上是否带
 *   0xE0 前缀，Agent 注入时加 extended 标志；
 * - 覆盖 ANSI 104 键 + 方向/编辑键 + 功能键 F1~F24 + 小键盘；
 * - `Pause` 为 E1 序列、`PrintScreen` 为 E0 2A E0 37 双序列，无法用单条
 *   「基本码 + extended」表达完整语义：PrintScreen 按 E0 37 近似（Windows
 *   MapVirtualKey(E0 37) → VK_SNAPSHOT）；Pause 不映射（scancodeOf 返回
 *   null，该键忽略）。
 * - `Meta*`（徽标键 / macOS Cmd）默认映射到徽标键扫描码；Cmd→Ctrl 重映射
 *   见 `mapCmdToCtrl`（任务卡步骤 7）。
 */

/** 单个键的扫描码（PS/2 Set 1）。 */
export interface ScanCode {
  sc: number;
  /** 是否需要 0xE0 前缀（extended 标志）。 */
  ext: boolean;
}

/** KeyEvent.modifiers 位图（协议字段仅作信息参考，Agent 不换算）：Shift。 */
export const MOD_SHIFT = 1;
/** KeyEvent.modifiers 位图：Ctrl。 */
export const MOD_CTRL = 2;
/** KeyEvent.modifiers 位图：Alt。 */
export const MOD_ALT = 4;
/** KeyEvent.modifiers 位图：Meta（Cmd / 徽标键）。 */
export const MOD_META = 8;

/** 由四类修饰键状态合成协议 `modifiers` 位图。 */
export function modifierBits(
  ctrl: boolean,
  shift: boolean,
  alt: boolean,
  meta: boolean,
): number {
  return (
    (ctrl ? MOD_CTRL : 0) |
    (shift ? MOD_SHIFT : 0) |
    (alt ? MOD_ALT : 0) |
    (meta ? MOD_META : 0)
  );
}

function sc(code: number, ext = false): ScanCode {
  return { sc: code, ext };
}

/** `KeyboardEvent.code` → PS/2 Set 1 扫描码表（只读）。 */
const TABLE: Readonly<Record<string, ScanCode>> = {
  // ---- 字母（QWERTY 顺序）----
  KeyA: sc(0x1e),
  KeyB: sc(0x30),
  KeyC: sc(0x2e),
  KeyD: sc(0x20),
  KeyE: sc(0x12),
  KeyF: sc(0x21),
  KeyG: sc(0x22),
  KeyH: sc(0x23),
  KeyI: sc(0x17),
  KeyJ: sc(0x24),
  KeyK: sc(0x25),
  KeyL: sc(0x26),
  KeyM: sc(0x32),
  KeyN: sc(0x31),
  KeyO: sc(0x18),
  KeyP: sc(0x19),
  KeyQ: sc(0x10),
  KeyR: sc(0x13),
  KeyS: sc(0x1f),
  KeyT: sc(0x14),
  KeyU: sc(0x16),
  KeyV: sc(0x2f),
  KeyW: sc(0x11),
  KeyX: sc(0x2d),
  KeyY: sc(0x15),
  KeyZ: sc(0x2c),
  // ---- 数字行 ----
  Digit1: sc(0x02),
  Digit2: sc(0x03),
  Digit3: sc(0x04),
  Digit4: sc(0x05),
  Digit5: sc(0x06),
  Digit6: sc(0x07),
  Digit7: sc(0x08),
  Digit8: sc(0x09),
  Digit9: sc(0x0a),
  Digit0: sc(0x0b),
  // ---- 主键区其他键 ----
  Minus: sc(0x0c),
  Equal: sc(0x0d),
  Backspace: sc(0x0e),
  Tab: sc(0x0f),
  BracketLeft: sc(0x1a),
  BracketRight: sc(0x1b),
  Enter: sc(0x1c),
  ControlLeft: sc(0x1d),
  Semicolon: sc(0x27),
  Quote: sc(0x28),
  Backquote: sc(0x29),
  ShiftLeft: sc(0x2a),
  Backslash: sc(0x2b),
  Comma: sc(0x33),
  Period: sc(0x34),
  Slash: sc(0x35),
  ShiftRight: sc(0x36),
  Space: sc(0x39),
  CapsLock: sc(0x3a),
  // ---- 功能键 ----
  F1: sc(0x3b),
  F2: sc(0x3c),
  F3: sc(0x3d),
  F4: sc(0x3e),
  F5: sc(0x3f),
  F6: sc(0x40),
  F7: sc(0x41),
  F8: sc(0x42),
  F9: sc(0x43),
  F10: sc(0x44),
  F11: sc(0x57),
  F12: sc(0x58),
  F13: sc(0x64, true),
  F14: sc(0x65, true),
  F15: sc(0x66, true),
  F16: sc(0x68, true),
  F17: sc(0x69, true),
  F18: sc(0x6a, true),
  F19: sc(0x6b, true),
  F20: sc(0x6c, true),
  F21: sc(0x6d, true),
  F22: sc(0x6e, true),
  F23: sc(0x76, true),
  F24: sc(0x77, true),
  // ---- 锁定 / 截屏 ----
  NumLock: sc(0x45),
  ScrollLock: sc(0x46),
  PrintScreen: sc(0x37, true), // E0 37 近似（完整序列 E0 2A E0 37 无法单条表达）
  // ---- 小键盘 ----
  NumpadMultiply: sc(0x37),
  NumpadSubtract: sc(0x4a),
  NumpadAdd: sc(0x4e),
  NumpadDecimal: sc(0x53),
  NumpadEnter: sc(0x1c, true),
  NumpadDivide: sc(0x35, true),
  Numpad0: sc(0x52),
  Numpad1: sc(0x4f),
  Numpad2: sc(0x50),
  Numpad3: sc(0x51),
  Numpad4: sc(0x4b),
  Numpad5: sc(0x4c),
  Numpad6: sc(0x4d),
  Numpad7: sc(0x47),
  Numpad8: sc(0x48),
  Numpad9: sc(0x49),
  // ---- 编辑 / 方向（E0 前缀）----
  Insert: sc(0x52, true),
  Delete: sc(0x53, true),
  Home: sc(0x47, true),
  End: sc(0x4f, true),
  PageUp: sc(0x49, true),
  PageDown: sc(0x51, true),
  ArrowUp: sc(0x48, true),
  ArrowDown: sc(0x50, true),
  ArrowLeft: sc(0x4b, true),
  ArrowRight: sc(0x4d, true),
  // ---- 右侧修饰 / 系统键（E0 前缀）----
  ControlRight: sc(0x1d, true),
  AltRight: sc(0x38, true),
  MetaLeft: sc(0x5b, true),
  MetaRight: sc(0x5c, true),
  ContextMenu: sc(0x5d, true),
  // ---- 非美式布局补充（102 键）----
  IntlBackslash: sc(0x56),
};

/** 查 `event.code` 的扫描码；未收录（如 Pause）返回 null（该键忽略）。 */
export function scancodeOf(code: string): ScanCode | null {
  return TABLE[code] ?? null;
}

/**
 * Cmd→Ctrl 映射（任务卡步骤 7，默认开）：启用时把 `MetaLeft/MetaRight`
 * 重解释为左/右 Ctrl，使 macOS 上 Cmd+C 等组合落到远端 Ctrl+C。
 */
export function mapCmdToCtrl(code: string, enabled: boolean): string {
  if (!enabled) {
    return code;
  }
  if (code === "MetaLeft") {
    return "ControlLeft";
  }
  if (code === "MetaRight") {
    return "ControlRight";
  }
  return code;
}
