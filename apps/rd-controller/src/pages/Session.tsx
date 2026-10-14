/**
 * 会话页（T-021）：多标签，每标签一个窗口组。工具栏（模式 / zoom / HUD /
 * 导出）+ 标签栏（标题 + 关闭）+ 舞台（激活标签的 WindowGroupView）。
 *
 * URL 参数：`?open=<windowId>` 自动打开标签；`?term=<shell>` 终端标签由
 * T-023 消费（本卡不处理）。快捷键：Ctrl+Shift+H 切换 HUD、Ctrl+Shift+S
 * 导出当前激活标签根窗口的合成 PNG。
 */
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import {
  EMPTY_SNAPSHOT,
  SessionStore,
  ZOOM_LEVELS,
  type SessionSnapshot,
} from "../session/SessionStore";
import { WindowGroupView } from "../session/WindowGroupView";
import { closeWindow } from "../api/session";
import type { FitMode } from "../api/session";
import styles from "./Session.module.css";

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 无激活标签时 useSyncExternalStore 的占位订阅/快照。 */
const subscribeNoop = () => () => undefined;
const getEmptySnapshot = () => EMPTY_SNAPSHOT;

/** 单个会话标签的运行时容器：store 与视图共享键（windowId）。 */
interface TabEntry {
  readonly id: string;
  readonly store: SessionStore;
}

export function Session({ deviceId }: { deviceId: string }) {
  const [tabs, setTabs] = useState<readonly TabEntry[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [hudVisible, setHudVisible] = useState(true);
  /** 导出结果 / 错误提示（数秒后自动消失）。 */
  const [toast, setToast] = useState<string | null>(null);
  const [toastError, setToastError] = useState(false);
  /** 激活标签注册的导出函数（WindowGroupView 挂载时写入）。 */
  const exportFnRef = useRef<(() => Promise<string>) | null>(null);
  const toastTimerRef = useRef<number | undefined>(undefined);

  const showToast = useCallback((message: string, isError: boolean) => {
    setToast(message);
    setToastError(isError);
    window.clearTimeout(toastTimerRef.current);
    toastTimerRef.current = window.setTimeout(() => {
      setToast(null);
    }, 5000);
  }, []);

  /** 打开标签（幂等）；store 随标签创建。 */
  const openTab = useCallback((windowId: string) => {
    setTabs((prev) => {
      if (prev.some((t) => t.id === windowId)) {
        return prev;
      }
      return [...prev, { id: windowId, store: new SessionStore(windowId) }];
    });
    setActiveId(windowId);
  }, []);

  const closeTab = useCallback(
    (windowId: string) => {
      // 视图卸载清理同样会发 CloseWindow；重复发送幂等（Agent 静默忽略）
      void closeWindow(deviceId, windowId).catch(() => undefined);
      setTabs((prev) => prev.filter((t) => t.id !== windowId));
      setActiveId((cur) => (cur === windowId ? null : cur));
    },
    [deviceId],
  );

  // 关闭激活标签后回退到剩余最后一个标签
  useEffect(() => {
    if (activeId === null && tabs.length > 0) {
      const last = tabs[tabs.length - 1];
      if (last !== undefined) {
        setActiveId(last.id);
      }
    }
  }, [activeId, tabs]);

  // URL ?open=<windowId>：挂载与 hash 变化时解析并自动打开标签
  // （?term= 由 T-023 消费）。App 路由剥离查询串，同设备换开窗口只体现为
  // hash 变化，本页自行监听以捕获。
  useEffect(() => {
    const openFromHash = () => {
      const params = new URLSearchParams(window.location.hash.split("?")[1] ?? "");
      const open = params.get("open");
      if (open !== null && open.length > 0) {
        openTab(open);
      }
    };
    openFromHash();
    window.addEventListener("hashchange", openFromHash);
    return () => {
      window.removeEventListener("hashchange", openFromHash);
    };
  }, [openTab]);

  const activeEntry =
    activeId !== null
      ? tabs.find((t) => t.id === activeId) ?? null
      : null;
  const snapshot: SessionSnapshot = useSyncExternalStore(
    activeEntry?.store.subscribe ?? subscribeNoop,
    activeEntry?.store.getSnapshot ?? getEmptySnapshot,
  );

  const setFit = useCallback(
    (fit: FitMode) => {
      activeEntry?.store.setFit(fit);
    },
    [activeEntry],
  );

  const setZoom = useCallback(
    (zoom: number) => {
      activeEntry?.store.setZoom(zoom);
    },
    [activeEntry],
  );

  const doExport = useCallback(() => {
    const fn = exportFnRef.current;
    if (fn === null) {
      showToast("当前无激活会话，无法导出", true);
      return;
    }
    fn()
      .then((path) => showToast(`已导出：${path}`, false))
      .catch((e: unknown) => showToast(`导出失败：${errText(e)}`, true));
  }, [showToast]);

  // 快捷键：Ctrl+Shift+H 切换 HUD；Ctrl+Shift+S 导出合成 PNG
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey || !e.shiftKey) {
        return;
      }
      if (e.code === "KeyH") {
        e.preventDefault();
        setHudVisible((v) => !v);
      } else if (e.code === "KeyS") {
        e.preventDefault();
        doExport();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
    };
  }, [doExport]);

  // 卸载时清理 toast 定时器
  useEffect(() => {
    const timer = toastTimerRef.current;
    return () => {
      window.clearTimeout(timer);
    };
  }, []);

  return (
    <main className={styles.page}>
      <div className={styles.toolbar}>
        {(["FIT", "SCALE", "NATIVE"] as const).map((mode) => (
          <button
            key={mode}
            type="button"
            className={
              snapshot.fit === mode
                ? `${styles.toolBtn} ${styles.toolBtnActive}`
                : styles.toolBtn
            }
            onClick={() => setFit(mode)}
          >
            {mode === "FIT" ? "Fit" : mode === "SCALE" ? "Scale" : "Native"}
          </button>
        ))}
        <select
          className={styles.zoomSelect}
          aria-label="缩放倍率"
          value={snapshot.zoom}
          onChange={(e) => setZoom(Number(e.currentTarget.value))}
        >
          {ZOOM_LEVELS.map((z) => (
            <option key={z} value={z}>
              {Math.round(z * 100)}%
            </option>
          ))}
        </select>
        <button type="button" className={styles.toolBtn} onClick={doExport}>
          导出 PNG
        </button>
        <span className={styles.spacer} />
        {snapshot.notice !== null && (
          <span className={styles.notice}>{snapshot.notice}</span>
        )}
        {toast !== null && (
          <span className={toastError ? `${styles.toast} ${styles.toastError}` : styles.toast}>
            {toast}
          </span>
        )}
      </div>

      <nav className={styles.tabs}>
        {tabs.map((tab) => (
          <TabButton
            key={tab.id}
            id={tab.id}
            store={tab.store}
            active={tab.id === activeId}
            onSelect={() => setActiveId(tab.id)}
            onClose={() => closeTab(tab.id)}
          />
        ))}
      </nav>

      <div className={styles.stageArea}>
        {activeEntry !== null ? (
          <WindowGroupView
            key={activeEntry.id}
            deviceId={deviceId}
            windowId={activeEntry.id}
            store={activeEntry.store}
            showHud={hudVisible}
            exportFnRef={exportFnRef}
            onRootGone={() => closeTab(activeEntry.id)}
          />
        ) : (
          <p className={styles.empty}>
            从窗口列表打开一个远程窗口开始会话（Ctrl+Shift+H 切换 HUD，
            Ctrl+Shift+S 导出清晰度对比图）。
          </p>
        )}
      </div>
    </main>
  );
}

interface TabButtonProps {
  id: string;
  store: SessionStore;
  active: boolean;
  onSelect: () => void;
  onClose: () => void;
}

/** 标签按钮：标题取自会话快照（MOVED/TITLE 事件更新），未知名回退窗口 id。 */
function TabButton({ id, store, active, onSelect, onClose }: TabButtonProps) {
  const snapshot = useSyncExternalStore(store.subscribe, store.getSnapshot);
  const title = snapshot.title ?? `窗口 ${id}`;
  return (
    <div
      className={active ? styles.tabActive : styles.tab}
      role="tab"
      aria-selected={active}
      title={title}
      onClick={onSelect}
    >
      <span className={styles.tabTitle}>{title}</span>
      <button
        type="button"
        className={styles.tabClose}
        aria-label={`关闭 ${title}`}
        onClick={(e) => {
          e.stopPropagation();
          onClose();
        }}
      >
        ×
      </button>
    </div>
  );
}
