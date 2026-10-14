/**
 * 终端视图（T-023）：xterm.js 本地渲染远端 shell（L2 语义渲染）。
 *
 * - 挂载即建 Terminal（unicode11 宽字符、webgl 渲染失败回退 DOM）→ 测量
 *   初始尺寸 → openTerminal（sessionStorage 有 resume_id 则恢复，Agent 回放
 *   环形缓冲，历史可见）；输出数据写入 xterm；
 * - FitAddon + ResizeObserver → term_resize（100 ms 去抖，任务卡步骤 3）；
 * - Ctrl+= / Ctrl+- 调字号（localStorage 持久化，8~32，默认 14）；
 * - resume_id 存 sessionStorage（key：deviceId+tab，任务卡步骤 4），重挂载
 *   / 断线重连自动恢复；恢复目标已回收（Agent Error{30}）则清 id 后新开；
 * - 进程退出检测：每 5 s 空载荷存活探测 + 每次写失败分类 —— Agent 关流后
 *   写报“终端流 X 未打开”→ “ [进程已退出]”遮罩 + 重新打开（协议无
 *   TerminalExit 消息，退出码不可得，见上报）。
 *
 * 纯逻辑（字号/resume 存取、resize 去抖、错误分类）从本文件导出，测试在
 * vitest node 环境直接驱动（与 Windows.test.tsx 同策略，不渲染真实 xterm）。
 */
import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import "@xterm/xterm/css/xterm.css";
import { openTerminal, termInput, termResize } from "../api/term";
import styles from "./TermView.module.css";

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/** 字号范围（任务卡步骤 2：默认 14，Ctrl+= / Ctrl+- 调整）。 */
export const FONT_SIZE_MIN = 8;
export const FONT_SIZE_MAX = 32;
export const FONT_SIZE_DEFAULT = 14;
const FONT_STORAGE_KEY = "rd.term.fontSize";

/** term_resize 去抖（任务卡步骤 3）。 */
export const RESIZE_DEBOUNCE_MS = 100;
/** 存活探测周期（进程退出后 Agent 关流，写失败即退出信号）。 */
const EXIT_PROBE_MS = 5000;
/** sessionStorage resume key 前缀（key：rd.term.resume.<deviceId>.<tabId>）。 */
const RESUME_PREFIX = "rd.term.resume.";

// ---------------------------------------------------------------------------
// 纯逻辑（导出供测试）
// ---------------------------------------------------------------------------

/** 字号收敛到 [8, 32]。 */
export function clampFontSize(px: number): number {
  if (!Number.isFinite(px)) {
    return FONT_SIZE_DEFAULT;
  }
  return Math.min(FONT_SIZE_MAX, Math.max(FONT_SIZE_MIN, Math.round(px)));
}

/** 读字号（坏值/越界回退默认 14；storage 注入便于测试）。 */
export function loadFontSize(
  storage: Pick<Storage, "getItem"> = window.localStorage,
): number {
  const raw = storage.getItem(FONT_STORAGE_KEY);
  if (raw === null) {
    return FONT_SIZE_DEFAULT;
  }
  const parsed = Number(raw);
  return clampFontSize(Number.isFinite(parsed) ? parsed : FONT_SIZE_DEFAULT);
}

/** 持久化字号（本卡约定 localStorage）。 */
export function saveFontSize(
  px: number,
  storage: Pick<Storage, "setItem"> = window.localStorage,
): void {
  storage.setItem(FONT_STORAGE_KEY, String(clampFontSize(px)));
}

/** resume 存储 key：deviceId + tab（任务卡步骤 4）。 */
export function resumeKey(deviceId: string, tabId: string): string {
  return `${RESUME_PREFIX}${deviceId}.${tabId}`;
}

/** 读 resume_id（无/空回退 null）。 */
export function loadResumeId(
  deviceId: string,
  tabId: string,
  storage: Pick<Storage, "getItem"> = window.sessionStorage,
): string | null {
  const raw = storage.getItem(resumeKey(deviceId, tabId));
  return raw !== null && raw.length > 0 ? raw : null;
}

/** 保存 resume_id（open_terminal ack 后调用）。 */
export function saveResumeId(
  deviceId: string,
  tabId: string,
  termId: string,
  storage: Pick<Storage, "setItem"> = window.sessionStorage,
): void {
  storage.setItem(resumeKey(deviceId, tabId), termId);
}

/** 清除 resume_id（进程退出 / 恢复目标已回收时调用）。 */
export function clearResumeId(
  deviceId: string,
  tabId: string,
  storage: Pick<Storage, "removeItem"> = window.sessionStorage,
): void {
  storage.removeItem(resumeKey(deviceId, tabId));
}

/** 终端尺寸（cols × rows）。 */
export interface ResizeDims {
  readonly cols: number;
  readonly rows: number;
}

/**
 * resize 去抖调度器（任务卡步骤 3）：抖动期内的多次调度合并为最后一次，
 * 且与上次已发送值相同则跳过（避免静态布局下的重复 term_resize）。
 */
export function makeResizeScheduler(
  send: (dims: ResizeDims) => void,
  delayMs: number = RESIZE_DEBOUNCE_MS,
): { schedule(dims: ResizeDims): void; cancel(): void } {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let last: ResizeDims | null = null;
  return {
    schedule(dims: ResizeDims): void {
      if (timer !== undefined) {
        clearTimeout(timer);
      }
      timer = setTimeout(() => {
        timer = undefined;
        if (last !== null && last.cols === dims.cols && last.rows === dims.rows) {
          return;
        }
        last = dims;
        send(dims);
      }, delayMs);
    },
    cancel(): void {
      if (timer !== undefined) {
        clearTimeout(timer);
        timer = undefined;
      }
    },
  };
}

/** 终端流已关闭（进程退出，conn.rs 路由移除后的写失败文案）。 */
export function isTerminalClosedError(message: string): boolean {
  return message.includes("终端流") && message.includes("未打开");
}

/** 设备离线（连接已断开，需重连设备后重试）。 */
export function isDeviceOfflineError(message: string): boolean {
  return message.includes("设备未连接");
}

/** resume 目标已被回收（Agent Error{30}：不存在或已过期）。 */
export function isResumeGoneError(message: string): boolean {
  return message.includes("不存在或已过期");
}

// ---------------------------------------------------------------------------
// 组件
// ---------------------------------------------------------------------------

/** 视图状态：连接中 / 已打开 / 进程已退出 / 打开失败。 */
type TermStatus =
  | { readonly kind: "connecting" }
  | { readonly kind: "open" }
  | { readonly kind: "exited" }
  | { readonly kind: "error"; readonly message: string };

export interface TermViewProps {
  readonly deviceId: string;
  /** 标签 id（resume 存储 key 的一部分，Session 层保证稳定）。 */
  readonly tabId: string;
  /** shell 可执行名（Agent 白名单内，如 pwsh.exe）。 */
  readonly shell: string;
  /** xterm onTitleChange 透传（Session 用作标签标题）。 */
  readonly onTitle?: (title: string) => void;
}

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function TermView({ deviceId, tabId, shell, onTitle }: TermViewProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const streamIdRef = useRef<number | null>(null);
  const termIdRef = useRef<string | null>(null);
  const statusRef = useRef<TermStatus>({ kind: "connecting" });
  const onTitleRef = useRef(onTitle);
  onTitleRef.current = onTitle;
  const [status, setStatus] = useState<TermStatus>({ kind: "connecting" });
  /** 重试代数：递增触发整只视图重建（term 重新打开）。 */
  const [attempt, setAttempt] = useState(0);

  const setStatusBoth = (next: TermStatus): void => {
    statusRef.current = next;
    setStatus(next);
  };

  useEffect(() => {
    const container = containerRef.current;
    if (container === null) {
      return undefined;
    }
    let disposed = false;
    const encoder = new TextEncoder();

    // ---- xterm 初始化（任务卡步骤 2）----
    const term = new Terminal({
      fontFamily: '"Cascadia Mono", "JetBrains Mono", Menlo, monospace',
      fontSize: loadFontSize(),
      allowProposedApi: true,
      scrollback: 5000,
      theme: { background: "#0a0a0a" },
    });
    termRef.current = term;
    const fit = new FitAddon();
    term.loadAddon(fit);
    // unicode11 宽字符（中文/emoji 宽度正确）
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = "unicode11";
    try {
      // webgl 渲染失败回退默认 DOM 渲染器（加载失败/上下文丢失均回退）
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => {
        webgl.dispose();
      });
      term.loadAddon(webgl);
    } catch {
      // WebGL 不可用：DOM 渲染器兜底，不致命
    }
    term.open(container);
    try {
      fit.fit();
    } catch {
      // 容器尚未完成布局：挂载后 ResizeObserver 会再触发一次
    }

    const remoteResize = (dims: ResizeDims): void => {
      const termId = termIdRef.current;
      if (termId === null) {
        return;
      }
      void termResize(deviceId, termId, dims.cols, dims.rows).catch(
        () => undefined, // resize 单向且幂等，失败由下一次布局变更重试
      );
    };
    const scheduler = makeResizeScheduler(remoteResize);

    // 打开前不上报（term_id 未就绪，调度器会记录 last 导致漏发），
    // 打开完成后统一补一次
    const scheduleResize = (): void => {
      if (termIdRef.current === null) {
        return;
      }
      scheduler.schedule({ cols: term.cols, rows: term.rows });
    };

    // ---- FitAddon + ResizeObserver → term_resize（100 ms 去抖）----
    const observer = new ResizeObserver(() => {
      try {
        fit.fit();
      } catch {
        return;
      }
      scheduleResize();
    });
    observer.observe(container);

    // ---- 写失败分类：退出 / 设备离线（任务卡步骤 6）----
    const handleFailure = (message: string): void => {
      if (isTerminalClosedError(message)) {
        // 进程退出：Agent 已关流并回收会话，resume_id 失效
        clearResumeId(deviceId, tabId);
        setStatusBoth({ kind: "exited" });
      } else if (isDeviceOfflineError(message)) {
        setStatusBoth({
          kind: "error",
          message: "设备连接已断开，重新连接设备后点击重试",
        });
      }
      // 其他瞬时写失败不打断视图（下一次输入/探测自动重试）
    };

    // ---- 键入 → term_input；Ctrl+= / Ctrl+- 调字号 ----
    const applyFontSize = (delta: number): void => {
      const next = clampFontSize(loadFontSize() + delta);
      saveFontSize(next);
      term.options.fontSize = next;
      try {
        fit.fit();
      } catch {
        return;
      }
      scheduleResize();
    };
    term.attachCustomKeyEventHandler((ev) => {
      if (
        ev.type === "keydown" &&
        ev.ctrlKey &&
        !ev.altKey &&
        !ev.metaKey &&
        !ev.shiftKey
      ) {
        if (ev.key === "=" || ev.key === "+") {
          // 阻断浏览器/WebView 页面缩放，仅调终端字号
          ev.preventDefault();
          applyFontSize(1);
          return false;
        }
        if (ev.key === "-") {
          ev.preventDefault();
          applyFontSize(-1);
          return false;
        }
      }
      return true;
    });
    term.onData((data) => {
      const streamId = streamIdRef.current;
      if (streamId === null) {
        return;
      }
      void termInput(deviceId, streamId, Array.from(encoder.encode(data))).catch(
        (e: unknown) => handleFailure(errText(e)),
      );
    });
    term.onTitleChange((title) => {
      onTitleRef.current?.(title);
    });

    // ---- 打开/恢复会话（任务卡步骤 1/4）----
    const open = (resumeId: string | null) =>
      openTerminal(
        deviceId,
        shell,
        Math.max(term.cols, 2),
        Math.max(term.rows, 2),
        resumeId,
        (frame) => {
          term.write(new Uint8Array(frame));
        },
      );
    void (async () => {
      const stored = loadResumeId(deviceId, tabId);
      try {
        let ack;
        try {
          ack = await open(stored);
        } catch (e) {
          const message = errText(e);
          if (stored !== null && isResumeGoneError(message)) {
            // 会话已被回收（空闲超时/已退出）：清 id 后新开
            clearResumeId(deviceId, tabId);
            ack = await open(null);
          } else {
            throw e;
          }
        }
        if (disposed) {
          return;
        }
        saveResumeId(deviceId, tabId, ack.termId);
        termIdRef.current = ack.termId;
        streamIdRef.current = ack.streamId;
        setStatusBoth({ kind: "open" });
        // 连接期间发生的布局变化在此统一补报一次（远端 PTY 尺寸对齐）
        try {
          fit.fit();
        } catch {
          // 布局未就绪：下一次 ResizeObserver 再对齐
        }
        scheduler.schedule({ cols: term.cols, rows: term.rows });
      } catch (e) {
        if (!disposed) {
          setStatusBoth({ kind: "error", message: errText(e) });
        }
      }
    })();

    // ---- 存活探测：空载荷写入，Agent 关流后写失败 → 退出态 ----
    const probe = window.setInterval(() => {
      const streamId = streamIdRef.current;
      if (streamId === null || statusRef.current.kind !== "open") {
        return;
      }
      void termInput(deviceId, streamId, []).catch((e: unknown) =>
        handleFailure(errText(e)),
      );
    }, EXIT_PROBE_MS);

    return () => {
      disposed = true;
      window.clearInterval(probe);
      observer.disconnect();
      scheduler.cancel();
      term.dispose();
      termRef.current = null;
      streamIdRef.current = null;
      termIdRef.current = null;
    };
    // attempt 仅作为重建开关（重试/重新打开时递增触发本 effect 重跑）
  }, [deviceId, shell, tabId, attempt]);

  const retry = () => setAttempt((a) => a + 1);
  const reopen = () => {
    clearResumeId(deviceId, tabId);
    setAttempt((a) => a + 1);
  };

  return (
    <div className={styles.wrap}>
      <div ref={containerRef} className={styles.term} />
      {status.kind === "connecting" && (
        <div className={styles.overlay}>
          <p className={styles.overlayText}>正在打开终端…</p>
        </div>
      )}
      {status.kind === "exited" && (
        <div className={styles.overlay}>
          <p className={styles.overlayText}>[进程已退出]</p>
          <button type="button" className={styles.retryBtn} onClick={reopen}>
            重新打开
          </button>
        </div>
      )}
      {status.kind === "error" && (
        <div className={styles.overlay}>
          <p className={styles.overlayText}>{status.message}</p>
          <button type="button" className={styles.retryBtn} onClick={retry}>
            重试
          </button>
        </div>
      )}
    </div>
  );
}
