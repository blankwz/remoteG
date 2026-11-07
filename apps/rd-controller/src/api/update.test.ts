// @vitest-environment jsdom
/**
 * T-038 更新模块测试：节流纯函数 + 浏览器 no-op + 节流戳读写。
 * （checkForUpdate 的插件交互在 useUpdater.test.tsx 里以 mock 全链路覆盖。）
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  checkForUpdate,
  readLastCheck,
  shouldCheckNow,
  UPDATE_CHECK_INTERVAL_MS,
} from "./update";
import * as bridge from "./bridge";

function mockTauri(on: boolean): void {
  vi.spyOn(bridge, "isTauriRuntime").mockReturnValue(on);
}

describe("shouldCheckNow（24h 节流纯函数）", () => {
  it("从未检查过 → 该检查", () => {
    expect(shouldCheckNow(null, 1_000_000)).toBe(true);
  });
  it("刚查过 → 不查", () => {
    expect(shouldCheckNow(1_000_000, 1_000_000 + 1_000)).toBe(false);
  });
  it("超过 24h → 查", () => {
    expect(
      shouldCheckNow(1_000_000, 1_000_000 + UPDATE_CHECK_INTERVAL_MS),
    ).toBe(true);
  });
  it("差一点没到 24h → 不查（边界含等号）", () => {
    expect(
      shouldCheckNow(1_000_000, 1_000_000 + UPDATE_CHECK_INTERVAL_MS - 1),
    ).toBe(false);
  });
});

describe("checkForUpdate（环境守卫）", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("非 Tauri 环境 → null 且不写节流戳", async () => {
    mockTauri(false);
    await expect(checkForUpdate()).resolves.toBeNull();
    expect(readLastCheck()).toBeNull();
  });

  it("浏览器环境 readLastCheck：无记录/坏记录 → null", () => {
    expect(readLastCheck()).toBeNull();
    window.localStorage.setItem("rd.update.lastCheck", "not-a-number");
    expect(readLastCheck()).toBeNull();
    window.localStorage.setItem("rd.update.lastCheck", "0");
    expect(readLastCheck()).toBeNull();
  });

  it("Tauri 环境写节流戳后可读回", () => {
    mockTauri(true);
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: vi.fn().mockResolvedValue(null),
    }));
    // 动态 import 在模块加载后发生，doMock 对已加载模块仍生效
    return checkForUpdate().then(() => {
      expect(readLastCheck()).not.toBeNull();
    });
  });
});
