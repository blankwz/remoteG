/**
 * 设置页 P1 批次行为锁定：
 * - P1-2 中继地址保存前格式校验（错误不写入）+ 生效文案写实；
 * - P1-4 全局 UI 缩放（切换即时生效 + 持久化）。
 */
// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Settings, relayAddrError } from "./Settings";

const { callMock } = vi.hoisted(() => ({ callMock: vi.fn() }));

vi.mock("../api/bridge", () => ({
  call: callMock,
  isTauriRuntime: () => true,
  listenStub: vi.fn().mockResolvedValue(() => undefined),
}));

function mockReply(cmd: string): unknown {
  switch (cmd) {
    case "get_media_fec":
      return false;
    case "get_relay_addr":
      return null;
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

describe("relayAddrError 校验规则（P1-2）", () => {
  it.each([
    ["", null], // 空串 = 清除，合法
    ["https://xxx.workers.dev", null],
    ["wss://relay.example.com", null],
    ["http://1.2.3.4:8080", null],
    ["1.2.3.4:21118", null],
    ["1.2.3.4", null], // 省端口，后端补默认 21118
    ["abc", "格式不对"],
    ["1.2.3.4:abc", "格式不对"],
    ["myhost:21118", "格式不对"], // 主机名不带协议：后端 SocketAddr 拒绝
    ["https://", "缺主机名"],
  ])("%j → %j", (input, expectedPrefix) => {
    const err = relayAddrError(input);
    if (expectedPrefix === null) {
      expect(err).toBeNull();
    } else {
      expect(err).toContain(expectedPrefix);
    }
  });
});

describe("中继地址保存（P1-2）", () => {
  it("格式错误：hint 位置出错误提示，且不写入后端", async () => {
    render(<Settings />);
    const input = await screen.findByLabelText("原生中继地址");
    fireEvent.change(input, { target: { value: "随便填的地址" } });
    fireEvent.click(screen.getByText("保存"));
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toContain("格式不对");
    expect(
      callMock.mock.calls.some(([cmd]) => cmd === "set_relay_addr"),
    ).toBe(false);
  });

  it("合法地址：写入后端，成功文案写明重启 rd-controller 生效", async () => {
    render(<Settings />);
    const input = await screen.findByLabelText("原生中继地址");
    fireEvent.change(input, { target: { value: "https://xxx.workers.dev" } });
    fireEvent.click(screen.getByText("保存"));
    await vi.waitFor(() => {
      expect(
        callMock.mock.calls.some(
          ([cmd, args]) => cmd === "set_relay_addr" && args?.relay === "https://xxx.workers.dev",
        ),
      ).toBe(true);
    });
    expect(await screen.findByText(/重启 rd-controller 后生效/)).toBeTruthy();
  });
});

describe("全局 UI 缩放（P1-4）", () => {
  it("切档即时改根节点变量并持久化", async () => {
    render(<Settings />);
    const select = screen.getByLabelText("界面缩放");
    fireEvent.change(select, { target: { value: "1.5" } });
    expect(
      document.documentElement.style.getPropertyValue("--rd-ui-scale"),
    ).toBe("1.5");
    expect(window.localStorage.getItem("rd.uiScale")).toBe("1.5");
    // 换档再验证
    fireEvent.change(select, { target: { value: "1.25" } });
    expect(
      document.documentElement.style.getPropertyValue("--rd-ui-scale"),
    ).toBe("1.25");
  });

  it("挂载时回显已保存档位", async () => {
    window.localStorage.setItem("rd.uiScale", "1.25");
    render(<Settings />);
    const select = screen.getByLabelText("界面缩放") as HTMLSelectElement;
    expect(select.value).toBe("1.25");
  });
});
