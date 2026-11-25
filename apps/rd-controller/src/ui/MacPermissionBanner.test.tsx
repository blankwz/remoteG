// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MacPermissionBanner } from "./MacPermissionBanner";

const { callMock } = vi.hoisted(() => ({ callMock: vi.fn() }));

vi.mock("../api/bridge", () => ({
  call: callMock,
  isTauriRuntime: () => true,
  listenStub: vi.fn().mockResolvedValue(() => undefined),
}));

beforeEach(() => {
  callMock.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("MacPermissionBanner（macOS 隐私与安全性引导）", () => {
  it("非 macOS 系统不展示横幅", async () => {
    callMock.mockResolvedValue({
      isMac: false,
      screenRecording: true,
      accessibility: true,
    });
    const { container } = render(<MacPermissionBanner />);
    await vi.waitFor(() => {
      expect(callMock).toHaveBeenCalledWith("get_mac_permissions");
    });
    expect(container.firstChild).toBeNull();
  });

  it("macOS 缺权限时展示横幅，点击直达对应《隐私与安全性》面板", async () => {
    callMock.mockImplementation((cmd: string) => {
      if (cmd === "get_mac_permissions") {
        return Promise.resolve({
          isMac: true,
          screenRecording: false,
          accessibility: false,
        });
      }
      return Promise.resolve(undefined);
    });

    render(<MacPermissionBanner />);
    expect(await screen.findByText(/系统设置 → 隐私与安全性/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "打开「屏幕录制」设置" }));
    expect(callMock).toHaveBeenCalledWith("open_mac_privacy_settings", { pane: "screen" });

    fireEvent.click(screen.getByRole("button", { name: "打开「辅助功能」设置" }));
    expect(callMock).toHaveBeenCalledWith("open_mac_privacy_settings", { pane: "accessibility" });
  });
});
