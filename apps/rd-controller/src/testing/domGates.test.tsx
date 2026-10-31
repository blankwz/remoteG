/**
 * DOM 级门禁（R13 无障碍基建债清偿）：jsdom + RTL + axe-core。
 *
 * 门前欠账（无障碍 R12 配额第 3 件）：aria-hidden-focus / nested-interactive
 * 两条 axe 规则与「弹层关闭焦点回触发器」回归断言——node 环境跑不了 DOM，
 * 基建落地后此处永久锁定。
 *
 * 环境：per-file jsdom；api 层经 bridge 桩（同 Windows.test.tsx 策略），
 * @tauri-apps/plugin-opener 单独桩。
 */
// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { axeScan } from "./axeScan";
import { useDismiss } from "../ui/useDismiss";
import { Devices } from "../pages/Devices";
import { Settings } from "../pages/Settings";
import { useState, useRef, useCallback } from "react";

const { callMock } = vi.hoisted(() => ({ callMock: vi.fn() }));

vi.mock("../api/bridge", () => ({
  call: callMock,
  isTauriRuntime: () => true,
  listenStub: vi.fn().mockResolvedValue(() => undefined),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn().mockResolvedValue(undefined),
}));

function mockReply(cmd: string): unknown {
  switch (cmd) {
    case "list_devices":
      return [
        {
          id: "dev-a",
          name: "测试机",
          addr: "100.64.0.1:21118",
          fingerprintHex: "a1".repeat(28),
          addedAt: 0,
          online: true,
        },
      ];
    case "host_status":
      return { listenAddrs: [], peers: [], paused: false };
    case "get_media_fec":
      return false;
    default:
      return true;
  }
}

beforeEach(() => {
  callMock.mockReset();
  callMock.mockImplementation((cmd: string) => Promise.resolve(mockReply(cmd)));
  window.localStorage.clear();
});
afterEach(() => {
  cleanup();
});

// ---------------------------------------------------------------------------
// useDismiss：焦点还原 + menu 键控（弹层后半段契约永久锁定）
// ---------------------------------------------------------------------------

/** 挂载点：触发按钮 + 条件渲染浮层。 */
function DismissHarness({ menu }: { menu?: boolean }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const close = useCallback(() => setOpen(false), []);
  useDismiss(open, ref, close, menu ? { menu: true } : {});
  return (
    <div>
      <button type="button" onClick={() => setOpen(true)}>打开</button>
      {open && (
        <div ref={ref} role="menu">
          <button type="button">甲</button>
          <button type="button" onClick={close}>
            乙
          </button>
          <button type="button">丙</button>
        </div>
      )}
    </div>
  );
}

describe("useDismiss 协议（焦点还原 + menu 键控）", () => {
  it("Esc 关闭后焦点回触发按钮（曾丢 BODY）", () => {
    render(<DismissHarness />);
    const trigger = screen.getByText("打开");
    trigger.focus(); // jsdom 的 fireEvent.click 不移焦，真实浏览器 mousedown 会
    fireEvent.click(trigger);
    expect(screen.getByRole("menu")).toBeTruthy();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(screen.getByText("打开"));
  });

  it("menu 契约：打开聚焦首项，ArrowDown/End 移焦", () => {
    render(<DismissHarness menu />);
    const trigger = screen.getByText("打开");
    trigger.focus();
    fireEvent.click(trigger);
    expect(document.activeElement).toBe(screen.getByText("甲"));
    fireEvent.keyDown(window, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getByText("乙"));
    fireEvent.keyDown(window, { key: "End" });
    expect(document.activeElement).toBe(screen.getByText("丙"));
  });

  it("menu 契约：点菜单项后焦点回触发按钮", () => {
    render(<DismissHarness menu />);
    const trigger = screen.getByText("打开");
    trigger.focus();
    fireEvent.click(trigger);
    fireEvent.click(screen.getByText("乙"));
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(screen.getByText("打开"));
  });
});

// ---------------------------------------------------------------------------
// axe 扫描（真实页面渲染树）：aria-hidden-focus / nested-interactive 钉死
// ---------------------------------------------------------------------------

describe("axe DOM 门禁（真实页面）", () => {
  it("Devices 页：aria-hidden-focus / nested-interactive 零违规", async () => {
    const { container } = render(<Devices />);
    const results = await axeScan(container);
    const bad = results.violations.filter(
      (v) => v.id === "aria-hidden-focus" || v.id === "nested-interactive",
    );
    expect(
      bad.map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(" ")).join("; ")}`),
    ).toEqual([]);
  });

  it("Settings 页：aria-hidden-focus / nested-interactive 零违规", async () => {
    const { container } = render(<Settings />);
    const results = await axeScan(container);
    const bad = results.violations.filter(
      (v) => v.id === "aria-hidden-focus" || v.id === "nested-interactive",
    );
    expect(
      bad.map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(" ")).join("; ")}`),
    ).toEqual([]);
  });

  it("活性验证：种入 aria-hidden 持焦点必须被 axe 抓到", async () => {
    const div = document.createElement("div");
    div.innerHTML =
      '<button type="button">外</button><span aria-hidden="true"><button type="button">内</button></span>';
    document.body.appendChild(div);
    const results = await axeScan(div);
    expect(results.violations.some((v) => v.id === "aria-hidden-focus")).toBe(true);
    div.remove();
  });
});

// ---------------------------------------------------------------------------
// Devices 添加设备对话框：焦点陷阱 + Esc 还原（D07 回归锁定）
// ---------------------------------------------------------------------------

describe("Devices 对话框焦点协议（D07）", () => {
  it("打开→Esc 关闭→焦点还原到「添加设备」触发钮", async () => {
    render(<Devices />);
    const trigger = await screen.findByText("添加设备");
    trigger.focus();
    fireEvent.click(trigger);
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toBeTruthy();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });
});
