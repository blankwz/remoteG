/**
 * 设备页信息架构收敛（P2 批次）行为锁定：
 * - 本机被控状态条两态：空闲=本机待命+一键出码；被控=正在被控制+对方+时长+切断；
 * - 展开层默认收起、localStorage 记忆、aria-live 播报、重挂载回显；
 * - 删除确认 400ms 武装期：进度条视觉提示 + 未就绪禁用，就绪后可删；
 * - 线路测试结果归位设备卡内（成功/失败样式标记 data-failed）。
 */
// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Devices } from "./Devices";

const { callMock } = vi.hoisted(() => ({ callMock: vi.fn() }));

vi.mock("../api/bridge", () => ({
  call: callMock,
  isTauriRuntime: () => true,
  listenStub: vi.fn().mockResolvedValue(() => undefined),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn().mockResolvedValue(undefined),
}));

const device = {
  id: "dev-a",
  name: "测试机",
  addr: "100.64.0.1:21118",
  fingerprintHex: "a1".repeat(28),
  addedAt: 0,
  online: true,
};

/** host_status 回包（用例可覆盖）。 */
let hostReply: {
  listenAddrs: string[];
  peers: { name: string; sinceSecs: number }[];
  paused: boolean;
};

/** tailscale_ping 注入失败（用例可置位）。 */
let pingError: Error | null = null;

function mockReply(cmd: string): unknown {
  switch (cmd) {
    case "list_devices":
      return [device];
    case "host_status":
      return hostReply;
    case "get_media_fec":
      return false;
    case "generate_pair_code":
      return { code: "123456", addrs: ["192.168.1.5:21118"], paused: false };
    case "tailscale_ping":
      if (pingError !== null) {
        return Promise.reject(pingError);
      }
      return "直达 12ms";
    default:
      return true;
  }
}

beforeEach(() => {
  callMock.mockReset();
  callMock.mockImplementation((cmd: string) => Promise.resolve(mockReply(cmd)));
  hostReply = { listenAddrs: ["100.64.0.5:21118"], peers: [], paused: false };
  pingError = null;
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
});

describe("本机被控状态条两态", () => {
  it("空闲态：本机待命 + 生成配对码入口，详情默认收起", async () => {
    render(<Devices />);
    await screen.findByText("本机待命");
    expect(screen.getByText("生成配对码")).toBeTruthy();
    expect(screen.queryByText("切断")).toBeNull();
    // 收敛验证：监听地址/弱网纠错/指纹入口都在展开层里，默认不可见
    expect(screen.queryByLabelText(/弱网纠错/)).toBeNull();
    expect(screen.queryByText(/监听地址/)).toBeNull();
    expect(screen.queryByText(/安全核对/)).toBeNull();
  });

  it("被控态：正在被控制 + 对方与时长徽标 + 红色切断，出码入口隐藏", async () => {
    hostReply = {
      listenAddrs: ["100.64.0.5:21118"],
      peers: [{ name: "甲的电脑", sinceSecs: 130 }],
      paused: false,
    };
    render(<Devices />);
    await screen.findByText("正在被控制");
    expect(screen.getByText("「甲的电脑」· 2 分钟")).toBeTruthy();
    expect(screen.queryByText("生成配对码")).toBeNull();
    fireEvent.click(screen.getByText("切断"));
    await vi.waitFor(() => {
      expect(
        callMock.mock.calls.some(
          ([cmd, args]) => cmd === "set_host_paused" && args?.paused === true,
        ),
      ).toBe(true);
    });
  });
});

describe("展开层记忆与播报", () => {
  it("默认收起；展开后弱网纠错/监听地址可见，aria-live 播报并写入记忆；重挂载回显", async () => {
    render(<Devices />);
    await screen.findByText("本机待命");
    const toggle = screen.getByRole("button", { name: "本机被控详情" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByLabelText(/弱网纠错/)).toBeTruthy();
    expect(screen.getByText(/监听地址/)).toBeTruthy();
    expect(screen.getByText(/安全核对/)).toBeTruthy();
    expect(window.localStorage.getItem("rd.hostDetailsOpen")).toBe("1");
    // aria-live 播报区文本随展开更新
    expect(screen.getByRole("status").textContent).toBe("已展开本机被控详情");

    // 记忆回显：重挂载后直接展开
    cleanup();
    render(<Devices />);
    await screen.findByText("本机待命");
    expect(screen.getByLabelText(/弱网纠错/)).toBeTruthy();

    // 再收起：播报 + 记忆归零
    fireEvent.click(screen.getByRole("button", { name: "本机被控详情" }));
    expect(screen.getByRole("status").textContent).toBe("已收起本机被控详情");
    expect(window.localStorage.getItem("rd.hostDetailsOpen")).toBe("0");
  });
});

describe("删除确认武装期（P2①）", () => {
  it("进度条提示 + 未就绪禁用，0.4s 后自动就绪，点击删除生效", async () => {
    render(<Devices />);
    await screen.findByText("测试机");
    fireEvent.click(screen.getByLabelText("测试机 的更多操作"));
    fireEvent.click(await screen.findByText("删除设备…"));

    const confirmBtn = screen.getByText("确认删除").closest("button") as HTMLButtonElement;
    expect(confirmBtn.dataset.armed).toBe("false");
    expect(confirmBtn.disabled).toBe(true);
    // 武装期视觉提示：进度条元素存在（不只靠 disabled）
    expect(confirmBtn.querySelector("span[aria-hidden='true']")).not.toBeNull();

    await vi.waitFor(() => expect(confirmBtn.dataset.armed).toBe("true"), { timeout: 2000 });
    expect(confirmBtn.disabled).toBe(false);
    expect(confirmBtn.querySelector("span[aria-hidden='true']")).toBeNull();

    fireEvent.click(confirmBtn);
    await vi.waitFor(() => {
      expect(
        callMock.mock.calls.some(
          ([cmd, args]) => cmd === "remove_device" && args?.id === "dev-a",
        ),
      ).toBe(true);
    });
  });
});

describe("线路测试结果块（P2②）", () => {
  it("结果归位到触发它的设备卡内，成功/失败各有样式标记", async () => {
    render(<Devices />);
    await screen.findByText("测试机");

    // 成功态
    fireEvent.click(screen.getByLabelText("测试机 的更多操作"));
    fireEvent.click(await screen.findByText("线路测试"));
    const ok = await screen.findByText(/「测试机」链路：直达/);
    expect(ok.dataset.failed).toBe("false");

    // 失败态（注入 ping 失败，重开菜单再测一次）
    pingError = new Error("DERP 不可达");
    fireEvent.click(screen.getByLabelText("测试机 的更多操作"));
    fireEvent.click(screen.getByText("线路测试"));
    const bad = await screen.findByText(/链路测试没有成功/);
    expect(bad.dataset.failed).toBe("true");
  });
});
