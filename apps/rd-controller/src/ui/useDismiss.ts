/**
 * 弹层统一协议（R11 D08 → R12 升级）：Esc + 点外关闭 + 焦点还原。
 *
 * R11 只修「能关」；R12 补齐后半段契约（无障碍 R12 Top1/2/5）：
 * - 焦点还原：open 翻真时记录触发元素，关闭/卸载时 focus() 回去——键盘用户
 *   不再落地 BODY；
 * - menu 键控：↑↓ 在浮层内按钮间移焦，Home/End 跳首尾，打开即聚焦首项
 *   （ARIA APG menu 契约——`role="menu"` 不再是空承诺）；
 * - trap：Tab 困在容器内循环（对话框浮层用）。
 */
import { useEffect, useRef } from "react";
import type { RefObject } from "react";

export interface DismissOptions {
  /** 按 menu 契约接管 ↑↓/Home/End 键控与打开聚焦首项。 */
  readonly menu?: boolean;
  /** Tab 困在容器内循环（对话框类浮层）。 */
  readonly trap?: boolean;
  /** menu 键控与初始聚焦的作用域（触发钮与菜单同容器的下拉用：键控只在
   * 菜单项间循环，不含触发钮；省略时沿用容器本身）。 */
  readonly menuScope?: RefObject<HTMLElement | null>;
  /** 额外豁免点击关闭的容器（portal 到 body 的弹出面板：其 DOM 不在主容器
   * 内，点面板内元素会被误判「点外」秒关——把这些面板容器 ref 传进来）。 */
  readonly extraRefs?: Array<RefObject<HTMLElement | null>>;
}

/** 容器内可聚焦元素（菜单项/按钮）。 */
function focusablesIn(el: HTMLElement): HTMLElement[] {
  return Array.from(
    el.querySelectorAll<HTMLElement>(
      'button:not([disabled]), [href], input:not([disabled]), [tabindex]:not([tabindex="-1"])',
    ),
  );
}

/** open 为 true 时挂 Esc + mousedown 点外监听；关闭或卸载时自动摘除并还原焦点。 */
export function useDismiss(
  open: boolean,
  ref: RefObject<HTMLElement | null>,
  onClose: () => void,
  options: DismissOptions = {},
): void {
  const optionsRef = useRef(options);
  optionsRef.current = options;
  useEffect(() => {
    if (!open) {
      return undefined;
    }
    const trigger = document.activeElement as HTMLElement | null;
    const opts = optionsRef.current;

    if (opts.menu) {
      // 打开即聚焦首项（APG：menu 打开后方向键即生效）；作用域优先 menuScope
      const scope = opts.menuScope?.current ?? ref.current;
      if (scope !== null) {
        const items = focusablesIn(scope);
        if (items.length > 0) {
          (items[0] as HTMLElement).focus();
        }
      }
    }

    const onDown = (e: MouseEvent): void => {
      const el = ref.current;
      if (el === null || el.contains(e.target as Node)) {
        return;
      }
      // portal 面板（body 直挂）豁免：点面板内不算「点外」
      const extras = opts.extraRefs ?? [];
      for (const r of extras) {
        const host = r.current;
        if (host !== null && host.contains(e.target as Node)) {
          return;
        }
      }
      onClose();
    };
    const onKey = (e: KeyboardEvent): void => {
      const el = ref.current;
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
        return;
      }
      if (el === null) {
        return;
      }
      if (opts.trap && e.key === "Tab") {
        const items = focusablesIn(el);
        if (items.length === 0) {
          return;
        }
        const first = items[0] as HTMLElement;
        const last = items[items.length - 1] as HTMLElement;
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
        return;
      }
      if (opts.menu && (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "Home" || e.key === "End")) {
        const items = focusablesIn(opts.menuScope?.current ?? el);
        if (items.length === 0) {
          return;
        }
        e.preventDefault();
        const cur = items.indexOf(document.activeElement as HTMLElement);
        let next: number;
        if (e.key === "Home") {
          next = 0;
        } else if (e.key === "End") {
          next = items.length - 1;
        } else if (cur === -1) {
          next = 0;
        } else {
          const delta = e.key === "ArrowDown" ? 1 : -1;
          next = (cur + delta + items.length) % items.length;
        }
        (items[next] as HTMLElement).focus();
      }
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
      // 焦点还原：弹层关闭后回触发元素（曾落地 BODY，键盘用户迷路）
      if (trigger !== null && document.contains(trigger)) {
        trigger.focus();
      }
    };
  }, [open, ref, onClose]);
}
