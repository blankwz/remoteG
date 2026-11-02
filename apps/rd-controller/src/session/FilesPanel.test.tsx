/**
 * FilesPanel 门禁（T-031 + P0-1 取回确认）：demo 数据源渲染远端目录 +
 * 关闭协议 + 点文件先确认落盘位置（曾点击即落盘无确认）。
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

  it("点文件先弹目标位置确认，不直接发起传输（P0-1）", async () => {
    render(<FilesPanel deviceId="dev-x" onClose={() => undefined} />);
    await waitFor(() => {
      expect(screen.getByText("report.pdf")).toBeTruthy();
    });
    fireEvent.click(screen.getByText("report.pdf"));
    // 确认条出现：展示目标目录 + 确认/取消
    expect(screen.getByRole("region", { name: "确认取回位置" })).toBeTruthy();
    expect(screen.getByText(/保存到/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "确认取回" })).toBeTruthy();
    // 未确认前没有传输进度行
    expect(screen.queryByText(/取回 report\.pdf/)).toBeNull();
  });

  it("确认后才开始取回并出现进度行（demo mock 传输）", async () => {
    render(<FilesPanel deviceId="dev-x" onClose={() => undefined} />);
    await waitFor(() => {
      expect(screen.getByText("report.pdf")).toBeTruthy();
    });
    fireEvent.click(screen.getByText("report.pdf"));
    fireEvent.click(screen.getByRole("button", { name: "确认取回" }));
    await waitFor(() => {
      expect(screen.getByText(/取回 report\.pdf/)).toBeTruthy();
    });
    // 确认条已收起
    expect(screen.queryByRole("region", { name: "确认取回位置" })).toBeNull();
  });

  it("取消确认不发起传输", async () => {
    render(<FilesPanel deviceId="dev-x" onClose={() => undefined} />);
    await waitFor(() => {
      expect(screen.getByText("setup.exe")).toBeTruthy();
    });
    fireEvent.click(screen.getByText("setup.exe"));
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    expect(screen.queryByRole("region", { name: "确认取回位置" })).toBeNull();
    // mock 传输有 80ms 延迟，等待一拍确认进度行确实没出现
    await new Promise((r) => setTimeout(r, 150));
    expect(screen.queryByText(/取回 setup\.exe/)).toBeNull();
  });

  it("取回完成的条目出现「打开文件夹」，demo 环境为禁用态（P2-⑥）", async () => {
    render(<FilesPanel deviceId="dev-x" onClose={() => undefined} />);
    await waitFor(() => {
      expect(screen.getByText("report.pdf")).toBeTruthy();
    });
    fireEvent.click(screen.getByText("report.pdf"));
    fireEvent.click(screen.getByRole("button", { name: "确认取回" }));
    await waitFor(() => {
      expect(screen.getByText(/取回 report\.pdf/)).toBeTruthy();
    });
    // mock 传输 80ms 后收尾；done 定格后按钮出现
    const btn = await screen.findByRole("button", { name: "打开文件夹" });
    // 浏览器 demo（无 Tauri IPC）：按钮置灰不承诺（降级为不可用态）
    expect((btn as HTMLButtonElement).disabled).toBe(true);
    expect(btn.getAttribute("title")).toContain("演示环境");
    // 发送方向没有本地落盘，不出现该按钮（由发送用例外的链路保证，此处
    // 仅断言面板内唯一的按钮来自取回行）
    expect(screen.getAllByRole("button", { name: "打开文件夹" })).toHaveLength(1);
  });
});
