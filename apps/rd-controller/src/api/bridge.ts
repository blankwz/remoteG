/**
 * 运行时桥接（demo 预览模式）：区分 Tauri 桌面运行时与纯浏览器环境。
 *
 * 纯浏览器（`pnpm dev` 后直接开 http://localhost:1420）下没有 Tauri IPC，
 * 一切 invoke 都会失败——此时走 mock（mock.ts）返回演示数据，供 UI/UX
 * 设计评审截图使用。桌面运行时行为与原 invoke 完全一致，零影响。
 */
import { invoke } from "@tauri-apps/api/core";
import { mockInvoke } from "./mock";

/** 是否运行在 Tauri 桌面（WebView）环境。 */
export function isTauriRuntime(): boolean {
  return (
    typeof window !== "undefined" &&
    (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ !== undefined
  );
}

/** invoke 的桥接版：浏览器环境自动切换 mock 数据源。 */
export function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriRuntime()) {
    return mockInvoke<T>(cmd, args);
  }
  return invoke<T>(cmd, args);
}

/** Tauri 事件监听的桥接版：浏览器环境返回空订阅（无事件）。 */
export function listenStub<P>(
  event: string,
  handler: (e: { payload: P }) => void,
): Promise<() => void> {
  if (!isTauriRuntime()) {
    void handler;
    return Promise.resolve(() => undefined);
  }
  return import("@tauri-apps/api/event").then((m) => m.listen(event, handler));
}
