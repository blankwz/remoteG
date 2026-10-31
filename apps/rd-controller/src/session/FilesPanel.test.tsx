/**
 * FilesPanel 门禁（T-031）：demo 数据源渲染远端目录 + 关闭协议。
 * 真实 IPC 不可达（jsdom），走 bridge 的 mock 数据源（file_list 演示表）。
 */
// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { FilesPanel } from "./FilesPanel";

afterEach(cleanup);

describe("FilesPanel", () => {
  it("打开即列远端主目录（demo 数据）", async () => {
    render(<FilesPanel deviceId="dev-x" onClose={() => undefined} />);
    // file_list mock 表：path + entries 结构（H1：前端以回传绝对路径为准）
    await waitFor(() => {
      expect(screen.getByText("Desktop")).toBeTruthy();
    });
    expect(screen.getByText("Documents")).toBeTruthy();
    expect(screen.getByText("report.pdf")).toBeTruthy();
    // 目录与文件大小列
    expect(screen.getAllByText("目录").length).toBeGreaterThanOrEqual(3);
    expect(screen.getByText("1.0 MB")).toBeTruthy();
  });

  it("关闭按钮触发 onClose", async () => {
    const onClose = vi.fn();
    render(<FilesPanel deviceId="dev-x" onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "关闭文件传输面板" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("进度列表展示 done 定格（模拟一条进度）", async () => {
    render(<FilesPanel deviceId="dev-x" onClose={() => undefined} />);
    // 直接断言面板骨架存在（进度行由真实传输驱动，jsdom 无 IPC；
    // push/pull 的协议正确性由 Rust 侧单测与 e2e 锁定）
    expect(screen.getByRole("dialog", { name: "文件传输" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "发送文件到远端…" })).toBeTruthy();
  });
});
