/**
 * 移动端双模触控与双指缩放捕获层（借鉴 RustDesk Mouse Mode + Moonlight Pinch-Zoom）。
 *
 * - `trackpad` 模式：全屏作为高精度笔记本触控板，绘制低延迟 SVG 虚拟指针，
 *   支持单指移动/轻点左键/双击、双指轻点右键、双指捏合缩放(1x~4x)与双指滚轮；
 * - `direct` 模式：单指事件透传给底层 `WindowGroupView`（走 Rust `rd-touch-translate`
 *   snug 状态机），同时仍支持双指捏合缩放。
 */
import { useRef, useState, type TouchEvent } from "react";
import { mouseButton, mouseMove, mouseWheel } from "../api/input";
import {
  applyPointerDelta,
  isDoubleTap,
  type MobileInputMode,
  type TrackpadCursor,
} from "./trackpad";
import {
  applyPinchStep,
  type Point2D,
  type ViewportTransform,
} from "./viewport";

interface MobileTouchOverlayProps {
  deviceId: string;
  windowId: string;
  inputMode: MobileInputMode;
  remoteWidth: number;
  remoteHeight: number;
  viewport: ViewportTransform;
  onViewportChange: (next: ViewportTransform) => void;
  onThreeFingerTap?: () => void;
}

export function MobileTouchOverlay({
  deviceId,
  windowId,
  inputMode,
  remoteWidth,
  remoteHeight,
  viewport,
  onViewportChange,
  onThreeFingerTap,
}: MobileTouchOverlayProps) {
  const wrapRef = useRef<HTMLDivElement>(null);
  const [cursor, setCursor] = useState<TrackpadCursor>(() => ({
    x: Math.round(remoteWidth / 2),
    y: Math.round(remoteHeight / 2),
  }));
  const cursorRef = useRef<TrackpadCursor>(cursor);
  cursorRef.current = cursor;

  const gestureRef = useRef<{
    fingers: number;
    startTime: number;
    startP1: Point2D;
    lastP1: Point2D;
    lastP2: Point2D | null;
    moved: boolean;
    lastTapMs: number;
    lastTapPos: Point2D;
  }>({
    fingers: 0,
    startTime: 0,
    startP1: { x: 0, y: 0 },
    lastP1: { x: 0, y: 0 },
    lastP2: null,
    moved: false,
    lastTapMs: 0,
    lastTapPos: { x: 0, y: 0 },
  });

  const toLocal = (t: { clientX: number; clientY: number }): Point2D => {
    const rect = wrapRef.current?.getBoundingClientRect();
    if (!rect) return { x: t.clientX, y: t.clientY };
    return { x: t.clientX - rect.left, y: t.clientY - rect.top };
  };

  const onTouchStart = (e: TouchEvent<HTMLDivElement>) => {
    const g = gestureRef.current;
    g.fingers = e.touches.length;
    g.startTime = performance.now();
    g.moved = false;
    const t0 = e.touches[0];
    const t1 = e.touches[1];
    if (t0) {
      g.startP1 = toLocal(t0);
      g.lastP1 = g.startP1;
    }
    g.lastP2 = t1 ? toLocal(t1) : null;
  };

  const onTouchMove = (e: TouchEvent<HTMLDivElement>) => {
    const g = gestureRef.current;
    const rect = wrapRef.current?.getBoundingClientRect();
    if (!rect) return;

    if (e.touches.length >= 2 && e.touches[0] && e.touches[1]) {
      e.preventDefault();
      const p1 = toLocal(e.touches[0]);
      const p2 = toLocal(e.touches[1]);
      if (g.lastP2) {
        const prevDist = Math.hypot(g.lastP2.x - g.lastP1.x, g.lastP2.y - g.lastP1.y);
        const nextDist = Math.hypot(p2.x - p1.x, p2.y - p1.y);
        const distDelta = Math.abs(nextDist - prevDist);
        const midDy = (p1.y + p2.y) / 2 - (g.lastP1.y + g.lastP2.y) / 2;
        const midDx = (p1.x + p2.x) / 2 - (g.lastP1.x + g.lastP2.x) / 2;

        // 显著捏合变化 → 双指缩放平移；同向平行双指滑动 → 远端滚轮
        if (distDelta > 2.5 || viewport.scale > 1.02) {
          onViewportChange(
            applyPinchStep(viewport, g.lastP1, g.lastP2, p1, p2, rect.width, rect.height),
          );
        } else if (inputMode === "trackpad" && Math.hypot(midDx, midDy) > 1.5) {
          void mouseWheel(
            deviceId,
            windowId,
            Math.round(cursorRef.current.x),
            Math.round(cursorRef.current.y),
            Math.round(midDx * 3),
            Math.round(midDy * 3),
          ).catch(() => undefined);
        }
        if (Math.hypot(midDx, midDy) > 4 || distDelta > 4) {
          g.moved = true;
        }
      }
      g.lastP1 = p1;
      g.lastP2 = p2;
      return;
    }

    if (inputMode === "trackpad" && e.touches.length === 1 && e.touches[0]) {
      e.preventDefault();
      const p1 = toLocal(e.touches[0]);
      const dx = p1.x - g.lastP1.x;
      const dy = p1.y - g.lastP1.y;
      if (Math.hypot(p1.x - g.startP1.x, p1.y - g.startP1.y) > 5) {
        g.moved = true;
      }
      g.lastP1 = p1;
      const next = applyPointerDelta(
        cursorRef.current,
        dx,
        dy,
        remoteWidth,
        remoteHeight,
      );
      setCursor(next);
      void mouseMove(deviceId, windowId, Math.round(next.x), Math.round(next.y)).catch(
        () => undefined,
      );
    }
  };

  const onTouchEnd = (e: TouchEvent<HTMLDivElement>) => {
    const g = gestureRef.current;
    const elapsed = performance.now() - g.startTime;
    if (e.touches.length === 0 && !g.moved && elapsed < 280) {
      if (g.fingers === 3) {
        onThreeFingerTap?.();
      } else if (inputMode === "trackpad" && g.fingers === 2) {
        // 双指轻点 → 虚拟指针处右键单击
        const cx = Math.round(cursorRef.current.x);
        const cy = Math.round(cursorRef.current.y);
        void mouseButton(deviceId, windowId, cx, cy, 1, true)
          .then(() => mouseButton(deviceId, windowId, cx, cy, 1, false))
          .catch(() => undefined);
      } else if (inputMode === "trackpad" && g.fingers === 1) {
        // 单指轻点 → 虚拟指针处左键单击（自动识别双击）
        const cx = Math.round(cursorRef.current.x);
        const cy = Math.round(cursorRef.current.y);
        const now = performance.now();
        const dbl = isDoubleTap(
          g.lastTapMs,
          now,
          g.lastTapPos.x,
          g.lastTapPos.y,
          g.startP1.x,
          g.startP1.y,
        );
        g.lastTapMs = now;
        g.lastTapPos = g.startP1;
        void mouseButton(deviceId, windowId, cx, cy, 0, true)
          .then(() => mouseButton(deviceId, windowId, cx, cy, 0, false))
          .then(() => {
            if (dbl) {
              return mouseButton(deviceId, windowId, cx, cy, 0, true).then(() =>
                mouseButton(deviceId, windowId, cx, cy, 0, false),
              );
            }
            return undefined;
          })
          .catch(() => undefined);
      }
    }
    if (e.touches.length < 2) {
      g.lastP2 = null;
    }
  };

  const cursorLeftPct = (cursor.x / Math.max(1, remoteWidth)) * 100;
  const cursorTopPct = (cursor.y / Math.max(1, remoteHeight)) * 100;

  return (
    <div
      ref={wrapRef}
      style={{
        position: "absolute",
        inset: 0,
        zIndex: 25,
        touchAction: "none",
        // 直触模式仅拦截双指缩放，单指事件直接透传给底层 WindowGroupView (rd-touch-translate)
        pointerEvents: inputMode === "trackpad" ? "auto" : "none",
      }}
      onTouchStart={onTouchStart}
      onTouchMove={onTouchMove}
      onTouchEnd={onTouchEnd}
      aria-label={inputMode === "trackpad" ? "移动端触控板区域" : "移动端直触区域"}
    >
      {inputMode === "trackpad" && (
        <svg
          width="22"
          height="22"
          viewBox="0 0 24 24"
          aria-hidden="true"
          style={{
            position: "absolute",
            left: `${cursorLeftPct}%`,
            top: `${cursorTopPct}%`,
            pointerEvents: "none",
            filter: "drop-shadow(0 1px 3px rgba(0,0,0,0.85))",
          }}
        >
          <path
            d="M4 2 L4 19 L9 14.5 L12.5 21 L15 19.7 L11.5 13.5 L18 13.5 Z"
            fill="#ffffff"
            stroke="#101216"
            strokeWidth="1.6"
            strokeLinejoin="round"
          />
        </svg>
      )}
    </div>
  );
}
