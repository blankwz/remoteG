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
    case "lab_p2p_host_open":
      return {
        ok: true,
        token: "rd-p2p://demo-token",
        mappedAddr: "112.80.248.75:54321",
        natType: "cone",
        natLabel: "圆锥型 NAT",
        pairCode: "654321",
        stunUsed: "stun.miwifi.com:3478",
        candidates: ["192.168.1.10:21118"],
        advice: "已锁定公网洞口",
      };
    case "lab_p2p_connect_token":
      return {
        ok: false,
        needReversePunch: true,
        needPairCode: false,
        reverseCode: "rd-punch://112.80.248.99:51234",
        device: null,
        routeTag: "等待双向开洞",
        detail: "需双向开洞",
      };
    case "easytier_detect_status":
      return {
        installed: true,
        binaryPath: "easytier-core",
        enabled: true,
        running: true,
        noTun: false,
        networkName: "rd-mesh-demo",
        virtualIp: "10.126.126.1",
        peerUrls: ["tcp://public.easytier.top:11010"],
        inviteCode: "rd-et://demo-room",
        peers: [{ hostname: "Office-PC", ipv4: "10.126.126.2", cost: "P2P", latencyMs: 16.5 }],
      };
    case "lab_scan_mesh_peers":
      return [
        { source: "EasyTier", name: "Office-PC", addr: "10.126.126.2", latencyMs: 16.5, cost: "P2P" },
      ];
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

  it("智能连接中心覆盖 5 大远程方案家族并支持自定义方案绑定", async () => {
    render(<Lab deviceId="dev-studio" />);
    expect(await screen.findByText("局域网直连")).toBeTruthy();
    expect(screen.getByText("P2P 打洞")).toBeTruthy();
    expect(screen.getByText("自建中转")).toBeTruthy();
    expect(screen.getByText("虚拟组网")).toBeTruthy();
    expect(screen.getByText("轻量/无服务器")).toBeTruthy();

    // 打开自定义方案表单，验证支持 Cloudflare Workers / frp / ZeroTier / DERP 等全部 9 种通道
    fireEvent.click(screen.getByText("+ 自定义方案"));
    const modeSelect = screen.getByLabelText("方案类型") as HTMLSelectElement;
    const optionValues = Array.from(modeSelect.options).map((o) => o.value);
    expect(optionValues).toEqual(
      expect.arrayContaining([
        "direct",
        "punch",
        "relay",
        "easytier",
        "tailscale",
        "zerotier",
        "cf_worker",
        "frp",
        "derp",
      ]),
    );
  });

  it("P2P 打洞向导与虚拟组网向导提供真实可走通的分步交互（生成打洞码/回执码/扫描组网成员直连）", async () => {
    render(<Lab deviceId="dev-studio" />);
    expect(await screen.findByText(/P2P 打洞直连 · 零服务器公共 STUN 实战向导/)).toBeTruthy();

    // 1) 被控端一键生成 STUN 打洞连接码
    fireEvent.click(screen.getByText(/生成本机 STUN 打洞连接码/));
    expect(await screen.findByText("rd-p2p://demo-token")).toBeTruthy();

    // 2) 控制端粘贴打洞码连接，遇到端口受限 NAT 自动给出双向开洞回执码
    const tokenInput = screen.getByLabelText("粘贴对方 STUN 打洞连接码");
    fireEvent.change(tokenInput, { target: { value: "rd-p2p://peer-token" } });
    fireEvent.click(screen.getByText(/一键打洞并进入远程桌面/));
    expect(await screen.findByText("rd-punch://112.80.248.99:51234")).toBeTruthy();

    // 3) 切换到虚拟组网向导，扫描在线组网设备并呈现「连接桌面」按钮
    fireEvent.click(screen.getByText("虚拟组网"));
    expect(await screen.findByText(/虚拟组网直连 · EasyTier 免注册口令组网/)).toBeTruthy();
    fireEvent.click(screen.getByText(/刷新在线组网设备/));
    expect(await screen.findByText("Office-PC")).toBeTruthy();
    expect(screen.getByText("10.126.126.2")).toBeTruthy();
  });
});
