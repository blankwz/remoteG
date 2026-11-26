/**
 * 移动端低侵入悬浮控制球（借鉴 Scrcpy + RustDesk Mobile）。
 *
 * - 默认贴靠屏幕边缘、半透明不遮挡远程画面；
 * - 支持手指拖动自由停靠左/右侧边缘；
 * - 点开后 1 键直达全部移动端高频动作：触控板/直触切换、软键盘、缩放重置、
 *   虚拟屏分辨率适配、画质切换、原生终端、文件传输与返回设备页。
 */
import { useRef, useState, type PointerEvent } from "react";
import type { Quality } from "../session/FloatingToolbar";
import type { MobileInputMode } from "./trackpad";
import styles from "./MobileFloatingBall.module.css";

interface MobileFloatingBallProps {
  inputMode: MobileInputMode;
  onToggleInputMode: () => void;
  keyboardOpen: boolean;
  onToggleKeyboard: () => void;
  zoomScale: number;
  onResetZoom: () => void;
  quality: Quality;
  onCycleQuality: () => void;
  onOpenTerm: () => void;
  onOpenFiles: () => void;
  onFitMobileDisplay?: () => void;
  onBackDevices: () => void;
}

const QUALITY_TEXT: Record<Quality, string> = {
  auto: "画质:自动",
  high: "画质:高清",
  medium: "画质:均衡",
  low: "画质:流畅",
};

export function MobileFloatingBall(props: MobileFloatingBallProps) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ side: "left" | "right"; topPct: number }>({
    side: "right",
    topPct: 68,
  });
  const dragRef = useRef<{ startX: number; startY: number; moved: boolean } | null>(null);

  const onPointerDown = (e: PointerEvent<HTMLButtonElement>) => {
    dragRef.current = { startX: e.clientX, startY: e.clientY, moved: false };
    e.currentTarget.setPointerCapture?.(e.pointerId);
  };

  const onPointerMove = (e: PointerEvent<HTMLButtonElement>) => {
    const d = dragRef.current;
    if (!d) return;
    if (Math.hypot(e.clientX - d.startX, e.clientY - d.startY) > 10) {
      d.moved = true;
      const h = Math.max(window.innerHeight, 300);
      const topPct = Math.max(12, Math.min(84, (e.clientY / h) * 100));
      const side = e.clientX < window.innerWidth / 2 ? "left" : "right";
      setPos({ side, topPct });
    }
  };

  const onPointerUp = () => {
    const wasDrag = dragRef.current?.moved ?? false;
    dragRef.current = null;
    if (!wasDrag) {
      setOpen((v) => !v);
    }
  };

  return (
    <div
      className={styles.anchor}
      data-side={pos.side}
      style={{ top: `${pos.topPct}%` }}
    >
      {open && (
        <div
          className={styles.menu}
          role="menu"
          aria-label="移动端快捷控制菜单"
        >
          <button
            type="button"
            role="menuitem"
            className={styles.menuBtn}
            onClick={() => {
              props.onToggleInputMode();
              setOpen(false);
            }}
          >
            {props.inputMode === "trackpad" ? "🖱️ 触控板模式 → 切直触" : "👆 直触模式 → 切触控板"}
          </button>
          <button
            type="button"
            role="menuitem"
            className={styles.menuBtn}
            onClick={() => {
              props.onToggleKeyboard();
              setOpen(false);
            }}
          >
            {props.keyboardOpen ? "⌨️ 收起软键盘条" : "⌨️ 呼出键盘与快捷条"}
          </button>
          <button
            type="button"
            role="menuitem"
            className={styles.menuBtn}
            onClick={() => {
              props.onResetZoom();
              setOpen(false);
            }}
          >
            🔍 重置视口（当前 {props.zoomScale.toFixed(1)}×）
          </button>
          {props.onFitMobileDisplay && (
            <button
              type="button"
              role="menuitem"
              className={styles.menuBtn}
              onClick={() => {
                props.onFitMobileDisplay?.();
                setOpen(false);
              }}
            >
              📱 虚拟屏适配手机宽
            </button>
          )}
          <button
            type="button"
            role="menuitem"
            className={styles.menuBtn}
            onClick={() => {
              props.onCycleQuality();
            }}
          >
            ⚡ {QUALITY_TEXT[props.quality]}
          </button>
          <button
            type="button"
            role="menuitem"
            className={styles.menuBtn}
            onClick={() => {
              props.onOpenTerm();
              setOpen(false);
            }}
          >
            💻 原生终端 (PTY)
          </button>
          <button
            type="button"
            role="menuitem"
            className={styles.menuBtn}
            onClick={() => {
              props.onOpenFiles();
              setOpen(false);
            }}
          >
            📁 文件传输
          </button>
          <button
            type="button"
            role="menuitem"
            className={`${styles.menuBtn} ${styles.menuBtnExit}`}
            onClick={() => {
              setOpen(false);
              props.onBackDevices();
            }}
          >
            ← 返回设备列表
          </button>
        </div>
      )}

      <button
        type="button"
        className={open ? `${styles.ball} ${styles.ballOpen}` : styles.ball}
        aria-label="移动端控制悬浮球"
        aria-expanded={open}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
      >
        {open ? "×" : props.inputMode === "trackpad" ? "🖱" : "👆"}
      </button>
    </div>
  );
}
