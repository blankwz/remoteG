/**
 * 更新流程状态机（T-038）：静默检查（App 挂载、24h 节流）与手动检查
 * （设置页「检查更新」按钮）共用同一 hook；UI 只读渲染，单一事实。
 *
 * idle → checking → available → downloading(pct) → ready → restartApp()
 * - 静默路径吞错保持 idle；手动检查才把错误亮出来（phase error）。
 */
import { useCallback, useEffect, useRef, useState } from "react";
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

/** 供 App（静默+横幅）与 Settings（手动检查）共用的更新 hook。 */
export function useUpdater(): {
  state: UpdateState;
  checkNow: () => Promise<void>;
  install: () => Promise<void>;
  restart: () => void;
} {
  const [state, setState] = useState<UpdateState>({ phase: "idle" });
  /** available 句柄保 ref：install 用最新值，不进依赖数组。 */
  const pendingRef = useRef<PendingUpdate | null>(null);
  const stateRef = useRef<UpdateState>({ phase: "idle" });
  stateRef.current = state;

  const checkNow = useCallback(async (): Promise<void> => {
    if (
      stateRef.current.phase === "checking" ||
      stateRef.current.phase === "downloading"
    ) {
      return; // 进行中不重入
    }
    setState({ phase: "checking" });
    try {
      const found = await checkForUpdate();
      if (found === null) {
        setState({ phase: "idle" });
        return;
      }
      pendingRef.current = found;
      setState({ phase: "available", pending: found });
    } catch (e) {
      setState({
        phase: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  const install = useCallback(async (): Promise<void> => {
    const pending = pendingRef.current;
    if (pending === null || stateRef.current.phase === "downloading") {
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
      setState({
        phase: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  const restart = useCallback((): void => {
    void restartApp().catch(() => undefined);
  }, []);

  // 静默检查：挂载一次，24h 节流；失败吞错保持 idle（不打扰）
  useEffect(() => {
    if (!shouldCheckNow(readLastCheck(), Date.now())) {
      return;
    }
    void checkNow().catch(() => undefined);
  }, [checkNow]);

  return { state, checkNow, install, restart };
}
