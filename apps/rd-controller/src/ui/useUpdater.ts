/**
 * 更新流程状态机（T-038 / R2 修复 H-1）：**模块级单例**外部 store——
 * App 横幅与设置页共享同一份状态与 pending 句柄，杜绝「两实例各自
 * available / 双份 downloadAndInstall」的割裂（code-review H-1）。
 *
 * idle → checking → available → downloading(pct) → ready → restartApp()
 * - 静默检查：首个订阅者挂载时跑一次（autoCheckStarted 进程级只跑一次），
 *   24h 节流；失败状态如实记 error，但 UpdateBanner 渲染层吞掉不打扰。
 * - 手动检查（设置页）：错误亮出；同一状态机，横幅同步可见。
 */
import { useEffect, useSyncExternalStore } from "react";
import {
  checkForUpdate,
  readLastCheck,
  restartApp,
  shouldCheckNow,
  type PendingUpdate,
} from "../api/update";

/** 更新状态机的对外视图（UI 只读渲染）。 */
export type UpdateState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "available"; pending: PendingUpdate }
  | { phase: "downloading"; pct: number | null }
  | { phase: "ready" }
  | { phase: "error"; message: string };

// ---- 模块级单例 store（每进程一份；所有消费者经 useSyncExternalStore 订阅）----
let state: UpdateState = { phase: "idle" };
/** available 句柄：install 的唯一入口，避免两个消费者各持一份。 */
let pending: PendingUpdate | null = null;
/** 静默检查进程级只触发一次（多页面挂载不重复检查）。 */
let autoCheckStarted = false;
const listeners = new Set<() => void>();

function setState(next: UpdateState): void {
  state = next;
  for (const notify of listeners) {
    notify();
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 手动/静默共用的检查入口；checking/downloading 中不重入。 */
async function checkNow(): Promise<void> {
  if (state.phase === "checking" || state.phase === "downloading") {
    return; // 进行中不重入
  }
  setState({ phase: "checking" });
  try {
    const found = await checkForUpdate();
    if (found === null) {
      setState({ phase: "idle" });
      return;
    }
    pending = found;
    setState({ phase: "available", pending: found });
  } catch (e) {
    setState({ phase: "error", message: messageOf(e) });
  }
}

/** 下载并安装（唯一入口走模块级 pending）；完成后 ready，等用户点重启。 */
async function install(): Promise<void> {
  if (pending === null || state.phase === "downloading") {
    return;
  }
  setState({ phase: "downloading", pct: null });
  try {
    await pending.install((p) => {
      setState({
        phase: "downloading",
        pct:
          p.total === null
            ? null
            : p.total > 0
              ? Math.round((p.received / p.total) * 100)
              : null,
      });
    });
    setState({ phase: "ready" });
  } catch (e) {
    setState({ phase: "error", message: messageOf(e) });
  }
}

function restart(): void {
  void restartApp().catch(() => undefined);
}

/** 供 App（横幅）与 Settings（手动检查）共用的更新 hook（单例视图）。 */
export function useUpdater(): {
  state: UpdateState;
  checkNow: () => Promise<void>;
  install: () => Promise<void>;
  restart: () => void;
} {
  const snapshot = useSyncExternalStore(subscribe, () => state);

  // 静默检查：进程内首个订阅者挂载时触发一次；24h 节流；失败不打扰
  useEffect(() => {
    if (autoCheckStarted) {
      return;
    }
    autoCheckStarted = true;
    if (!shouldCheckNow(readLastCheck(), Date.now())) {
      return;
    }
    void checkNow().catch(() => undefined);
  }, []);

  return { state: snapshot, checkNow, install, restart };
}

/** 测试专用：重置模块级单例（进程全局状态，不重置会跨用例泄漏）。 */
export function __resetUpdaterStoreForTests(): void {
  state = { phase: "idle" };
  pending = null;
  autoCheckStarted = false;
  listeners.clear();
}
