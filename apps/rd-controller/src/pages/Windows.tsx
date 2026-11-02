/**
 * 窗口列表页（T-020）：可捕获窗口网格卡片（图标 / 标题 / 进程名），
 * 搜索过滤（标题 / 进程，大小写不敏感），窗口事件实时增删改（订阅推送，
 * 无轮询）；点击卡片打开会话，“新建终端”下拉直达终端会话。
 *
 * 事件应用与过滤为纯函数（applyWindowEvent / filterWindows），导出供
 * Windows.test.tsx 直接驱动（测试环境无 DOM，见测试文件头注）。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  connectDevice,
  listDevices,
  recordConnect,
  rememberLastDeviceId,
} from "../api/devices";
import {
  listWindows,
  subscribeWindowEvents,
  type WindowDto,
  type WindowEventDto,
} from "../api/windows";
import { IconArrowLeft, IconRefresh, IconSearch } from "../ui/Icon";
import { ShellMenu } from "../ui/ShellMenu";
import styles from "./Windows.module.css";

/**
 * 事件的目标窗口 id：优先 info.id；info 缺省（DESTROYED 载荷可省略）时
 * 回退 root_id。两者皆无返回 null，事件按无操作处理。
 */
export function windowEventId(ev: WindowEventDto): string | null {
  if (ev.info !== null && ev.info.id !== "0") {
    return ev.info.id;
  }
  if (ev.rootId !== "0") {
    return ev.rootId;
  }
  return null;
}

/**
 * 应用一条顶层窗口事件，返回新列表（不可变，不改入参）：
 * - CREATED / TITLE / MOVED：info 原位 upsert（存在则替换且顺序不变，否则追加）；
 * - DESTROYED：按 windowEventId 移除；
 * - CHILD_ADDED / CHILD_REMOVED：只影响组内子窗口，顶层列表不消费。
 * info 缺失或事件不可定位时返回内容等价的拷贝。
 */
export function applyWindowEvent(
  list: readonly WindowDto[],
  ev: WindowEventDto,
): WindowDto[] {
  switch (ev.kind) {
    case "CREATED":
    case "TITLE":
    case "MOVED": {
      const info = ev.info;
      if (info === null) {
        return [...list];
      }
      return list.some((w) => w.id === info.id)
        ? list.map((w) => (w.id === info.id ? info : w))
        : [...list, info];
    }
    case "DESTROYED": {
      const id = windowEventId(ev);
      return id === null ? [...list] : list.filter((w) => w.id !== id);
    }
    default:
      return [...list];
  }
}

/** 搜索过滤：标题 / 进程名子串匹配（大小写不敏感，忽略首尾空白）。 */
export function filterWindows(
  list: readonly WindowDto[],
  query: string,
): WindowDto[] {
  const q = query.trim().toLowerCase();
  if (q === "") {
    return [...list];
  }
  return list.filter(
    (w) =>
      w.title.toLowerCase().includes(q) ||
      w.process.toLowerCase().includes(q),
  );
}

/**
 * 桌面地图布局（R12 设计 P0：窗口页从「进程列表」升级为「那台电脑的桌面」
 * ——WindowDto 已有 x/y/w/h 几何，零后端拿 80% 空间记忆效果）。
 * 虚拟桌面固定 1920×1080 基准，窗口矩形按比例映射为百分比定位（越界裁剪）。
 */
export interface DesktopRect {
  readonly id: string;
  readonly title: string;
  readonly process: string;
  readonly minimized: boolean;
  readonly leftPct: number;
  readonly topPct: number;
  readonly widthPct: number;
  readonly heightPct: number;
  /** 宽度不足 4% 的窗口：收成边缘窄条（icon-only），不再读作「渲染破了」。 */
  readonly narrow: boolean;
}

/** 虚拟桌面基准（主流单屏；多屏超界窗口裁剪到界内）。 */
const VIRTUAL_DESKTOP = { w: 1920, h: 1080 };

export function desktopMap(
  windows: readonly WindowDto[],
): readonly DesktopRect[] {
  return windows.map((w) => {
    const left = Math.min(Math.max(0, w.x), VIRTUAL_DESKTOP.w - 80);
    const top = Math.min(Math.max(0, w.y), VIRTUAL_DESKTOP.h - 60);
    const width = Math.min(Math.max(120, w.w), VIRTUAL_DESKTOP.w - left);
    const height = Math.min(Math.max(80, w.h), VIRTUAL_DESKTOP.h - top);
    const widthPct = (width / VIRTUAL_DESKTOP.w) * 100;
    return {
      id: w.id,
      title: w.title.length > 0 ? w.title : "(无标题窗口)",
      process: w.process,
      minimized: w.minimized,
      leftPct: (left / VIRTUAL_DESKTOP.w) * 100,
      topPct: (top / VIRTUAL_DESKTOP.h) * 100,
      widthPct,
      heightPct: (height / VIRTUAL_DESKTOP.h) * 100,
      narrow: widthPct < 4,
    };
  });
}

/** “新建终端”下拉可用的 shell（?term=<shell> 由会话页 T-021/T-023 消费）。 */
const SHELLS = [
  { id: "pwsh", label: "PowerShell" },
  { id: "cmd", label: "cmd" },
  { id: "wsl", label: "WSL" },
] as const;

export type ShellId = (typeof SHELLS)[number]["id"];

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 图标缺位时的占位字符：进程名去掉 .exe 后取首字符。 */
function fallbackIcon(process: string): string {
  const stem = process.replace(/\.exe$/i, "");
  return stem.length > 0 ? stem.charAt(0).toUpperCase() : "?";
}

type LoadStatus = "connecting" | "loading" | "ready" | "error";

/** 骨架屏占位卡片数（约铺满一屏）。 */
const SKELETONS = [1, 2, 3, 4, 5, 6, 7, 8];

/**
 * 窗口列表页（#/windows/:deviceId）。
 *
 * 装载次序：先确认设备连接（离线或上次失败后，先 connect_device 重建设备
 * 连接；在线则跳过——对在线设备重复 connect 会触发后端连接替换清理竞态），
 * 再订阅事件（订阅与列表返回之间到达的事件先缓冲、列表落地后补放），最后
 * 拉一次列表；此后完全由事件驱动，无轮询。
 */
export function Windows({ deviceId }: { deviceId: string }) {
  const [windows, setWindows] = useState<readonly WindowDto[]>([]);
  const [query, setQuery] = useState("");
  /** 视图切换（R12 设计 P0：默认桌面地图，列表降为次视图）。 */
  const [view, setView] = useState<"map" | "list">("map");
  const [status, setStatus] = useState<LoadStatus>("connecting");
  /** 当前设备名（标题上下文：我在谁的窗口里）。 */
  const [deviceName, setDeviceName] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // 仅最新一期装载（重试 / 切换设备）的事件与列表结果生效
  const seqRef = useRef(0);
  /** 失败自动重试计数（成功后清零）。 */
  const retryRef = useRef(0);
  /** 上次装载失败后置位：重试时强制重建设备连接（应对连接表内失效连接）。 */
  const forceConnectRef = useRef(false);

  /**
   * 订阅窗口事件并拉取一次列表（先订阅后拉列表：订阅与列表返回之间到达
   * 的事件先缓冲，列表落地后补放）。失败向上抛出由 load 统一处理。
   */
  const loadList = useCallback(
    async (seq: number): Promise<void> => {
      const buffered: WindowEventDto[] = [];
      let ready = false;
      const subscription = subscribeWindowEvents(deviceId, (ev) => {
        if (seqRef.current !== seq) {
          return;
        }
        if (ready) {
          setWindows((prev) => applyWindowEvent(prev, ev));
        } else {
          buffered.push(ev);
        }
      });
      const list = await listWindows(deviceId);
      if (seqRef.current !== seq) {
        return;
      }
      setWindows(list);
      ready = true;
      for (const ev of buffered) {
        setWindows((prev) => applyWindowEvent(prev, ev));
      }
      await subscription; // 订阅失败同样进入错误态
    },
    [deviceId],
  );

  const load = useCallback(async () => {
    const seq = seqRef.current + 1;
    seqRef.current = seq;
    const forceConnect = forceConnectRef.current;
    forceConnectRef.current = false;
    setError(null);
    setStatus("connecting");
    try {
      const devices = await listDevices();
      setDeviceName(devices.find((d) => d.id === deviceId)?.name ?? null);
      if (seqRef.current !== seq) {
        return;
      }
      const online = devices.some((d) => d.id === deviceId && d.online);
      if (forceConnect || !online) {
        await connectDevice(deviceId);
        if (seqRef.current !== seq) {
          return;
        }
        // 连接替换竞态自愈（conn.rs put_device 已知问题，见本轮报告）：
        // 若刚建立的连接被旧连接的断线监听清出连接表，立即补连一次——
        // 此时旧连接的监听已消费，第二次连接不会被清除
        const after = await listDevices();
        if (seqRef.current !== seq) {
          return;
        }
        if (!after.some((d) => d.id === deviceId && d.online)) {
          await connectDevice(deviceId);
          if (seqRef.current !== seq) {
            return;
          }
        }
      }
      setStatus("loading");
      await loadList(seq);
      rememberLastDeviceId(deviceId);
      recordConnect(deviceId);
      setStatus("ready");
      retryRef.current = 0;
    } catch (e) {
      if (seqRef.current !== seq) {
        return;
      }
      // 失败后重试强制重建设备连接（避免在失效连接上反复 list_windows）
      forceConnectRef.current = true;
      setError(errText(e));
      setStatus("error");
      // 自动退避重试（2026-10-23 用户反馈：open 失败返回后列表死掉只能手动刷）：
      // 1s/2s/4s 三次，仍失败则停在 error 态等手动「刷新/重试」
      const attempt = retryRef.current + 1;
      if (attempt <= 3) {
        retryRef.current = attempt;
        window.setTimeout(() => {
          if (seqRef.current === seq) {
            void load();
          }
        }, 1000 * 2 ** (attempt - 1));
      }
    }
  }, [deviceId, loadList]);

  // `/` 快捷键聚焦搜索（效率评委 R8）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const typing =
        target !== null &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA");
      if (e.key === "/" && !typing) {
        e.preventDefault();
        document.querySelector<HTMLInputElement>('input[type="search"]')?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    void load();
    // 事件推送之外的兜底：窗口是易变数据，15s 轻轮询保证新开程序最终可见
    const iv = window.setInterval(() => {
      void listWindows(deviceId)
        .then((list) => {
          if (seqRef.current !== 0) {
            setWindows(list);
          }
        })
        .catch(() => undefined);
    }, 15_000);
    return () => window.clearInterval(iv);
  }, [load, deviceId]);

  const openWindow = useCallback(
    (w: WindowDto) => {
      window.location.hash = `#/session/${deviceId}?open=${w.id}`;
    },
    [deviceId],
  );

  const openTerminal = useCallback(
    (shell: ShellId) => {
      window.location.hash = `#/session/${deviceId}?term=${shell}`;
    },
    [deviceId],
  );

  const updateQuery = useCallback((value: string) => {
    setQuery(value);
  }, []);

  const filtered = filterWindows(windows, query);
  const mapRects = desktopMap(filtered);

  return (
    <main className={styles.page}>
      <header className={styles.header}>
        <div className={styles.titleWrap}>
          <button
            type="button"
            className={styles.backBtn}
            title="返回设备列表"
            onClick={() => {
              window.location.hash = "#/devices";
            }}
          >
            <IconArrowLeft size={15} />
          </button>
          <h1 className={styles.title}>
            {deviceName !== null ? `${deviceName} 的窗口` : "窗口"}
          </h1>
        </div>
        <div className={styles.tools}>
          <div className={styles.searchWrap}>
            <IconSearch size={14} className={styles.searchIcon} />
            <input
              className={styles.search}
              type="search"
              placeholder="搜索标题或进程名（按 / 聚焦）…"
              aria-label="搜索窗口"
              value={query}
              onChange={(e) => updateQuery(e.currentTarget.value)}
            />
          </div>
          <button
            type="button"
            className={styles.retry}
            title="重新拉取被控端窗口列表（事件推送丢失时使用）"
            onClick={() => void load()}
          >
            <IconRefresh size={13} />
            刷新
          </button>
          <ShellMenu shells={SHELLS} onPick={openTerminal} variant="primary" />
        </div>
      </header>

      {error !== null && (
        <p className={styles.error}>
          {error}{" "}
          <button type="button" className={styles.retry} onClick={() => void load()}>
            重试
          </button>
        </p>
      )}

      {status === "connecting" && (
        <ul className={styles.grid} aria-label="连接中">
          {SKELETONS.map((i) => (
            <li key={i}>
              <div className={`${styles.card} ${styles.skeleton}`}>
                <span className={`${styles.icon} ${styles.skeletonBlock}`} />
                <span className={styles.meta}>
                  <span className={`${styles.skeletonBlock} ${styles.skTitle}`} />
                  <span className={`${styles.skeletonBlock} ${styles.skProc}`} />
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}

      {status === "loading" && (
        <ul className={styles.grid} aria-label="正在获取窗口列表">
          {SKELETONS.map((i) => (
            <li key={i}>
              <div className={`${styles.card} ${styles.skeleton}`}>
                <span className={`${styles.icon} ${styles.skeletonBlock}`} />
                <span className={styles.meta}>
                  <span className={`${styles.skeletonBlock} ${styles.skTitle}`} />
                  <span className={`${styles.skeletonBlock} ${styles.skProc}`} />
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}

      {status === "ready" && windows.length === 0 && (
        <p className={styles.empty}>未发现可捕获窗口（远端窗口列表为空）。</p>
      )}

      {status === "ready" && windows.length > 0 && filtered.length === 0 && (
        <p className={styles.empty}>没有匹配「{query}」的窗口。</p>
      )}

      {status === "ready" && windows.length > 0 && (
        <div className={styles.listHead}>
          <p className={styles.countLine}>
            {windows.length} 个窗口
            {windows.some((w) => w.minimized) &&
              ` · ${windows.filter((w) => w.minimized).length} 个已最小化`}
          </p>
          <div className={styles.viewToggle} role="group" aria-label="视图切换">
            <button
              type="button"
              className={view === "map" ? styles.viewBtnOn : styles.viewBtn}
              aria-pressed={view === "map"}
              onClick={() => setView("map")}
            >
              桌面
            </button>
            <button
              type="button"
              className={view === "list" ? styles.viewBtnOn : styles.viewBtn}
              aria-pressed={view === "list"}
              onClick={() => setView("list")}
            >
              列表
            </button>
          </div>
        </div>
      )}

      {status === "ready" && view === "map" && windows.length > 0 && filtered.length > 0 && (
        <div
          className={styles.desktopMap}
          aria-label="远端桌面窗口布局（点击窗口打开会话）。按 1920×1080 虚拟桌面摆位，远端为多屏或超宽分辨率时布局仅供参考。"
          title="按 1920×1080 虚拟桌面摆位；远端为多屏或超宽分辨率时布局仅供参考，窗口位置以列表视图为准"
        >
          {mapRects.map((r) => (
            <button
              key={r.id}
              type="button"
              className={`${styles.mapWin} ${r.narrow ? styles.mapWinEdge : ""}`}
              data-minimized={r.minimized}
              style={{
                left: `${r.leftPct}%`,
                top: `${r.topPct}%`,
                width: r.narrow ? "22px" : `${r.widthPct}%`,
                height: r.narrow ? `${r.heightPct}%` : `${r.heightPct}%`,
              }}
              title={`${r.title} — 打开远程会话`}
              onClick={() => {
                const w = filtered.find((x) => x.id === r.id);
                if (w !== undefined) {
                  openWindow(w);
                }
              }}
            >
              <span className={styles.mapWinBar}>
                {(() => {
                  const w = filtered.find((x) => x.id === r.id);
                  const icon = w?.icon ?? null;
                  return icon !== null ? (
                    <img src={icon} alt="" width={14} height={14} />
                  ) : (
                    <span className={styles.mapWinLetter}>
                      {fallbackIcon(r.process)}
                    </span>
                  );
                })()}
                <span className={styles.mapWinTitle}>{r.title}</span>
              </span>
              {r.minimized && (
                <span className={styles.mapWinMin}>最小化</span>
              )}
            </button>
          ))}
        </div>
      )}

      <ul className={styles.grid} style={status === "ready" && view === "map" ? { display: "none" } : undefined}>
        {filtered.map((w) => (
          <li key={w.id}>
            <button
              type="button"
              className={styles.card}
              data-minimized={w.minimized}
              title="打开远程会话"
              onClick={() => openWindow(w)}
            >
              <span className={styles.icon}>
                {w.icon !== null ? (
                  <img src={w.icon} alt="" width={32} height={32} />
                ) : (
                  <span className={styles.iconFallback}>
                    {fallbackIcon(w.process)}
                  </span>
                )}
              </span>
              <span className={styles.meta}>
                <span className={styles.winTitle}>
                  {w.title.length > 0 ? w.title : "(无标题窗口)"}
                </span>
                <span className={styles.process}>{w.process}</span>
              </span>
              {w.minimized && (
                <span className={styles.restoreHint}>最小化</span>
              )}
            </button>
          </li>
        ))}
      </ul>
    </main>
  );
}

