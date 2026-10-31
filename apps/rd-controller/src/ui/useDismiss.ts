/**
 * 弹层关闭统一协议（R11 D08）：Esc + 点外关闭。
 *
 * 会话页「新建终端」下拉曾各自为政（点开后点画面/侧栏不关、Esc 无效），
 * 与 ⋯ 菜单、工具栏面板形成三套弹层三种脾气——本 hook 是该协议的唯一出口，
 * 浮层组件一律接入，不再散抄 effect。
 */
import { useEffect } from "react";
import type { RefObject } from "react";

/** open 为 true 时挂 Esc + mousedown 点外监听；关闭或卸载时自动摘除。 */
export function useDismiss(
  open: boolean,
  ref: RefObject<HTMLElement | null>,
  onClose: () => void,
): void {
  useEffect(() => {
    if (!open) {
      return undefined;
    }
    const onDown = (e: MouseEvent): void => {
      const el = ref.current;
      if (el === null || el.contains(e.target as Node)) {
        return;
      }
      onClose();
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open, ref, onClose]);
}
