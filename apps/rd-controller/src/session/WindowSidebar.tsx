/**
 * 会话内窗口列表侧边栏（T-028 F4）：远端可捕获窗口的实时清单，点击即看。
 * 向日葵同款交互——不用回到「窗口列表」页再开一次，会话中直接切换。
 *
 * 数据：5s 轮询 `list_windows`（协议与窗口列表页同源）；条目上标注「已打开」，
 * 点击已打开条目 = 切换标签，未打开 = 新开。搜索框过滤标题/进程名。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { listWindows, type WindowDto } from "../api/windows";
import styles from "./WindowSidebar.module.css";

interface Props {
  deviceId: string;
  /** 已打开的窗口标签 id（高亮 + 「已打开」标记）。 */
  openIds: ReadonlySet<string>;
  /** 当前激活标签（更强高亮）。 */
  activeId: string | null;
  onSelect: (windowId: string) => void;
  onOpen: (windowId: string) => void;
  onCloseTab: (windowId: string) => void;
}

/** 轮询周期：远端窗口的开关以秒级延迟出现即可（组事件推送是标签内的事）。 */
const POLL_MS = 5000;

export function WindowSidebar({ deviceId, openIds, activeId, onSelect, onOpen, onCloseTab }: Props) {
  const [windows, setWindows] = useState<readonly WindowDto[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

  const refresh = useCallback(async () => {
    try {
      const list = await listWindows(deviceId);
      setWindows(list);
      setError(null);
    } catch (e) {
      // 轮询失败不打断会话：显示一行提示，下个周期重试
      setError(String(e));
    }
  }, [deviceId]);

  useEffect(() => {
    void refresh();
    const iv = window.setInterval(() => void refresh(), POLL_MS);
    return () => window.clearInterval(iv);
  }, [refresh]);

  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (q === "") {
      return windows;
    }
    return windows.filter(
      (w) => w.title.toLowerCase().includes(q) || w.process.toLowerCase().includes(q),
    );
  }, [windows, filter]);

  return (
    <aside className={styles.sidebar}>
      <div className={styles.head}>
        <b className={styles.title}>远程窗口</b>
        <button type="button" className={styles.refresh} title="刷新" onClick={() => void refresh()}>
          ⟳
        </button>
      </div>
      <input
        className={styles.search}
        placeholder="搜索标题 / 进程…"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      {error !== null && <div className={styles.error}>列表获取失败，重试中…</div>}
      <div className={styles.list}>
        {shown.length === 0 && error === null && (
          <div className={styles.empty}>没有可捕获的窗口</div>
        )}
        {shown.map((w) => {
          const opened = openIds.has(w.id);
          const active = activeId === w.id;
          return (
            <div
              key={w.id}
              className={`${styles.item} ${active ? styles.active : ""}`}
              onClick={() => (opened ? onSelect(w.id) : onOpen(w.id))}
              title={opened ? "切换到这个窗口" : "打开这个窗口"}
            >
              {w.icon !== null ? (
                <img className={styles.icon} src={w.icon} alt="" />
              ) : (
                <span className={`${styles.icon} ${styles.iconPh}`}>▢</span>
              )}
              <span className={styles.meta}>
                <span className={styles.name}>{w.title || "(无标题)"}</span>
                <span className={styles.proc}>{w.process}</span>
              </span>
              {opened && (
                <button
                  type="button"
                  className={styles.close}
                  title="关闭这个标签"
                  onClick={(e) => {
                    e.stopPropagation();
                    onCloseTab(w.id);
                  }}
                >
                  ×
                </button>
              )}
              {opened && <span className={styles.badge}>{active ? "正在看" : "已打开"}</span>}
            </div>
          );
        })}
      </div>
    </aside>
  );
}
