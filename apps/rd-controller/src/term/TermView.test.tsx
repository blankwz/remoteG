/**
 * 终端视图单测（T-023）。
 *
 * 环境说明：项目未引入 jsdom / testing-library，测试运行于 vitest 默认
 * node 环境（与 Windows.test.tsx 同策略）。因此本文件只驱动 TermView.tsx
 * 导出的纯逻辑（resize 去抖、resume/字号存取、错误分类），并用 vi.mock
 * 隔离 xterm 渲染层（不真实渲染 WebGL/DOM）；api 层为纯 invoke 封装，
 * 打开/输入/resize 的命令接线由 Rust 侧命令签名保证。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

// 隔离渲染层：node 环境无 DOM，真实 xterm 不能加载
vi.mock("@xterm/xterm", () => ({ Terminal: class {} }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class {} }));
vi.mock("@xterm/addon-webgl", () => ({ WebglAddon: class {} }));
vi.mock("@xterm/addon-unicode11", () => ({ Unicode11Addon: class {} }));

import {
  clampFontSize,
  clearResumeId,
  FONT_SIZE_DEFAULT,
  FONT_SIZE_MAX,
  FONT_SIZE_MIN,
  isDeviceOfflineError,
  isResumeGoneError,
  isTerminalClosedError,
  loadFontSize,
  loadResumeId,
  makeResizeScheduler,
  resumeKey,
  saveFontSize,
  saveResumeId,
  type ResizeDims,
} from "./TermView";
import { DEFAULT_TERM_SHELL, parseShell, TERM_SHELLS } from "../api/term";

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

/** Map 后备的内存 Storage（node 环境无 sessionStorage/localStorage）。 */
function fakeStorage(): Storage {
  const map = new Map<string, string>();
  return {
    getItem: (key: string) => (map.has(key) ? (map.get(key) as string) : null),
    setItem: (key: string, value: string) => {
      map.set(key, String(value));
    },
    removeItem: (key: string) => {
      map.delete(key);
    },
    clear: () => {
      map.clear();
    },
    key: () => null,
    get length() {
      return map.size;
    },
  } as Storage;
}

/** 收集 resize 发送结果的槽。 */
function sender() {
  const sent: ResizeDims[] = [];
  return {
    sent,
    send: (dims: ResizeDims) => {
      sent.push(dims);
    },
  };
}

beforeEach(() => {
  vi.useFakeTimers();
});

// ---------------------------------------------------------------------------
// resize 去抖（任务卡步骤 3）
// ---------------------------------------------------------------------------

describe("makeResizeScheduler", () => {
  it("抖动期内的多次调度只发送最后一次", () => {
    const { sent, send } = sender();
    const scheduler = makeResizeScheduler(send, 100);
    scheduler.schedule({ cols: 80, rows: 24 });
    vi.advanceTimersByTime(50);
    scheduler.schedule({ cols: 120, rows: 40 });
    vi.advanceTimersByTime(50);
    expect(sent).toEqual([]); // 第一次已被取消
    vi.advanceTimersByTime(50);
    expect(sent).toEqual([{ cols: 120, rows: 40 }]);
  });

  it("与上次已发送值相同则跳过", () => {
    const { sent, send } = sender();
    const scheduler = makeResizeScheduler(send, 100);
    scheduler.schedule({ cols: 80, rows: 24 });
    vi.advanceTimersByTime(100);
    expect(sent).toEqual([{ cols: 80, rows: 24 }]);
    // 同尺寸重复调度（如无关的 DOM 布局抖动）不再发送
    scheduler.schedule({ cols: 80, rows: 24 });
    vi.advanceTimersByTime(200);
    expect(sent).toHaveLength(1);
  });

  it("尺寸变化后再次发送", () => {
    const { sent, send } = sender();
    const scheduler = makeResizeScheduler(send, 100);
    scheduler.schedule({ cols: 80, rows: 24 });
    vi.advanceTimersByTime(100);
    scheduler.schedule({ cols: 100, rows: 30 });
    vi.advanceTimersByTime(100);
    expect(sent).toEqual([
      { cols: 80, rows: 24 },
      { cols: 100, rows: 30 },
    ]);
  });

  it("cancel 丢弃未发送的调度", () => {
    const { sent, send } = sender();
    const scheduler = makeResizeScheduler(send, 100);
    scheduler.schedule({ cols: 80, rows: 24 });
    scheduler.cancel();
    vi.advanceTimersByTime(500);
    expect(sent).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 字号存取（任务卡步骤 2）
// ---------------------------------------------------------------------------

describe("fontSize", () => {
  it("clamp 收敛到 [8, 32]，非法值回退默认 14", () => {
    expect(clampFontSize(0)).toBe(FONT_SIZE_MIN);
    expect(clampFontSize(14)).toBe(14);
    expect(clampFontSize(999)).toBe(FONT_SIZE_MAX);
    expect(clampFontSize(Number.NaN)).toBe(FONT_SIZE_DEFAULT);
    expect(FONT_SIZE_DEFAULT).toBe(14);
  });

  it("save/load 往返，坏值与越界回退", () => {
    const storage = fakeStorage();
    expect(loadFontSize(storage)).toBe(FONT_SIZE_DEFAULT); // 未存储 → 默认
    saveFontSize(18, storage);
    expect(loadFontSize(storage)).toBe(18);
    storage.setItem("rd.term.fontSize", "999");
    expect(loadFontSize(storage)).toBe(FONT_SIZE_MAX); // 越界收敛
    storage.setItem("rd.term.fontSize", "abc");
    expect(loadFontSize(storage)).toBe(FONT_SIZE_DEFAULT); // 坏值回退
  });
});

// ---------------------------------------------------------------------------
// resume_id 存取（任务卡步骤 4）
// ---------------------------------------------------------------------------

describe("resumeId", () => {
  it("key 由 deviceId + tab 组成", () => {
    expect(resumeKey("dev-1", "term:pwsh.exe")).toBe(
      "rd.term.resume.dev-1.term:pwsh.exe",
    );
  });

  it("save/load/clear 往返，空值回退 null", () => {
    const storage = fakeStorage();
    expect(loadResumeId("dev-1", "tab-1", storage)).toBeNull();
    saveResumeId("dev-1", "tab-1", "term-abc", storage);
    expect(loadResumeId("dev-1", "tab-1", storage)).toBe("term-abc");
    // 同设备不同标签互不干扰
    expect(loadResumeId("dev-1", "tab-2", storage)).toBeNull();
    clearResumeId("dev-1", "tab-1", storage);
    expect(loadResumeId("dev-1", "tab-1", storage)).toBeNull();
    // 重复清除安全
    expect(() => clearResumeId("dev-1", "tab-1", storage)).not.toThrow();
  });
});

// ---------------------------------------------------------------------------
// 错误分类（任务卡步骤 1/6）
// ---------------------------------------------------------------------------

describe("error classification", () => {
  it("终端流已关闭（进程退出）", () => {
    expect(isTerminalClosedError("终端输入失败：终端流 12 未打开")).toBe(true);
    expect(isTerminalClosedError("设备未连接")).toBe(false);
  });

  it("设备离线", () => {
    expect(isDeviceOfflineError("设备未连接")).toBe(true);
    expect(isDeviceOfflineError("终端输入失败：终端流 12 未打开")).toBe(false);
  });

  it("resume 目标已回收（Agent Error{30}）", () => {
    expect(isResumeGoneError("对端错误 30: 终端会话不存在或已过期")).toBe(true);
    // 新开失败（同码不同文案）不得误判为恢复失败
    expect(isResumeGoneError("对端错误 30: 终端启动失败")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// shell 解析（URL ?term= 消费，任务卡步骤 5）
// ---------------------------------------------------------------------------

describe("parseShell", () => {
  it("接受带/不带 .exe 的白名单名（大小写不敏感）", () => {
    expect(parseShell("pwsh")).toBe("pwsh.exe");
    expect(parseShell(" PWSH.EXE ")).toBe("pwsh.exe");
    expect(parseShell("cmd")).toBe("cmd.exe");
    expect(parseShell("wsl.exe")).toBe("wsl.exe");
    expect(parseShell("powershell")).toBe("powershell.exe");
  });

  it("未知值回退缺省 pwsh.exe", () => {
    expect(parseShell("evil.exe")).toBe(DEFAULT_TERM_SHELL);
    expect(parseShell("")).toBe(DEFAULT_TERM_SHELL);
    expect(DEFAULT_TERM_SHELL).toBe("pwsh.exe");
    expect(TERM_SHELLS).toContain("wsl.exe");
  });
});
