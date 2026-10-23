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
  rememberLastDeviceId,
} from "../api/devices";
import {
  listWindows,
  subscribeWindowEvents,
  type WindowDto,
  type WindowEventDto,
} from "../api/windows";
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
  const [status, setStatus] = useState<LoadStatus>("connecting");
  const [error, setError] = useState<string | null>(null);
  // 仅最新一期装载（重试 / 切换设备）的事件与列表结果生效
  const seqRef = useRef(0);
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
      setStatus("ready");
    } catch (e) {
      if (seqRef.current !== seq) {
        return;
      }
      // 失败后重试强制重建设备连接（避免在失效连接上反复 list_windows）
      forceConnectRef.current = true;
      setError(errText(e));
      setStatus("error");
    }
  }, [deviceId, loadList]);

  useEffect(() => {
    void load();
  }, [load]);

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

  return (
    <main className={styles.page}>
      <header className={styles.header}>
        <h1 className={styles.title}>窗口</h1>
        <div className={styles.tools}>
          <button
            type="button"
            className={styles.retry}
            title="重新拉取被控端窗口列表（事件推送丢失时使用）"
            onClick={() => void load()}
          >
            刷新
          </button>
          <input
            className={styles.search}
            type="search"
            placeholder="搜索标题或进程名…"
            aria-label="搜索窗口"
            value={query}
            onChange={(e) => updateQuery(e.currentTarget.value)}
          />
          <details className={styles.termMenu}>
            <summary className={styles.termBtn}>新建终端</summary>
            <div className={styles.termItems}>
              {SHELLS.map((s) => (
                <button
                  key={s.id}
                  type="button"
                  className={styles.termItem}
                  onClick={(e) => {
                    e.currentTarget.closest("details")?.removeAttribute("open");
                    openTerminal(s.id);
                  }}
                >
                  {s.label}
                </button>
              ))}
            </div>
          </details>
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

      {status === "connecting" && <p className={styles.empty}>连接中…</p>}

      {status === "loading" && <p className={styles.empty}>正在获取窗口列表…</p>}

      {status === "ready" && windows.length === 0 && (
        <p className={styles.empty}>未发现可捕获窗口（远端窗口列表为空）。</p>
      )}

      {status === "ready" && windows.length > 0 && filtered.length === 0 && (
        <p className={styles.empty}>没有匹配「{query}」的窗口。</p>
      )}

      <ul className={styles.grid}>
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
                <span className={styles.restoreHint}>将被还原</span>
              )}
            </button>
          </li>
        ))}
      </ul>
    </main>
  );
}

