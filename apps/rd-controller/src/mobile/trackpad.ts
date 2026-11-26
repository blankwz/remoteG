/**
 * 移动端双模输入引擎（借鉴 RustDesk Mobile Mouse Mode + Direct Touch Mode）。
 *
 * - `trackpad`（触控板模式）：
 *   把手机屏幕变成笔记本高精度触控板，配可见虚拟指针，彻底避免手指遮挡 4K 小字/按钮：
 *   · 单指拖动：带非线性加速的相对光标移动
 *   · 单指轻点：虚拟指针位置左键单击
 *   · 双击：左键双击
 *   · 双指轻点：虚拟指针位置右键菜单
 *   · 双指上下/左右滑动：平滑滚轮滚动
 * - `direct`（直触模式）：
 *   直接映射点击位置并交由 Rust 侧 `rd-touch-translate`（snug 状态机）处理轻点/惯性滚动/长按右键。
 */

export type MobileInputMode = "trackpad" | "direct";

const INPUT_MODE_KEY = "rd.mobile.inputMode";

export function loadMobileInputMode(): MobileInputMode {
  try {
    const saved = window.localStorage.getItem(INPUT_MODE_KEY);
    if (saved === "direct" || saved === "trackpad") return saved;
  } catch {
    // ignore
  }
  // 手机小屏控 4K 桌面默认「触控板模式」，零遮挡且精准（RustDesk / Moonlight 用户共识）
  return "trackpad";
}

export function saveMobileInputMode(mode: MobileInputMode): void {
  try {
    window.localStorage.setItem(INPUT_MODE_KEY, mode);
  } catch {
    // ignore
  }
}

export interface TrackpadCursor {
  x: number;
  y: number;
}

/**
 * 指针速度加速度曲线（类似 macOS / RustDesk 触控板）：
 * 慢速微调 1:1 精准命中 1px 像素，快速甩动自动提速跨屏移动。
 */
export function applyPointerDelta(
  cursor: TrackpadCursor,
  dxCss: number,
  dyCss: number,
  remoteW: number,
  remoteH: number,
  sensitivity = 1.6,
): TrackpadCursor {
  const speed = Math.hypot(dxCss, dyCss);
  const accel = speed > 12 ? Math.min(2.6, 1 + (speed - 12) * 0.045) : 1;
  const nx = cursor.x + dxCss * sensitivity * accel;
  const ny = cursor.y + dyCss * sensitivity * accel;
  return {
    x: Math.max(0, Math.min(Math.max(1, remoteW - 1), nx)),
    y: Math.max(0, Math.min(Math.max(1, remoteH - 1), ny)),
  };
}

/** 判断两次轻点是否构成双击（时间 < 280ms 且位移 < 24px）。 */
export function isDoubleTap(
  lastTapMs: number,
  nowMs: number,
  lastX: number,
  lastY: number,
  x: number,
  y: number,
): boolean {
  return nowMs - lastTapMs <= 280 && Math.hypot(x - lastX, y - lastY) <= 24;
}
