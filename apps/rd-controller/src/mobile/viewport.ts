/**
 * 移动端视口检测 + 通用双指缩放/平移数学引擎（借鉴 Moonlight / Chrome Remote Desktop）。
 *
 * 解决移动端 6 寸屏操控 4K 桌面的核心痛点：
 * - 在直触模式与触控板模式下均支持 1.0x ~ 4.0x 平滑双指捏合缩放与平移；
 * - 捏合中心始终锚定在双指中点下，不漂移；
 * - 提供精确逆变换 `screenToContentPoint`，放大 3.5x 时点击依然命中准确像素。
 */
import { useEffect, useState } from "react";

export interface ViewportTransform {
  scale: number;
  tx: number;
  ty: number;
}

export const IDENTITY_VIEWPORT: ViewportTransform = { scale: 1, tx: 0, ty: 0 };
export const VIEWPORT_MIN_SCALE = 1;
export const VIEWPORT_MAX_SCALE = 4;

export interface Point2D {
  x: number;
  y: number;
}

/** 检测当前是否运行在移动端形态（Android/iOS UA 或窄屏/粗指针）。 */
export function detectMobileViewport(): boolean {
  if (typeof window === "undefined") return false;
  const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
  if (/Android|iPhone|iPad|iPod/i.test(ua)) return true;
  if (typeof window.matchMedia === "function") {
    if (window.matchMedia("(max-width: 768px)").matches) return true;
    if (
      window.matchMedia("(pointer: coarse)").matches &&
      window.innerWidth <= 1024
    ) {
      return true;
    }
  }
  return window.innerWidth > 0 && window.innerWidth <= 768;
}

/** 响应式监听移动端形态变化（支持横竖屏切换与窗口缩放）。 */
export function useIsMobile(): boolean {
  const [mobile, setMobile] = useState<boolean>(detectMobileViewport);
  useEffect(() => {
    const update = () => setMobile(detectMobileViewport());
    window.addEventListener("resize", update);
    window.addEventListener("orientationchange", update);
    return () => {
      window.removeEventListener("resize", update);
      window.removeEventListener("orientationchange", update);
    };
  }, []);
  return mobile;
}

/** 将缩放与平移约束在舞台边界内，防止画布被拖出可视区域。 */
export function clampViewport(
  v: ViewportTransform,
  stageW: number,
  stageH: number,
): ViewportTransform {
  const scale = Math.max(VIEWPORT_MIN_SCALE, Math.min(VIEWPORT_MAX_SCALE, v.scale));
  if (scale <= 1.001) {
    return IDENTITY_VIEWPORT;
  }
  const minTx = stageW - stageW * scale;
  const minTy = stageH - stageH * scale;
  return {
    scale,
    tx: Math.max(minTx, Math.min(0, v.tx)),
    ty: Math.max(minTy, Math.min(0, v.ty)),
  };
}

/** 计算双指捏合+平移增量后的新视口变换（保持双指中点锚定）。 */
export function applyPinchStep(
  prev: ViewportTransform,
  prevP1: Point2D,
  prevP2: Point2D,
  nextP1: Point2D,
  nextP2: Point2D,
  stageW: number,
  stageH: number,
): ViewportTransform {
  const prevDist = Math.hypot(prevP2.x - prevP1.x, prevP2.y - prevP1.y);
  const nextDist = Math.hypot(nextP2.x - nextP1.x, nextP2.y - nextP1.y);
  const prevMid = { x: (prevP1.x + prevP2.x) / 2, y: (prevP1.y + prevP2.y) / 2 };
  const nextMid = { x: (nextP1.x + nextP2.x) / 2, y: (nextP1.y + nextP2.y) / 2 };

  const factor = prevDist > 4 ? nextDist / prevDist : 1;
  const nextScale = Math.max(
    VIEWPORT_MIN_SCALE,
    Math.min(VIEWPORT_MAX_SCALE, prev.scale * factor),
  );
  const ratio = nextScale / prev.scale;

  // 锚点公式：nextMid = contentPoint * nextScale + nextTx
  const nextTx = nextMid.x - (prevMid.x - prev.tx) * ratio;
  const nextTy = nextMid.y - (prevMid.y - prev.ty) * ratio;

  return clampViewport({ scale: nextScale, tx: nextTx, ty: nextTy }, stageW, stageH);
}

/** 屏幕局部坐标（相对 stage 左上角）逆变换到未缩放画布坐标。 */
export function stageToContentPoint(
  stageX: number,
  stageY: number,
  viewport: ViewportTransform,
): Point2D {
  return {
    x: (stageX - viewport.tx) / viewport.scale,
    y: (stageY - viewport.ty) / viewport.scale,
  };
}
