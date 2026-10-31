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
import { FloatingToolbar, type Quality } from "../session/FloatingToolbar";
import { useDismiss } from "../ui/useDismiss";
import { DEFAULT_TERM_SHELL, TERM_SHELLS, parseShell } from "../api/term";
import type { TermStatus } from "../term/TermView";
import {
  IconArrowLeft,
  IconChevronDown,
  IconEyeOff,
  IconLock,
  IconPlus,
  IconSidebar,
  IconTerminal,
} from "../ui/Icon";
import type { FitMode } from "../api/session";
import styles from "../session/Session.module.css";

/** 终端标签 id 前缀（窗口 id 为十进制字符串，命名空间不重叠）。 */
const TERM_ID_PREFIX = "term:";

/** shell → 显示名（标签/菜单共用一套词）。 */
function shellLabel(shell: string): string {
  switch (shell) {
    case "pwsh.exe":
      return "PowerShell";
    case "cmd.exe":
      return "cmd";
    case "wsl.exe":
      return "WSL";
    case "powershell.exe":
      return "PowerShell 5";
    default:
      return shell;
  }
}

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
  /** 画质档位（受控真实态；localStorage 记忆上次选择）。 */
  const [quality, setQuality] = useState<Quality>(() => {
    const saved = window.localStorage.getItem("rd.quality");
    return saved === "high" || saved === "medium" || saved === "low" ? saved : "auto";
  });
  /** 当前设备名（顶栏上下文显示）。 */
  const [deviceName, setDeviceName] = useState<string | null>(null);
  /** 窗口真实标题表（打开标签时从窗口列表反查，避免标签裸显句柄号）。 */
  const titleCacheRef = useRef<Map<string, string>>(new Map());
  /** 「+ 终端」shell 选择下拉开关。 */
  const [termMenuOpen, setTermMenuOpen] = useState(false);
  /** 下拉容器（Esc/点外关闭协议，R11 D08——唯一出口 useDismiss）。 */
  const termMenuRef = useRef<HTMLDivElement>(null);
  useDismiss(termMenuOpen, termMenuRef, useCallback(() => setTermMenuOpen(false), []));
  /** 录制开始时刻（REC 常驻指示计时用；null = 未在录）。 */
  const [recordingSince, setRecordingSince] = useState<number | null>(null);
  /** 快捷键速查浮层（? 呼出）。 */
  const [helpOpen, setHelpOpen] = useState(false);
  const helpOpenRef = useRef(false);
  useEffect(() => {
    helpOpenRef.current = helpOpen;
  }, [helpOpen]);
  /** REC 指示的当前时长文本（每秒刷新）。 */
  const [recElapsed, setRecElapsed] = useState("");
  /** 录屏状态（T-028 F10）。 */
  const [recordingOn, setRecordingOn] = useState(false);
  /** 锁屏待确认时刻（3 秒内再点确认；快捷键 effect 的真状态依赖）。 */
  const [lockArmedAt, setLockArmedAt] = useState(0);
  /** 激活标签注册的录屏控制器（WindowGroupView 挂载时写入）。 */
  const recordCtlRef = useRef<RecordControl | null>(null);
  /** 窗口列表侧边栏显隐（T-028 F4；默认展开）。 */
  const [sidebarOpen, setSidebarOpen] = useState<boolean>(() => {
    try {
      return window.localStorage.getItem("rd.sidebar") !== "closed";
    } catch {
      return true;
    }
  });
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

  /** tabs 镜像（同步 effect 维护），供 URL 幂等判断读取最新标签表。 */
  const tabsRef = useRef<readonly SessionTab[]>([]);
  useEffect(() => {
    tabsRef.current = tabs;
  }, [tabs]);

  /** 新开终端标签（显式多开：每次新 id；R1 评委共识——同 shell 也要能多开）。 */
  const openTermTab = useCallback((shell: string) => {
    const newId = `term:${shell}:${Date.now().toString(36)}`;
    setTabs((prev) => [...prev, { kind: "term", id: newId, shell, title: null }]);
    setActiveId(newId);
  }, []);

  /** URL `?term=` 路径的幂等打开：已有同 shell 终端则激活它（防 hash 往返误开）。 */
  const openTermFromUrl = useCallback(
    (shell: string) => {
      const found = tabsRef.current.find((t) => t.kind === "term" && t.shell === shell);
      if (found !== undefined) {
        setActiveId(found.id);
        return;
      }
      openTermTab(shell);
    },
    [openTermTab],
  );

  /** 终端标题上报（xterm onTitleChange）。 */
  const setTermTitle = useCallback((id: string, title: string) => {
    setTabs((prev) =>
      prev.map((t) => (t.kind === "term" && t.id === id ? { ...t, title } : t)),
    );
  }, []);

  /** 终端真实状态表（标签 pill 用；TermView onStatus 上报，D04）。 */
  const [termStatuses, setTermStatuses] = useState<Record<string, TermStatus>>({});
  const setTermStatus = useCallback((id: string, s: TermStatus) => {
    setTermStatuses((prev) => ({ ...prev, [id]: s }));
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
      setTermStatuses((prev) => {
        if (!(tabId in prev)) {
          return prev;
        }
        const { [tabId]: _removed, ...rest } = prev;
        return rest;
      });
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
  const lastHandledHashRef = useRef(" ");
  useEffect(() => {
    const openFromHash = () => {
      if (window.location.hash === lastHandledHashRef.current) {
        return; // StrictMode effect 双跑/重复事件防重（终端标签误双开）
      }
      lastHandledHashRef.current = window.location.hash;
      const params = new URLSearchParams(window.location.hash.split("?")[1] ?? "");
      const open = params.get("open");
      if (open !== null && open.length > 0) {
        openTab(open);
      }
      const term = params.get("term");
      if (term !== null && term.length > 0) {
        openTermFromUrl(parseShell(term));
      }
    };
    openFromHash();
    window.addEventListener("hashchange", openFromHash);
    return () => {
      window.removeEventListener("hashchange", openFromHash);
    };
  }, [openTab, openTermFromUrl]);

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

  // 自控自检测：设备地址是 loopback → 输入类操作提示停用（注入在 Agent
  // 侧也已停用，双保险；画面/工具栏 UI 不受影响）。同时取设备名供顶栏显示。
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const [devs, wins] = await Promise.all([
          import("../api/devices").then((m) => m.listDevices()),
          import("../api/windows").then((m) => m.listWindows(deviceId)).catch(() => []),
        ]);
        const dev = devs.find((d) => d.id === deviceId);
        if (alive && dev !== undefined) {
          setDeviceName(dev.name);
          // 反查窗口真实标题（标签命名用）
          const cache = new Map<string, string>();
          for (const w of wins) {
            if (w.title.length > 0) {
              cache.set(w.id, w.title);
            }
          }
          titleCacheRef.current = cache;
          setLoopback(
            Boolean(
              dev.addr.includes("127.0.0.1") ||
                dev.addr.includes("[::1]") ||
                dev.addr.includes("localhost"),
            ),
          );
        }
      } catch {
        /* 列表失败不影响会话 */
      }
    })();
    return () => {
      alive = false;
    };
  }, [deviceId]);

  /** 返回该设备的窗口列表（导航主出口）。 */
  const goWindows = useCallback(() => {
    window.location.hash = `#/windows/${deviceId}`;
  }, [deviceId]);

  // 快捷键：Ctrl+Shift+H 信息 / S 保存画面 / L 锁屏 / V 只看 / C 取回剪贴板 /
  // X 发送剪贴板；Ctrl+Alt+1..9 切会话标签。
  // （注入层同键保留判定见 InputMapper.isReservedCombo——两处必须同步，
  // 否则热键既下发远端又本地触发，R11 双入口回归）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Ctrl+Alt+1..9 切标签（保留表同步判定，不下发远端）
      if (e.ctrlKey && e.altKey && !e.shiftKey && !e.metaKey) {
        if (/^Digit[1-9]$/.test(e.code)) {
          const target = tabs[Number(e.code.slice(5)) - 1];
          if (target !== undefined) {
            e.preventDefault();
            setActiveId(target.id);
          }
          return;
        }
        return;
      }
      if (!e.ctrlKey || !e.shiftKey || e.altKey || e.metaKey) {
        return;
      }
      if (e.code === "KeyH") {
        e.preventDefault();
        setHudVisible((v) => !v);
      } else if (e.code === "KeyS") {
        e.preventDefault();
        void doScreenshot();
      } else if (e.code === "KeyL") {
        e.preventDefault();
        void doLockScreen();
      } else if (e.code === "KeyV") {
        e.preventDefault();
        doToggleViewOnly();
      } else if (e.code === "KeyC") {
        e.preventDefault();
        void doCopy("from");
      } else if (e.code === "KeyX") {
        e.preventDefault();
        void doCopy("to");
      }
    };
    const onHelpKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const typing =
        target !== null &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA");
      if (e.key === "?" && !typing) {
        e.preventDefault();
        setHelpOpen((v) => !v);
      } else if (e.key === "Escape" && helpOpenRef.current) {
        setHelpOpen(false);
      }
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("keydown", onHelpKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("keydown", onHelpKey);
    };
    // onKey 闭包经 ref（exportFnRef/sessionCtlRef）与 setXxx 读状态；真状态
    // 依赖是锁屏二次确认时刻与标签表（Ctrl+Alt+digit 切签）
  }, [lockArmedAt, tabs]);

  // 卸载时清理 toast 定时器（读 ref 本体，不捕获挂载时的 undefined）
  useEffect(() => {
    return () => {
      window.clearTimeout(toastTimerRef.current);
    };
  }, []);

  // 会话离开保护：有活跃标签时刷新/关闭页面给出确认（容错评委 R7）
  useEffect(() => {
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      if (tabs.length > 0) {
        e.preventDefault();
        e.returnValue = "";
      }
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [tabs]);

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
      showToast(`截屏没有成功：${errText(e)}`, true);
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
      showToast(`剪贴板操作没有成功：${errText(e)}`, true);
    }
  }
  async function doFullscreen() {
    if (document.fullscreenElement) await document.exitFullscreen();
    else await document.documentElement.requestFullscreen();
  }
  async function doLockScreen() {
    if (Date.now() - lockArmedAt > 3000) {
      setLockArmedAt(Date.now());
      showToast("再次点击「锁屏」确认执行（3 秒内）", false);
      return;
    }
    setLockArmedAt(0);
    try {
      const m = await import("../api/input");
      await m.textInput(deviceId, "__RD_LOCK__");
      showToast("已发送锁屏指令", false);
    } catch (e) {
      showToast(`锁屏指令没有送达：${errText(e)}`, true);
    }
  }
  async function doCtrlAltDel() {
    try {
      const m = await import("../api/input");
      await m.textInput(deviceId, "__RD_CAD__");
      showToast("已发送 Ctrl+Alt+Del，等待远端响应", false);
    } catch (e) {
      showToast(`Ctrl+Alt+Del 没有送达：${errText(e)}`, true);
    }
  }
  async function doSendText(text: string) {
    try {
      const m = await import("../api/input");
      await m.textInput(deviceId, text);
      showToast(`已发送「${text}」`, false);
    } catch (e) {
      showToast(`文字没有送达：${errText(e)}`, true);
    }
  }
  async function doSendCombo(keys: string[]) {
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
      showToast(`已发送 ${keys.join("+")}`, false);
    } catch (e) {
      showToast(`按键没有送达：${errText(e)}`, true);
    }
  }
  /** 画质档位（T-028 F5）：受控切换——先更新 UI 再下发，失败回滚。 */
  async function doQuality(preset: Quality) {
    const prev = quality;
    setQuality(preset); // 控件即时回声（受控）
    window.localStorage.setItem("rd.quality", preset);
    try {
      const { setQuality } = await import("../api/session");
      await setQuality(deviceId, preset);
      const label = { auto: "自动", high: "高清", medium: "均衡", low: "流畅" }[preset];
      showToast(`画质：${label}`, false);
    } catch (e) {
      setQuality(prev);
      showToast(`画质切换没有生效：${errText(e)}`, true);
    }
  }
  /** 安全屏待确认时刻（误触会让对方屏幕变黑，与锁屏同保护级别）。 */
  const [privacyArmedAt, setPrivacyArmedAt] = useState(0);
  /** 隐私屏（T-028 F9，Pro）：开关指令下发成功后置位（echo 链路由后端保证）。 */
  async function doPrivacy(on: boolean) {
    if (on && Date.now() - privacyArmedAt > 3000) {
      setPrivacyArmedAt(Date.now());
      showToast("再次点击「安全屏」确认开启（对方屏幕将变黑）", false);
      return;
    }
    setPrivacyArmedAt(0);
    try {
      const { setPrivacy } = await import("../api/session");
      await setPrivacy(deviceId, on);
      setPrivacyOn(on);
      showToast(
        on ? "隐私屏指令已送达：对方屏幕即将黑屏" : "隐私屏已关闭，对方屏幕恢复显示",
        false,
      );
    } catch (e) {
      showToast(`隐私屏指令没有送达：${errText(e)}`, true);
    }
  }
  /** 录屏（T-028 F10）：启停由激活标签注册的控制器执行；REC 指示跨标签常驻。 */
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
        setRecordingSince(Date.now());
        showToast("开始录制远程画面", false);
      } else {
        const path = await ctl.stop();
        setRecordingOn(false);
        setRecordingSince(null);
        showToast(`录制已保存：${path}`, false);
      }
    } catch (e) {
      setRecordingOn(false);
      setRecordingSince(null);
      showToast(`录屏失败：${errText(e)}`, true);
    }
  }

  /** 录制中标签被关闭：WindowGroupView 卸载路径自动落盘后回调（R11 D01）。
   * 复位 REC 徽标并如实告知保存结果——曾假承诺「切走会保存」实则丢文件。 */
  const handleRecordingAutoStop = useCallback(
    (path: string | null) => {
      setRecordingOn(false);
      setRecordingSince(null);
      showToast(
        path !== null ? `标签已关闭，录制已自动保存：${path}` : "标签已关闭，录制没能保存成功",
        path === null,
      );
    },
    [showToast],
  );

  // REC 计时：每秒刷新时长文本
  useEffect(() => {
    if (!recordingOn || recordingSince === null) {
      setRecElapsed("");
      return undefined;
    }
    const tick = () => {
      const secs = Math.max(0, Math.floor((Date.now() - recordingSince) / 1000));
      const mm = String(Math.floor(secs / 60)).padStart(2, "0");
      const ss = String(secs % 60).padStart(2, "0");
      setRecElapsed(`${mm}:${ss}`);
    };
    tick();
    const iv = window.setInterval(tick, 1000);
    return () => window.clearInterval(iv);
  }, [recordingOn, recordingSince]);

  return (
    <main className={styles.page}>
      <div className={styles.toolbar}>
        <button
          type="button"
          className={styles.backBtn}
          onClick={goWindows}
          title="返回该设备的窗口列表"
        >
          <IconArrowLeft size={14} />
          窗口列表
        </button>
        <span className={styles.toolbarDivider} aria-hidden="true" />
        <span className={styles.deviceContext} title="当前会话的设备">
          {deviceName ?? "…"}
        </span>
        <span className={styles.spacer} />
        <div
          className={styles.segGroup}
          role="group"
          aria-label="显示模式与缩放"
          style={activeWindow === null ? { display: "none" } : undefined}
          title={
            activeWindow === null ? "先打开一个远程窗口后可用" : undefined
          }
        >
          {(
            [
              ["FIT", "适应"],
              ["SCALE", "等比"],
              ["NATIVE", "原始"],
            ] as const
          ).map(([mode, label]) => (
            <button
              key={mode}
              type="button"
              className={
                snapshot.fit === mode
                  ? `${styles.segBtn} ${styles.segBtnActive}`
                  : styles.segBtn
              }
              disabled={activeWindow === null}
              onClick={() => setFit(mode)}
            >
              {label}
            </button>
          ))}
          <select
            className={styles.zoomSelect}
            aria-label="缩放倍率"
            value={snapshot.zoom}
            disabled={activeWindow === null || snapshot.fit === "NATIVE"}
            title={snapshot.fit === "NATIVE" ? "原始尺寸模式下缩放不适用" : undefined}
            onChange={(e) => setZoom(Number(e.currentTarget.value))}
          >
            {ZOOM_LEVELS.map((z) => (
              <option key={z} value={z}>
                {Math.round(z * 100)}%
              </option>
            ))}
          </select>
        </div>
        {snapshot.notice !== null && (
          <span className={styles.notice}>{snapshot.notice}</span>
        )}
      </div>

      <nav className={styles.tabs} role="tablist" aria-label="会话标签">
        {tabs.map((tab) =>
          tab.kind === "term" ? (
            <TabButton
              key={tab.id}
              title={tab.title ?? `终端 ${shellLabel(tab.shell)}`}
              closeLabel={`关闭 终端 ${shellLabel(tab.shell)}`}
              active={tab.id === activeId}
              onSelect={() => setActiveId(tab.id)}
              onClose={() => closeTab(tab.id)}
            />
          ) : (
            <WindowTabButton
              key={tab.id}
              tab={tab}
              fallbackTitle={
                titleCacheRef.current.get(tab.id) ?? `窗口 ${tab.id}`
              }
              active={tab.id === activeId}
              onSelect={() => setActiveId(tab.id)}
              onClose={() => closeTab(tab.id)}
            />
          ),
        )}
        <div className={styles.termMenuWrap} ref={termMenuRef}>
          <button
            type="button"
            className={styles.toolBtn}
            onClick={() => setTermMenuOpen((v) => !v)}
            title="打开远端终端"
            aria-haspopup="menu"
            aria-expanded={termMenuOpen}
          >
            <IconPlus size={13} />
            新建终端
            <IconChevronDown size={12} className={styles.termCaret} />
          </button>
          {termMenuOpen && (
            <div className={styles.termMenu} role="menu" aria-label="选择终端 shell">
              {TERM_SHELLS.map((shell) => (
                <button
                  key={shell}
                  type="button"
                  className={styles.termMenuItem}
                  role="menuitem"
                  onClick={() => {
                    setTermMenuOpen(false);
                    openTermTab(shell);
                  }}
                >
                  <IconTerminal size={13} />
                  {shellLabel(shell)}
                </button>
              ))}
            </div>
          )}
        </div>
        <button
          type="button"
          className={`${styles.toolBtn} ${styles.sidebarToggle}`}
          title={sidebarOpen ? "收起窗口列表" : "展开窗口列表"}
          aria-expanded={sidebarOpen}
          onClick={() =>
            setSidebarOpen((v) => {
              try {
                window.localStorage.setItem("rd.sidebar", v ? "closed" : "open");
              } catch {
                // 存储不可用仅失去记忆
              }
              return !v;
            })
          }
        >
          <IconSidebar size={14} />
          {sidebarOpen ? "收起列表" : "窗口列表"}
        </button>
      </nav>

      {loopback && (
        <div className={styles.loopbackBar} title="本机控制本机：点击会穿透到真实窗口（该窗口弹到前台），鼠标移动不完全跟随；控制其他电脑时无此限制">
          本机自测模式：点击会穿透到真实窗口，鼠标不完全跟随 — 控制其他电脑时无此限制
        </div>
      )}

      {activeTab !== null && (
        <div className={styles.toolbarRow}>
          <FloatingToolbar
            windowActions={activeWindow !== null}
            onScreenshot={() => void doScreenshot()}
            onFullscreen={() => void doFullscreen()}
            onLockScreen={() => void doLockScreen()}
            onCtrlAltDel={() => void doCtrlAltDel()}
            onQualityChange={(q) => void doQuality(q)}
            quality={quality}
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
          {/* chip 簇右置（R11 D11：曾与 bar 同行挤偏工具栏中心 ~80px） */}
          <div className={styles.chipCluster}>
            {activeWindow !== null && (
              <span
                className={styles.encryptChip}
                title="连接经 QUIC + TLS 加密传输，中转无法窥屏"
              >
                <IconLock size={11} />
                加密
              </span>
            )}
            {activeWindow !== null && snapshot.stats !== null && (
              <span
                className={styles.healthPill}
                data-level={snapshot.stats.rttMs < 50 ? "good" : snapshot.stats.rttMs < 150 ? "fair" : "poor"}
                title={`连接延迟 ${snapshot.stats.rttMs} ms（绿 <50 流畅 · 黄 <150 可用 · 红 卡顿；帧率按 Ctrl+Shift+H 查看）`}
              >
                <span className={styles.healthDot} aria-hidden="true" />
                延迟 {snapshot.stats.rttMs} ms
              </span>
            )}
            {activeTab.kind === "term" &&
              (() => {
                // pill 反映终端真实状态（R11 D04：曾恒显「已连接」，进程退出/
                // 打开失败时说谎）。配色复用延迟胶囊的语义色阶。
                const st = termStatuses[activeTab.id]?.kind ?? "connecting";
                const meta = {
                  connecting: { text: "连接中", level: "fair", tip: "正在打开远端终端…" },
                  open: { text: "已连接", level: "good", tip: "终端会话进行中" },
                  exited: { text: "已退出", level: "poor", tip: "远端进程已退出，可点「重新打开」" },
                  error: { text: "异常", level: "poor", tip: "终端打开失败，见画面内提示" },
                }[st];
                return (
                  <span
                    className={styles.healthPill}
                    data-level={meta.level}
                    title={meta.tip}
                  >
                    <span className={styles.healthDot} aria-hidden="true" />
                    {meta.text}
                  </span>
                );
              })()}
          </div>
        </div>
      )}

      <div className={styles.sessionBody}>
        {sidebarOpen && (
          <WindowSidebar
            deviceId={deviceId}
            openIds={new Set(tabs.filter((t) => t.kind === "window").map((t) => t.id))}
            activeId={activeId}
            viewOnly={sessionCtlRef.current?.viewOnly ?? false}
            onSelect={setActiveId}
            onOpen={openTab}
            onCloseTab={closeTab}
          />
        )}
        <div className={styles.stageArea}>
        {helpOpen && (
          <div
            className={styles.helpOverlay}
            role="dialog"
            aria-label="快捷键速查"
            onClick={() => setHelpOpen(false)}
          >
            <div className={styles.helpCard}>
              <b>快捷键</b>
              <ul>
                <li><kbd>Ctrl+Alt+1..9</kbd> 切换会话标签</li>
                <li><kbd>Ctrl+Shift+H</kbd> 画面信息（延迟/码率/帧率）</li>
                <li><kbd>Ctrl+Shift+S</kbd> 保存当前画面为 PNG</li>
                <li><kbd>Ctrl+Shift+L</kbd> 锁屏（3 秒内再点确认）</li>
                <li><kbd>Ctrl+Shift+V</kbd> 只看模式开关</li>
                <li><kbd>Ctrl+Shift+C</kbd> 取回远端剪贴板</li>
                <li><kbd>Ctrl+Shift+X</kbd> 发送本机剪贴板到远端</li>
                <li><kbd>?</kbd> 打开本速查 · <kbd>Esc</kbd> 关闭</li>
              </ul>
            </div>
          </div>
        )}
        {recordingOn && (
          <div className={styles.recBadge} title="正在录制远程画面（关闭本标签前会先自动保存）">
            <span className={styles.recDot} aria-hidden="true" />
            REC {recElapsed}
          </div>
        )}
        {privacyOn && (
          <button
            type="button"
            className={`${styles.recBadge} ${styles.privacyBadge}`}
            title="对方屏幕当前为黑屏，点击关闭隐私屏"
            onClick={() => void doPrivacy(false)}
          >
            <IconEyeOff size={12} />
            隐私屏已开启 · 点击关闭
          </button>
        )}
        {toast !== null && (
          <div
            className={
              toastError ? `${styles.toastAnchor} ${styles.toastError}` : styles.toastAnchor
            }
            role="status"
          >
            {toast}
          </div>
        )}
        {activeTab === null ? (
          <div className={styles.empty}>
            <p className={styles.emptyTitle}>从左侧列表选择一个远程窗口</p>
            <p className={styles.emptyHint}>
              点击左侧窗口即在远端打开会话；或直接开一个终端。
            </p>
            <div className={styles.emptyActions}>
              <button
                type="button"
                className={styles.toolBtn}
                onClick={() => openTermTab(DEFAULT_TERM_SHELL)}
              >
                <IconTerminal size={14} />
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
              onStatus={(s) => setTermStatus(activeTab.id, s)}
            />
          </div>
        ) : (
          <div className={styles.stage}>
            <WindowGroupView
              key={activeTab.id}
              deviceId={deviceId}
              windowId={activeTab.id}
              store={activeTab.store}
              showHud={hudVisible}
              exportFnRef={exportFnRef}
              recordCtlRef={recordCtlRef}
              sessionCtlRef={sessionCtlRef}
              onRecordingAutoStop={handleRecordingAutoStop}
              onRootGone={() => {
                const title = titleCacheRef.current.get(activeTab.id);
                closeTab(activeTab.id);
                showToast(`对方已关闭${title !== undefined ? `「${title}」` : "该窗口"}，可从左侧列表重新打开`, false);
              }}
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

/** 标签按钮（两类标签共用外观）。容器 div + 两枚同级真 button——
 * button 内嵌 span[role=button] 是 nested-interactive 违规（R11 D15）。 */
function TabButton({
  title,
  closeLabel,
  active,
  onSelect,
  onClose,
}: TabButtonProps) {
  return (
    <div className={active ? styles.tabActive : styles.tab} role="presentation">
      <button
        type="button"
        className={styles.tabMain}
        role="tab"
        aria-selected={active}
        title={title}
        onClick={onSelect}
      >
        <span className={styles.tabTitle}>{title}</span>
      </button>
      <button
        type="button"
        className={styles.tabClose}
        aria-label={closeLabel}
        title={closeLabel}
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

/** 窗口标签按钮：标题取自会话快照（MOVED/TITLE 事件更新），未知名回退窗口列表反查的标题。 */
function WindowTabButton({
  tab,
  fallbackTitle,
  active,
  onSelect,
  onClose,
}: {
  tab: WindowTab;
  fallbackTitle: string;
  active: boolean;
  onSelect: () => void;
  onClose: () => void;
}) {
  const snapshot = useSyncExternalStore(tab.store.subscribe, tab.store.getSnapshot);
  const title = snapshot.title ?? fallbackTitle;
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
