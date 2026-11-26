// @vitest-environment jsdom
import { describe, expect, it, vi, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { axeScan } from "../testing/axeScan";
import {
  applyPinchStep,
  clampViewport,
  IDENTITY_VIEWPORT,
  stageToContentPoint,
} from "./viewport";
import {
  applyPointerDelta,
  isDoubleTap,
  loadMobileInputMode,
  saveMobileInputMode,
} from "./trackpad";
import { MobileFloatingBall } from "./MobileFloatingBall";
import { MobileKeyboardBar } from "./MobileKeyboardBar";

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

describe("移动端视口缩放与逆变换引擎 (Moonlight 范式)", () => {
  it("双指捏合保持中点锚定，并约束在 1.0x~4.0x 边界内", () => {
    const next = applyPinchStep(
      IDENTITY_VIEWPORT,
      { x: 100, y: 100 },
      { x: 200, y: 100 },
      { x: 50, y: 100 },
      { x: 250, y: 100 },
      400,
      800,
    );
    expect(next.scale).toBeCloseTo(2.0, 3);
    // 捏合中点 (150, 100) 逆变换回内容坐标仍为 (150, 100)
    const contentPt = stageToContentPoint(150, 100, next);
    expect(contentPt.x).toBeCloseTo(150, 1);
    expect(contentPt.y).toBeCloseTo(100, 1);
  });

  it("缩回 1.0x 时自动复位为单位矩阵，不留偏移", () => {
    const clamped = clampViewport({ scale: 0.8, tx: -120, ty: -80 }, 400, 800);
    expect(clamped).toEqual(IDENTITY_VIEWPORT);
  });
});

describe("移动端双模触控板引擎 (RustDesk 范式)", () => {
  it("慢速微调精准移动，快速甩动自动提速且不越界", () => {
    const slow = applyPointerDelta({ x: 500, y: 500 }, 2, 0, 1920, 1080, 1.5);
    expect(slow.x).toBeCloseTo(503, 1);
    const fast = applyPointerDelta({ x: 500, y: 500 }, 40, 0, 1920, 1080, 1.5);
    expect(fast.x).toBeGreaterThan(560);
    const bounded = applyPointerDelta({ x: 1910, y: 1070 }, 500, 500, 1920, 1080, 2);
    expect(bounded.x).toBe(1919);
    expect(bounded.y).toBe(1079);
  });

  it("双击判定与模式持久化", () => {
    expect(isDoubleTap(1000, 1200, 50, 50, 55, 52)).toBe(true);
    expect(isDoubleTap(1000, 1500, 50, 50, 55, 52)).toBe(false);
    expect(loadMobileInputMode()).toBe("trackpad");
    saveMobileInputMode("direct");
    expect(loadMobileInputMode()).toBe("direct");
  });
});

describe("移动端悬浮球与极客快捷键盘条 UI + 无障碍门禁", () => {
  it("粘滞修饰键 Ctrl + Tab 组合发送后自动释放，且支持 IME 文本直发", async () => {
    const onSendCombo = vi.fn();
    const onSendText = vi.fn();
    const onClose = vi.fn();
    const { container } = render(
      <MobileKeyboardBar
        onSendCombo={onSendCombo}
        onSendText={onSendText}
        onClose={onClose}
      />,
    );

    fireEvent.click(screen.getByText("Ctrl"));
    fireEvent.click(screen.getByText("Tab"));
    expect(onSendCombo).toHaveBeenCalledWith(["Control", "Tab"]);

    // 再次点 Tab 时 Ctrl 已自动释放
    fireEvent.click(screen.getByText("Tab"));
    expect(onSendCombo).toHaveBeenLastCalledWith(["Tab"]);

    // IME 文本直发
    const input = screen.getByLabelText("移动端文本输入");
    fireEvent.change(input, { target: { value: "git status" } });
    fireEvent.click(screen.getByText("发送"));
    expect(onSendText).toHaveBeenCalledWith("git status");

    const results = await axeScan(container);
    const bad = results.violations.filter(
      (v) => v.id === "aria-hidden-focus" || v.id === "nested-interactive",
    );
    expect(bad).toEqual([]);
  });

  it("悬浮球展开后支持一键切换触控模式、键盘条与重置视口且无障碍零违规", async () => {
    const onToggleInputMode = vi.fn();
    const onToggleKeyboard = vi.fn();
    const onResetZoom = vi.fn();
    const { container } = render(
      <MobileFloatingBall
        inputMode="trackpad"
        onToggleInputMode={onToggleInputMode}
        keyboardOpen={false}
        onToggleKeyboard={onToggleKeyboard}
        zoomScale={2.5}
        onResetZoom={onResetZoom}
        quality="auto"
        onCycleQuality={() => undefined}
        onOpenTerm={() => undefined}
        onOpenFiles={() => undefined}
        onBackDevices={() => undefined}
      />,
    );

    fireEvent.pointerDown(screen.getByLabelText("移动端控制悬浮球"), {
      clientX: 360,
      clientY: 500,
    });
    fireEvent.pointerUp(screen.getByLabelText("移动端控制悬浮球"));

    fireEvent.click(screen.getByText(/触控板模式 → 切直触/));
    expect(onToggleInputMode).toHaveBeenCalledTimes(1);

    const results = await axeScan(container);
    const bad = results.violations.filter(
      (v) => v.id === "aria-hidden-focus" || v.id === "nested-interactive",
    );
    expect(bad).toEqual([]);
  });
});
