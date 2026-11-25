// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Lab } from "./Lab";

const { callMock } = vi.hoisted(() => ({ callMock: vi.fn() }));

vi.mock("../api/bridge", () => ({
  call: callMock,
  isTauriRuntime: () => false,
  listenStub: vi.fn().mockResolvedValue(() => undefined),
}));

function mockReply(cmd: string, args?: Record<string, unknown>): unknown {
  switch (cmd) {
    case "lab_get_state":
      return {
        activeDeviceId: "dev-studio",
        activeAddr: "100.101.43.7:21118",
        routeTag: "direct (192.168.1.10:21118)",
        congestion: "bbr",
        mediaFec: true,
        fecOverheadPct: 20,
        fecRecovered: 14,
        fecLost: 0,
        rttMs: 12,
        quicCwndKb: 256,
        quicSentPkts: 18420,
        quicLostPkts: 23,
        encoderName: "h264_nvenc",
        hardwareEnc: true,
        is444: false,
        targetKbps: 8000,
        videoKbps: 4200,
        tileKbps: 380,
        fpsCap: 60,
        tileStillMs: 300,
        tileBudgetPct: 50,
        tilesSent: 128,
        idrCount: 4,
        netem: {
          enabled: false,
          delayMs: 0,
          jitterMs: 0,
          lossPct: 0,
          burstLen: 1,
          rateKbps: 0,
        },
        netemDropped: 0,
        netemDelayed: 0,
        sandboxRunning: true,
        sandboxPsnrDb: 46.8,
        sandboxTextFidelityPct: 99.9,
      };
    case "lab_list_profiles":
      return [
        {
          id: "geek-lan-444",
          name: "⚡ 极客 LAN · 1:1 像素零损",
          boundDeviceId: null,
          transport: { routeMode: "direct", congestion: "bbr", mediaFec: false, fecOverheadPct: 15 },
          codec: {
            manualLock: true,
            bitrateKbps: 20000,
            maxFps: 60,
            preferHw: true,
            request444: true,
            tileEnabled: true,
            tileStillMs: 120,
            tileBudgetPct: 65,
          },
        },
        {
          id: "cross-wan-rs",
          name: "🛡️ 跨网抗丢包 · BBR + RS-FEC 25%",
          boundDeviceId: null,
          transport: { routeMode: "punch", congestion: "bbr", mediaFec: true, fecOverheadPct: 25 },
          codec: {
            manualLock: false,
            bitrateKbps: 8000,
            maxFps: 60,
            preferHw: true,
            request444: false,
            tileEnabled: true,
            tileStillMs: 250,
            tileBudgetPct: 50,
          },
        },
      ];
    case "lab_apply_transport":
      return "已热切至新传输策略";
    case "lab_set_netem":
      return args?.config ?? {
        enabled: true,
        delayMs: 60,
        jitterMs: 12,
        lossPct: 2,
        burstLen: 2,
        rateKbps: 15000,
      };
    case "lab_run_ab_bench":
      return {
        netemSummary: "Netem 开启：+60ms · 丢包 2.0%",
        runA: {
          label: "方案 A",
          encoderName: "h264_nvenc",
          avgFps: 42.0,
          p99FrameMs: 140.0,
          videoKbps: 6000,
          tileKbps: 400,
          fecRecovered: 0,
          framesDropped: 4,
          psnrDb: 38.0,
          textFidelityPct: 91.0,
        },
        runB: {
          label: "方案 B",
          encoderName: "h264_nvenc",
          avgFps: 59.5,
          p99FrameMs: 65.0,
          videoKbps: 6400,
          tileKbps: 480,
          fecRecovered: 11,
          framesDropped: 0,
          psnrDb: 47.2,
          textFidelityPct: 99.8,
        },
        winner: "方案 B",
        verdict: "方案 B 胜出：文字保真度 99.8%",
      };
    default:
      return null;
  }
}

beforeEach(() => {
  callMock.mockReset();
  callMock.mockImplementation((cmd: string, args?: Record<string, unknown>) =>
    Promise.resolve(mockReply(cmd, args)),
  );
});

afterEach(() => {
  cleanup();
});

describe("RD Geek Lab 极客操控台", () => {
  it("默认收起高级调优区；展开后渲染链路实时解剖条、四组硬核旋钮与 1:1 微距像素检视镜", async () => {
    render(<Lab deviceId="dev-studio" />);
    expect(await screen.findByText(/方案实验室 · 真栈操控台/)).toBeTruthy();
    expect(screen.queryByText(/1:1 微距像素检视镜/)).toBeNull();
    fireEvent.click(screen.getByText(/展开高级调优/));
    expect(await screen.findByText(/h264_nvenc/)).toBeTruthy();
    expect(screen.getByText(/1:1 微距像素检视镜/)).toBeTruthy();
  });

  it("拨动旋钮直接调用底层 Tauri IPC (选路/热调/Netem/Panic Reset/A-B对决)", async () => {
    render(<Lab deviceId="dev-studio" />);
    await screen.findByText(/方案实验室 · 真栈操控台/);
    fireEvent.click(screen.getByText(/展开高级调优/));

    // 1) 强制切 STUN 打洞
    fireEvent.click(screen.getByText("STUN 打洞"));
    expect(callMock).toHaveBeenCalledWith(
      "lab_apply_transport",
      expect.objectContaining({
        transport: expect.objectContaining({ routeMode: "punch" }),
      }),
    );

    // 2) 强制发送 IDR 关键帧 & 重刷无损瓦片
    fireEvent.click(screen.getByText(/立即强制发送 IDR 关键帧/));
    expect(callMock).toHaveBeenCalledWith(
      "lab_apply_tuning",
      expect.objectContaining({ forceIdr: true }),
    );

    // 3) 一键恶劣弱网注入
    fireEvent.click(screen.getByText(/跨城晚高峰/));
    expect(callMock).toHaveBeenCalledWith(
      "lab_set_netem",
      expect.objectContaining({
        config: expect.objectContaining({ enabled: true, delayMs: 60 }),
      }),
    );

    // 4) 启动 A/B 真栈对决
    fireEvent.click(screen.getByText(/启动 A\/B 真栈对决/));
    expect(await screen.findByText(/🏆 胜出：方案 B/)).toBeTruthy();

    // 5) Panic Reset 一键逃生舱
    fireEvent.click(screen.getByText(/Panic Reset/));
    expect(callMock).toHaveBeenCalledWith("lab_panic_reset", {
      deviceId: "dev-studio",
    });
  });
});
