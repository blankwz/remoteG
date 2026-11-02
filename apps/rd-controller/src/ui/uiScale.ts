/**
 * 全局 UI 缩放（P1-4）：字号阶梯全部走 --rd-fs-* 令牌，故实现为根节点
 * CSS 变量 --rd-ui-scale（100%/125%/150%），阶梯定义为
 * calc(px * var(--rd-ui-scale))——切档只改一个变量，即时生效、布局零重排
 * 冲突（相对 px 硬编码处如终端字号不受影响，符合预期）。
 */

const UI_SCALE_KEY = "rd.uiScale";

export const UI_SCALES = [1, 1.25, 1.5] as const;
export type UiScale = (typeof UI_SCALES)[number];

/** 读已保存的缩放档；未保存/值非法回退 100%。 */
export function loadUiScale(): UiScale {
  try {
    const saved = window.localStorage.getItem(UI_SCALE_KEY);
    return UI_SCALES.find((s) => String(s) === saved) ?? 1;
  } catch {
    return 1;
  }
}

/** 应用到根节点（documentElement），所有 --rd-fs-* 令牌即时缩放。 */
export function applyUiScale(scale: UiScale): void {
  document.documentElement.style.setProperty("--rd-ui-scale", String(scale));
}

/** 持久化（存储不可用仅失去记忆，不影响本次会话的生效态）。 */
export function saveUiScale(scale: UiScale): void {
  try {
    window.localStorage.setItem(UI_SCALE_KEY, String(scale));
  } catch {
    // 存储不可用仅失去持久化
  }
}
