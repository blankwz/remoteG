/**
 * demo 预览数据源单测（R11 D06 护栏）：未知命令人话报错、命令表形状与
 * 已知调用方期望一致（设备/窗口 DTO 字段）。
 */
import { describe, expect, it } from "vitest";
import { mockInvoke } from "./mock";
import type { DeviceDto } from "./devices";
import type { WindowDto } from "./windows";

describe("mockInvoke（demo 数据源护栏）", () => {
  it("未知命令给人话错误，不抛 __TAURI_INTERNALS__ 黑话（D06）", async () => {
    await expect(mockInvoke("no_such_cmd")).rejects.toThrow(/demo 预览模式/);
    await expect(mockInvoke("open_studio")).rejects.toThrow(/demo 预览模式/);
  });

  it("已知命令返回演示数据（80ms 延迟内）", async () => {
    const devs = await mockInvoke<DeviceDto[]>("list_devices");
    expect(devs.length).toBeGreaterThan(0);
    const wins = await mockInvoke<WindowDto[]>("list_windows");
    expect(wins.length).toBeGreaterThan(0);
  });
});
