/**
 * T-038/R2 useUpdater 单例 store 测试（jsdom + mock api/update）：
 * 静默命中 / 节流拦截 / 手动检查→available→ready 全链路 / 错误路径 /
 * H-1 回归锁定：两个消费者共享同一状态（横幅与设置页不分家）。
 * @vitest-environment jsdom
 */
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { type PendingUpdate } from "../api/update";
import { __resetUpdaterStoreForTests, useUpdater } from "./useUpdater";

/* ---- mock ../api/update：默认节流拦截（挂载自检默认不触发） ---- */
vi.mock("../api/update", () => {
  const INTERVAL = 24 * 60 * 60 * 1000;
  return {
    UPDATE_CHECK_INTERVAL_MS: INTERVAL,
    readLastCheck: () => Date.now(), // 默认「刚查过」= 挂载自检被节流拦下
    shouldCheckNow: vi.fn().mockReturnValue(false),
    restartApp: vi.fn().mockResolvedValue(undefined),
    checkForUpdate: vi.fn(),
  };
});
import { checkForUpdate, restartApp, shouldCheckNow } from "../api/update";

function fakePending(): PendingUpdate {
  return {
    info: { version: "9.9.9", notes: "修复若干" },
    install: vi.fn().mockResolvedValue(undefined),
  };
}

/** 放行挂载自检（resetAllMocks 会复原为默认 false）。 */
function allowAutoCheck(): void {
  vi.mocked(shouldCheckNow).mockReturnValue(true);
}

describe("useUpdater（模块级单例）", () => {
  beforeEach(() => {
    __resetUpdaterStoreForTests();
    vi.resetAllMocks();
    vi.mocked(restartApp).mockResolvedValue(undefined);
  });

  it("静默检查命中：挂载即检查并进入 available", async () => {
    allowAutoCheck();
    vi.mocked(checkForUpdate).mockResolvedValue(fakePending());
    const { result } = renderHook(() => useUpdater());
    await waitFor(() => expect(result.current.state.phase).toBe("available"));
    expect(checkForUpdate).toHaveBeenCalledTimes(1);
  });

  it("节流拦截：默认（shouldCheckNow=false）挂载不检查，保持 idle", async () => {
    const { result } = renderHook(() => useUpdater());
    await waitFor(() => {
      expect(checkForUpdate).not.toHaveBeenCalled();
      expect(result.current.state.phase).toBe("idle");
    });
  });

  it("静默检查失败：状态机如实记 error（横幅渲染层吞掉不打扰）", async () => {
    allowAutoCheck();
    vi.mocked(checkForUpdate).mockRejectedValue(new Error("网络不可达"));
    const { result } = renderHook(() => useUpdater());
    await waitFor(() => expect(result.current.state.phase).toBe("error"));
  });

  it("手动检查无更新 → idle", async () => {
    vi.mocked(checkForUpdate).mockResolvedValue(null);
    const { result } = renderHook(() => useUpdater());
    await act(async () => {
      await result.current.checkNow();
    });
    expect(result.current.state.phase).toBe("idle");
  });

  it("手动检查命中 → available → install → ready → restart 调 restartApp", async () => {
    const pending = fakePending();
    vi.mocked(checkForUpdate).mockResolvedValue(pending);
    const { result } = renderHook(() => useUpdater());
    await act(async () => {
      await result.current.checkNow();
    });
    expect(result.current.state.phase).toBe("available");
    await act(async () => {
      await result.current.install();
    });
    expect(result.current.state.phase).toBe("ready");
    await act(async () => {
      result.current.restart();
    });
    expect(restartApp).toHaveBeenCalledTimes(1);
  });

  it("install 失败 → error 且带消息", async () => {
    const pending = fakePending();
    (pending.install as ReturnType<typeof vi.fn>).mockRejectedValue(
      new Error("签名校验失败"),
    );
    vi.mocked(checkForUpdate).mockResolvedValue(pending);
    const { result } = renderHook(() => useUpdater());
    await act(async () => {
      await result.current.checkNow();
    });
    await act(async () => {
      await result.current.install();
    });
    expect(result.current.state.phase).toBe("error");
    expect((result.current.state as { message: string }).message).toContain(
      "签名校验失败",
    );
  });

  it("H-1 回归：两个消费者共享同一状态（A 处检查，B 处可见）", async () => {
    vi.mocked(checkForUpdate).mockResolvedValue(fakePending());
    const a = renderHook(() => useUpdater());
    const b = renderHook(() => useUpdater());
    expect(b.result.current.state.phase).toBe("idle");
    await act(async () => {
      await a.result.current.checkNow();
    });
    expect(b.result.current.state.phase).toBe("available");
    // 静默检查进程级只跑一次：A、B 两次挂载只触发一次 checkForUpdate
    await waitFor(() => expect(checkForUpdate).toHaveBeenCalledTimes(1));
  });
});
