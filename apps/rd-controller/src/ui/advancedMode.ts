/**
 * 全局「简洁模式（小白）/ 高级模式（极客）」开关存储（宪法 #2）。
 *
 * 默认关闭（false = 简洁模式）：主界面与设置页只保留高频基础项，零术语干扰；
 * 开启后（true = 高级模式）：解锁自建中继 SSH 一键部署、EasyTier、FEC、方案实验室等底层能力。
 */

export const ADVANCED_MODE_KEY = "rd.advancedMode";
const EVENT_NAME = "rd:advanced-mode-change";

export function loadAdvancedMode(): boolean {
  try {
    return window.localStorage.getItem(ADVANCED_MODE_KEY) === "1";
  } catch {
    return false;
  }
}

export function saveAdvancedMode(enabled: boolean): void {
  try {
    window.localStorage.setItem(ADVANCED_MODE_KEY, enabled ? "1" : "0");
  } catch {
    // 存储不可用时仅在内存态生效
  }
  window.dispatchEvent(new CustomEvent<boolean>(EVENT_NAME, { detail: enabled }));
}

export function subscribeAdvancedMode(cb: (enabled: boolean) => void): () => void {
  const onCustom = (ev: Event) => {
    const ce = ev as CustomEvent<boolean>;
    cb(Boolean(ce.detail));
  };
  const onStorage = (ev: StorageEvent) => {
    if (ev.key === ADVANCED_MODE_KEY) {
      cb(ev.newValue === "1");
    }
  };
  window.addEventListener(EVENT_NAME, onCustom);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(EVENT_NAME, onCustom);
    window.removeEventListener("storage", onStorage);
  };
}
