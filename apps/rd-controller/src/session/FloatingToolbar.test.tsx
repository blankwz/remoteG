/**
 * 悬浮工具栏 P1 批次行为锁定：
 * - P1-1 「⋯更多」菜单含「快捷键」项，点击触发 onShortcuts（? 键之外
 *   的可见入口）；
 * - P1-3 常用语面板：自定义条目可删、内置 5 条无删除钮，删除与设置页
 *   同一份 localStorage。
 */
// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { FloatingToolbar } from "./FloatingToolbar";
import { PHRASE_DEFAULTS, readCustomPhrases } from "./phrases";

function mountToolbar(overrides: Partial<Parameters<typeof FloatingToolbar>[0]> = {}) {
  const onShortcuts = vi.fn();
  const props = {
    windowActions: true,
    inputLocked: false,
    quality: "auto" as const,
    privacyOn: false,
    recordingOn: false,
    viewOnly: false,
    hudVisible: false,
    onScreenshot: () => undefined,
    onFullscreen: () => undefined,
    onLockScreen: () => undefined,
    onCtrlAltDel: () => undefined,
    onQualityChange: () => undefined,
    onPrivacyToggle: () => undefined,
    onRecordToggle: () => undefined,
    onToggleViewOnly: () => undefined,
    onCopyToRemote: () => undefined,
    onCopyFromRemote: () => undefined,
    onFiles: () => undefined,
    onToggleHud: () => undefined,
    onShortcuts,
    onSendText: () => undefined,
    onSendCombo: () => undefined,
    ...overrides,
  };
  render(<FloatingToolbar {...props} />);
  return { onShortcuts };
}

function openMoreMenu() {
  fireEvent.click(screen.getByText("更多"));
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
});

describe("更多菜单「快捷键」入口（P1-1）", () => {
  it("菜单里有「快捷键」项，点击触发 onShortcuts 并收起菜单", () => {
    const { onShortcuts } = mountToolbar();
    openMoreMenu();
    const item = screen.getByText("快捷键");
    fireEvent.click(item);
    expect(onShortcuts).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("menu", { name: "更多操作" })).toBeNull();
  });
});

describe("常用语面板删除（P1-3）", () => {
  function openPhrasesPanel() {
    openMoreMenu();
    fireEvent.click(screen.getByText("常用语"));
  }

  it("内置 5 条没有删除钮；自定义条目有「删除常用语」钮", () => {
    window.localStorage.setItem(
      "rd-phrases",
      JSON.stringify(["发个测试报告"]),
    );
    mountToolbar();
    openPhrasesPanel();
    for (const d of PHRASE_DEFAULTS) {
      expect(screen.queryByLabelText(`删除常用语「${d}」`)).toBeNull();
    }
    expect(screen.getByLabelText("删除常用语「发个测试报告」")).toBeTruthy();
  });

  it("点删除后条目消失且 localStorage 更新", () => {
    window.localStorage.setItem(
      "rd-phrases",
      JSON.stringify(["甲短语", "乙短语"]),
    );
    mountToolbar();
    openPhrasesPanel();
    fireEvent.click(screen.getByLabelText("删除常用语「甲短语」"));
    expect(screen.queryByLabelText("删除常用语「甲短语」")).toBeNull();
    expect(screen.getByLabelText("删除常用语「乙短语」")).toBeTruthy();
    expect(readCustomPhrases()).toEqual(["乙短语"]);
    expect(window.localStorage.getItem("rd-phrases")).toBe('["乙短语"]');
  });

  it("删除只影响目标条目，面板保持可用（其余条目仍在）", () => {
    window.localStorage.setItem("rd-phrases", JSON.stringify(["唯一条目"]));
    mountToolbar();
    openPhrasesPanel();
    fireEvent.click(screen.getByLabelText("删除常用语「唯一条目」"));
    // 内置条目不受影响
    expect(screen.getByText("你好")).toBeTruthy();
    expect(readCustomPhrases()).toEqual([]);
  });
});

describe("终端标签下画质下拉收口（R17）", () => {
  it("窗口标签（windowActions=true）显示画质下拉", () => {
    mountToolbar({ windowActions: true });
    expect(screen.queryByRole("combobox")).toBeTruthy(); // 画质 select
  });

  it("终端标签（windowActions=false）隐藏画质下拉——终端无视频流，不亮假可用性", () => {
    mountToolbar({ windowActions: false });
    expect(screen.queryByRole("combobox")).toBeNull(); // 画质 select 随 windowActions 收口
    expect(screen.getByText("锁屏")).toBeTruthy();      // 设备级操作仍保留
  });
});
