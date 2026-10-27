/**
 * 会话页（T-021 窗口组 + T-023 终端）：多标签。工具栏（「← 窗口列表」返回 +
 * 模式 / zoom 分组 + 导出分组）+ 标签栏（窗口标签 + “+ 终端”）+ 舞台（激活
 * 标签内容）；无标签时空态提供「打开窗口列表 / 打开终端」入口。
 *
 * 标签两类：窗口组（id = windowId，随标签持有 SessionStore）与终端
 * （id = `term:<shell>`，内容为 TermView；标题取 xterm onTitleChange，
 * 关闭只移除本地标签，远端会话保留供 resume）。
 *
 * URL 参数：`?open=<windowId>` 自动打开窗口标签；`?term=<shell>` 打开终端
 * 标签（如 `?term=wsl`）。快捷键：Ctrl+Shift+H 切换 HUD、Ctrl+Shift+S 导出
 * 当前激活窗口标签根窗口的合成 PNG（终端标签激活时为空操作提示）。
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
import type { RecordControl, SessionControl } from "../session/WindowGroupView";
import { WindowSidebar } from "../session/WindowSidebar";
import { TermView } from "../term/TermView";
import { closeWindow } from "../api/session";
import { FloatingToolbar } from "../session/FloatingToolbar";
import { DEFAULT_TERM_SHELL, parseShell } from "../api/term";
import type { FitMode } from "../api/session";
import styles from "../session/Session.module.css";

/** 终端标签 id 前缀（窗口 id 为十进制字符串，命名空间不重叠）。 */
const TERM_ID_PREFIX = "term:";

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 无激活标签时 useSyncExternalStore 的占位订阅/快照。 */
const subscribeNoop = () => () => undefined;
const getEmptySnapshot = () => EMPTY_SNAPSHOT;

/** 窗口组标签：id 为窗口句柄十进制字符串，store 与视图共享键。 */
interface WindowTab {
  readonly kind: "window";
  readonly id: string;
  readonly store: SessionStore;
}

/** 终端标签：id = `term:<shell>`（同 shell 幂等复用），标题由 TermView 上报。 */
interface TermTab {
  readonly kind: "term";
  readonly id: string;
  readonly shell: string;
  /** xterm onTitleChange 更新；未知名回退 shell。 */
  readonly title: string | null;
}

type SessionTab = WindowTab | TermTab;

export function Session({ deviceId }: { deviceId: string }) {
  const [tabs, setTabs] = useState<readonly SessionTab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  /** HUD 信息显隐（默认隐藏；工具栏 ℹ 或 Ctrl+Shift+H 切换，2026-10-27 用户反馈）。 */
  const [hudVisible, setHudVisible] = useState(false);
  /** 隐私屏状态（T-028 F9；Agent echo 确认后翻转）。 */
  const [privacyOn, setPrivacyOn] = useState(false);
  /** 录屏状态（T-028 F10）。 */
  const [recordingOn, setRecordingOn] = useState(false);
  /** 激活标签注册的录屏控制器（WindowGroupView 挂载时写入）。 */
  const recordCtlRef = useRef<RecordControl | null>(null);
  /** 窗口列表侧边栏显隐（T-028 F4；默认展开）。 */
  const [sidebarOpen, setSidebarOpen] = useState(true);
  /** 自控自会话（设备地址是 loopback）：注入类操作停用并提示（2026-10-27
   * 鼠标拉扯反馈——同机注入与控制端光标互斥，输入功能由双机场景验证）。 */
  const [loopback, setLoopback] = useState(false);
  /** 导出结果 / 错误提示（数秒后自动消失）。 */
  const [toast, setToast] = useState<string | null>(null);
  const [toastError, setToastError] = useState(false);
  /** 激活标签注册的导出函数（WindowGroupView 挂载时写入）。 */
  const exportFnRef = useRef<(() => Promise<string>) | null>(null);
  /** 激活标签注册的会话操作（只看/剪贴板；工具栏迁移 2026-10-27）。 */
  const sessionCtlRef = useRef<SessionControl | null>(null);
  const toastTimerRef = useRef<number | undefined>(undefined);

  const showToast = useCallback((message: string, isError: boolean) => {
    setToast(message);
    setToastError(isError);
    window.clearTimeout(toastTimerRef.current);
    toastTimerRef.current = window.setTimeout(() => {
      setToast(null);
    }, 5000);
  }, []);

  /** 打开窗口组标签（幂等）；store 随标签创建。 */
  const openTab = useCallback((windowId: string) => {
    setTabs((prev) => {
      if (prev.some((t) => t.id === windowId)) {
        return prev;
      }
      const next: WindowTab = {
        kind: "window",
        id: windowId,
        store: new SessionStore(windowId),
      };
      return [...prev, next];
    });
    setActiveId(windowId);
  }, []);

  /** 打开终端标签（同 shell 幂等；T-023）。 */
  const openTermTab = useCallback((shell: string) => {
    const id = `${TERM_ID_PREFIX}${shell}`;
    setTabs((prev) => {
      if (prev.some((t) => t.id === id)) {
        return prev;
      }
      const next: TermTab = { kind: "term", id, shell, title: null };
      return [...prev, next];
    });
    setActiveId(id);
  }, []);

  /** 终端标题上报（xterm onTitleChange）。 */
  const setTermTitle = useCallback((id: string, title: string) => {
    setTabs((prev) =>
      prev.map((t) => (t.kind === "term" && t.id === id ? { ...t, title } : t)),
    );
  }, []);

  const closeTab = useCallback(
    (tabId: string) => {
      // 终端标签只移除本地视图，远端会话保留（resume 窗口期内重开可恢复
      // 历史）；窗口标签照旧发 CloseWindow（视图卸载清理同样会发，重复
      // 发送幂等，Agent 静默忽略）
      if (!tabId.startsWith(TERM_ID_PREFIX)) {
        void closeWindow(deviceId, tabId).catch(() => undefined);
      }
      setTabs((prev) => prev.filter((t) => t.id !== tabId));
      setActiveId((cur) => (cur === tabId ? null : cur));
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

  // URL ?open=<windowId> / ?term=<shell>：挂载与 hash 变化时解析并自动打开
  // 标签（打开均幂等，重复 hash 事件无副作用）。App 路由剥离查询串，同设备
  // 换开窗口只体现为 hash 变化，本页自行监听以捕获。
  useEffect(() => {
    const openFromHash = () => {
      const params = new URLSearchParams(window.location.hash.split("?")[1] ?? "");
      const open = params.get("open");
      if (open !== null && open.length > 0) {
        openTab(open);
      }
      const term = params.get("term");
      if (term !== null && term.length > 0) {
        openTermTab(parseShell(term));
      }
    };
    openFromHash();
    window.addEventListener("hashchange", openFromHash);
    return () => {
      window.removeEventListener("hashchange", openFromHash);
    };
  }, [openTab, openTermTab]);

  const activeTab =
    activeId !== null ? tabs.find((t) => t.id === activeId) ?? null : null;
  const activeWindow = activeTab?.kind === "window" ? activeTab : null;
  const snapshot: SessionSnapshot = useSyncExternalStore(
    activeWindow?.store.subscribe ?? subscribeNoop,
    activeWindow?.store.getSnapshot ?? getEmptySnapshot,
  );

  const setFit = useCallback(
    (fit: FitMode) => {
      activeWindow?.store.setFit(fit);
    },
    [activeWindow],
  );

  const setZoom = useCallback(
    (zoom: number) => {
      activeWindow?.store.setZoom(zoom);
    },
    [activeWindow],
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

  // 自控自检测：设备地址是 loopback → 输入类操作提示停用（注入在 Agent
  // 侧也已停用，双保险；画面/工具栏 UI 不受影响）
  useEffect(() => {
    let alive = true;
    void import("../api/devices").then(async (m) => {
      try {
        const list = await m.listDevices();
        const dev = list.find((d) => d.id === deviceId);
        if (alive) {
          setLoopback(
            Boolean(
              dev?.addr.includes("127.0.0.1") ||
                dev?.addr.includes("[::1]") ||
                dev?.addr.includes("localhost"),
            ),
          );
        }
      } catch {
        /* 列表失败不影响会话 */
      }
    });
    return () => {
      alive = false;
    };
  }, [deviceId]);

  /** 返回该设备的窗口列表（导航主出口）。 */
  const goWindows = useCallback(() => {
    window.location.hash = `#/windows/${deviceId}`;
  }, [deviceId]);

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

  // ── 工具栏操作（T-028） ──
  async function doScreenshot() {
    // 静默失败 = 用户视角的「没反应」（2026-10-27 反馈）——所有路径都给提示
    if (!exportFnRef.current) {
      showToast("截屏不可用：请先打开一个远程窗口", true);
      return;
    }
    try {
      const path = await exportFnRef.current();
      showToast(`截屏已保存：${path}`, false);
    } catch (e) {
      showToast(`截屏失败：${String(e)}`, true);
    }
  }
  /** 只看不控切换（工具栏 👁）：操作在激活标签内，结果 toast。 */
  function doToggleViewOnly() {
    const ctl = sessionCtlRef.current;
    if (ctl === null) {
      showToast("请先打开一个远程窗口", true);
      return;
    }
    const before = ctl.viewOnly;
    ctl.toggleViewOnly();
    showToast(before ? "已恢复控制（键鼠生效）" : "已开启只看不控（键鼠停发）", false);
  }
  /** 剪贴板互传（工具栏 📤📥）：成功/失败都 toast（原静默失败已修）。 */
  async function doCopy(kind: "to" | "from") {
    const ctl = sessionCtlRef.current;
    if (ctl === null) {
      showToast("请先打开一个远程窗口", true);
      return;
    }
    try {
      if (kind === "to") {
        await ctl.copyToRemote();
        showToast("已发送到远端剪贴板（远端 Ctrl+V 粘贴）", false);
      } else {
        await ctl.copyFromRemote();
        showToast("已取回远端剪贴板（本机 Ctrl+V 粘贴）", false);
      }
    } catch (e) {
      showToast(`剪贴板操作失败：${String(e)}`, true);
    }
  }
  async function doFullscreen() {
    if (document.fullscreenElement) await document.exitFullscreen();
    else await document.documentElement.requestFullscreen();
  }
  async function doLockScreen() {
    try { const m = await import("../api/input"); await m.textInput(deviceId, "__RD_LOCK__"); } catch {}
  }
  async function doCtrlAltDel() {
    try { const m = await import("../api/input"); await m.textInput(deviceId, "__RD_CAD__"); } catch {}
  }
  async function doSendText(text: string) {
    if (loopback) {
      showToast("本机自测模式：注入已停用（自己控自己，文字本来就在本机）。连接第二台电脑即可使用", true);
      return;
    }
    try { const m = await import("../api/input"); await m.textInput(deviceId, text); } catch {}
  }
  async function doSendCombo(keys: string[]) {
    if (loopback) {
      showToast("本机自测模式：注入已停用。连接第二台电脑即可使用", true);
      return;
    }
    try {
      const { keyEvent } = await import("../api/input");
      const SCAN: Record<string, [number, boolean]> = {
        Ctrl: [0x1D, false], Alt: [0x38, false], Shift: [0x2A, false],
        Win: [0x5B, true], Enter: [0x1C, false], Escape: [0x01, false],
        Tab: [0x0F, false], Backspace: [0x0E, false], Delete: [0xD3, true],
        ArrowUp: [0xC8, true], ArrowDown: [0xD0, true],
        ArrowLeft: [0xCB, true], ArrowRight: [0xCD, true],
      };
      for (const k of keys) { const sc = SCAN[k]; if (sc) await keyEvent(deviceId, sc[0], sc[1], true, 0); }
      for (const k of [...keys].reverse()) { const sc = SCAN[k]; if (sc) await keyEvent(deviceId, sc[0], sc[1], false, 0); }
    } catch {}
  }
  /** 画质档位（T-028 F5）：会话内全部窗口生效，成功/失败都提示。 */
  async function doQuality(preset: "auto" | "high" | "medium" | "low") {
    try {
      const { setQuality } = await import("../api/session");
      await setQuality(deviceId, preset);
      const label = { auto: "自动", high: "高清", medium: "均衡", low: "流畅" }[preset];
      showToast(`画质已切换：${label}`, false);
    } catch (e) {
      showToast(`切换画质失败：${String(e)}`, true);
    }
  }
  /** 隐私屏（T-028 F9，Pro）：黑罩开关，状态翻转由后端 echo 确认。 */
  async function doPrivacy(on: boolean) {
    try {
      const { setPrivacy } = await import("../api/session");
      await setPrivacy(deviceId, on);
      setPrivacyOn(on);
      showToast(on ? "隐私屏已开启：对方屏幕已黑屏" : "隐私屏已关闭", false);
    } catch (e) {
      showToast(`隐私屏切换失败：${String(e)}`, true);
    }
  }
  /** 录屏（T-028 F10）：启停由激活标签注册的控制器执行。 */
  async function doRecordToggle() {
    const ctl = recordCtlRef.current;
    if (ctl === null) {
      showToast("请先打开一个远程窗口", true);
      return;
    }
    try {
      if (!recordingOn) {
        await ctl.start();
        setRecordingOn(true);
        showToast("开始录制远程画面", false);
      } else {
        const path = await ctl.stop();
        setRecordingOn(false);
        showToast(`录制已保存：${path}`, false);
      }
    } catch (e) {
      setRecordingOn(false);
      showToast(`录屏失败：${String(e)}`, true);
    }
  }

  return (
    <main className={styles.page}>
      <div className={styles.toolbar}>
        <button
          type="button"
          className={styles.backBtn}
          onClick={goWindows}
          title="返回该设备的窗口列表"
        >
          ← 窗口列表
        </button>
        <span className={styles.toolbarDivider} aria-hidden="true" />
        <div className={styles.toolbarGroup} role="group" aria-label="显示模式与缩放">
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
        </div>
        <span className={styles.toolbarDivider} aria-hidden="true" />
        <div className={styles.toolbarGroup} role="group" aria-label="会话操作">
          <button type="button" className={styles.toolBtn} onClick={doExport}>
            导出 PNG
          </button>
        </div>
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
        {tabs.map((tab) =>
          tab.kind === "term" ? (
            <TabButton
              key={tab.id}
              title={tab.title ?? `终端 ${tab.shell}`}
              closeLabel={`关闭 终端 ${tab.shell}`}
              active={tab.id === activeId}
              onSelect={() => setActiveId(tab.id)}
              onClose={() => closeTab(tab.id)}
            />
          ) : (
            <WindowTabButton
              key={tab.id}
              tab={tab}
              active={tab.id === activeId}
              onSelect={() => setActiveId(tab.id)}
              onClose={() => closeTab(tab.id)}
            />
          ),
        )}
        <button
          type="button"
          className={styles.toolBtn}
          onClick={() => openTermTab(DEFAULT_TERM_SHELL)}
        >
          + 终端
        </button>
        <button
          type="button"
          className={styles.toolBtn}
          title={sidebarOpen ? "收起窗口列表" : "展开窗口列表"}
          onClick={() => setSidebarOpen((v) => !v)}
        >
          {sidebarOpen ? "📌 收起列表" : "📌 窗口列表"}
        </button>
      </nav>

      <div className={styles.sessionBody}>
        {sidebarOpen && (
          <WindowSidebar
            deviceId={deviceId}
            openIds={new Set(tabs.filter((t) => t.kind === "window").map((t) => t.id))}
            activeId={activeId}
            onSelect={setActiveId}
            onOpen={openTab}
            onCloseTab={closeTab}
          />
        )}
        <div className={styles.stageArea}>
        {activeTab === null ? (
          <div className={styles.empty}>
            <p className={styles.emptyTitle}>未打开任何会话</p>
            <p className={styles.emptyHint}>
              从窗口列表打开一个远程窗口，或直接开始终端会话。
              <br />
              快捷键：Ctrl+Shift+H 切换 HUD，Ctrl+Shift+S 导出清晰度对比图。
            </p>
            <div className={styles.emptyActions}>
              <button type="button" className={styles.primaryBtn} onClick={goWindows}>
                打开窗口列表
              </button>
              <button
                type="button"
                className={styles.toolBtn}
                onClick={() => openTermTab(DEFAULT_TERM_SHELL)}
              >
                打开终端
              </button>
            </div>
          </div>
        ) : activeTab.kind === "term" ? (
          <div className={styles.stage}>
            <TermView
              key={activeTab.id}
              deviceId={deviceId}
              tabId={activeTab.id}
              shell={activeTab.shell}
              onTitle={(title) => setTermTitle(activeTab.id, title)}
            />
          </div>
        ) : (
          <div className={styles.stage} style={{ position: "relative" }}>
            <FloatingToolbar
              onScreenshot={() => void doScreenshot()}
              onFullscreen={() => void doFullscreen()}
              onLockScreen={() => void doLockScreen()}
              onCtrlAltDel={() => void doCtrlAltDel()}
              onQualityChange={(q) => void doQuality(q)}
              onPrivacyToggle={(on) => void doPrivacy(on)}
              privacyOn={privacyOn}
              onRecordToggle={() => void doRecordToggle()}
              recordingOn={recordingOn}
              onToggleViewOnly={doToggleViewOnly}
              viewOnly={sessionCtlRef.current?.viewOnly ?? false}
              onCopyToRemote={() => void doCopy("to")}
              onCopyFromRemote={() => void doCopy("from")}
              onToggleHud={() => setHudVisible((v) => !v)}
              hudVisible={hudVisible}
              onSendText={(text) => void doSendText(text)}
              onSendCombo={(keys) => void doSendCombo(keys)}
            />
            <WindowGroupView
              key={activeTab.id}
              deviceId={deviceId}
              windowId={activeTab.id}
              store={activeTab.store}
              showHud={hudVisible}
              exportFnRef={exportFnRef}
              recordCtlRef={recordCtlRef}
              sessionCtlRef={sessionCtlRef}
              onRootGone={() => closeTab(activeTab.id)}
            />
          </div>
        )}
        </div>
      </div>
    </main>
  );
}

interface TabButtonProps {
  title: string;
  closeLabel: string;
  active: boolean;
  onSelect: () => void;
  onClose: () => void;
}

/** 标签按钮（两类标签共用外观）。 */
function TabButton({
  title,
  closeLabel,
  active,
  onSelect,
  onClose,
}: TabButtonProps) {
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
        aria-label={closeLabel}
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

/** 窗口标签按钮：标题取自会话快照（MOVED/TITLE 事件更新），未知名回退窗口 id。 */
function WindowTabButton({
  tab,
  active,
  onSelect,
  onClose,
}: {
  tab: WindowTab;
  active: boolean;
  onSelect: () => void;
  onClose: () => void;
}) {
  const snapshot = useSyncExternalStore(tab.store.subscribe, tab.store.getSnapshot);
  const title = snapshot.title ?? `窗口 ${tab.id}`;
  return (
    <TabButton
      title={title}
      closeLabel={`关闭 ${title}`}
      active={active}
      onSelect={onSelect}
      onClose={onClose}
    />
  );
}
