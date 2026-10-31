/**
 * 会话内窗口列表侧边栏（T-028 F4）：远端可捕获窗口的实时清单，点击即看。
 * 向日葵同款交互——不用回到「窗口列表」页再开一次，会话中直接切换。
 *
 * 数据：5s 轮询 `list_windows`（协议与窗口列表页同源）；条目上标注
 * 「控制中/只看中/已打开」（激活标签按真实 viewOnly 态措辞，R11 D05——
 * 曾一律标「只看中」，与真实只看模式撞词且语义相反）；点击已打开条目 =
 * 切换标签，未打开 = 新开。搜索框过滤标题/进程名。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { listWindows, type WindowDto } from "../api/windows";
import { IconRefresh } from "../ui/Icon";
import styles from "./WindowSidebar.module.css";

interface Props {
  deviceId: string;
  /** 已打开的窗口标签 id（高亮 + 「已打开」标记）。 */
  openIds: ReadonlySet<string>;
  /** 当前激活标签（更强高亮）。 */
  activeId: string | null;
  /** 激活窗口标签当前是否只看中（决定徽标措辞：控制中 vs 只看中）。 */
  viewOnly: boolean;
  onSelect: (windowId: string) => void;
  onOpen: (windowId: string) => void;
  onCloseTab: (windowId: string) => void;
}

/** 轮询周期：远端窗口的开关以秒级延迟出现即可（组事件推送是标签内的事）。 */
const POLL_MS = 5000;

export function WindowSidebar({
  deviceId,
  openIds,
  activeId,
  viewOnly,
  onSelect,
  onOpen,
  onCloseTab,
}: Props) {
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
    const filtered =
      q === ""
        ? windows
        : windows.filter(
            (w) => w.title.toLowerCase().includes(q) || w.process.toLowerCase().includes(q),
          );
    // 已打开的窗口置顶（正在看的最前）——多窗口工作流一眼可达
    return [...filtered].sort((a, b) => {
      const aOpen = openIds.has(a.id) ? (activeId === a.id ? 2 : 1) : 0;
      const bOpen = openIds.has(b.id) ? (activeId === b.id ? 2 : 1) : 0;
      return bOpen - aOpen;
    });
  }, [windows, filter, openIds, activeId]);

  return (
    <aside className={styles.sidebar}>
      <div className={styles.head}>
        <b className={styles.title}>远程窗口</b>
        <button
          type="button"
          className={styles.refresh}
          title="刷新"
          aria-label="刷新窗口列表"
          onClick={() => void refresh()}
        >
          <IconRefresh size={14} />
        </button>
      </div>
      <input
        className={styles.search}
        placeholder="搜索标题或进程名…"
        aria-label="按标题或进程名搜索窗口"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      {error !== null && <div className={styles.error}>列表获取失败，重试中…</div>}
      <div className={styles.list}>
        {shown.length === 0 && error === null && (
          <div className={styles.empty}>
            {filter.trim().length > 0
              ? `没有匹配「${filter.trim()}」的窗口（共 ${windows.length} 个）`
              : "没有可捕获的窗口"}
          </div>
        )}
        {shown.map((w) => {
          const opened = openIds.has(w.id);
          const active = activeId === w.id;
          // 徽标措辞与真实控制权一致（R11 D05）：
          // 激活 + 只看态 → 「只看中」；激活 + 控制态 → 「控制中」；其余 → 「已打开」
          const badge = active ? (viewOnly ? "只看中" : "控制中") : "已打开";
          return (
            <div
              key={w.id}
              className={`${styles.item} ${active ? styles.active : ""}`}
            >
              <button
                type="button"
                className={styles.itemMain}
                onClick={() => (opened ? onSelect(w.id) : onOpen(w.id))}
                title={opened ? "切换到这个窗口" : "打开这个窗口"}
              >
                {w.icon !== null ? (
                  <img className={styles.icon} src={w.icon} alt="" />
                ) : (
                  <span className={`${styles.icon} ${styles.iconPh}`} aria-hidden="true">
                    <span className={styles.iconLetter}>
                      {(w.process.replace(/\.exe$/i, "").charAt(0) || "?").toUpperCase()}
                    </span>
                  </span>
                )}
                <span className={styles.meta}>
                  <span className={styles.name}>{w.title || "(无标题)"}</span>
                  <span className={styles.proc}>{w.process}</span>
                </span>
                {w.minimized && <span className={styles.badge}>最小化</span>}
                {opened && <span className={styles.badge}>{badge}</span>}
              </button>
              {opened && !active && (
                // 真 button 兄弟节点（R11 D15——button 内嵌 span[role=button]
                // 是 nested-interactive 违规）
                <button
                  type="button"
                  className={styles.close}
                  title="关闭这个标签"
                  aria-label={`关闭 ${w.title || w.process}`}
                  onClick={() => {
                    onCloseTab(w.id);
                  }}
                >
                  ×
                </button>
              )}
            </div>
          );
        })}
      </div>
    </aside>
  );
}
