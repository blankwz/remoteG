/**
 * 文件传输面板（T-031）：会话内浮层。
 *
 * 两个动作：发送（本机文件 → 被控端 Downloads\rd接收）与取回（远端路径
 * → 本机下载目录，重名自动 " (n)" 去重）。远端浏览从主目录起步，点目录
 * 进入、点文件拉取；进度条目随传输完成保留（done/error 定格）。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { downloadDir } from "@tauri-apps/api/path";
import {
  fileCancel,
  fileList,
  filePull,
  fileSend,
  type FileEntryDto,
  type XferProgress,
} from "../api/files";
import { useDismiss } from "../ui/useDismiss";
import { IconArrowLeft, IconClose, IconRefresh } from "../ui/Icon";
import styles from "./FilesPanel.module.css";
import { isTauriRuntime } from "../api/bridge";

/** 字节数 → 人话大小。 */
function fmtSize(n: number): string {
  if (n >= 1073741824) return `${(n / 1073741824).toFixed(1)} GB`;
  if (n >= 1048576) return `${(n / 1048576).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${n} B`;
}

interface Props {
  deviceId: string;
  onClose: () => void;
}

/** 单条传输进度行。 */
interface TransferRow {
  xferId: number;
  name: string;
  bytes: number;
  total: number;
  phase: string;
  message: string | null;
}

export function FilesPanel(props: Props) {
  const deviceId = props.deviceId;
  const onClose = props.onClose;
  const [entries, setEntries] = useState<FileEntryDto[] | null>(null);
  const [path, setPath] = useState("");
  const [inputValue, setInputValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [transfers, setTransfers] = useState<TransferRow[]>([]);
  const rootRef = useRef<HTMLDivElement | null>(null);
  // 拉取的落盘目录：本机下载目录（Tauri 运行时才有；demo 模式为演示占位）
  const destDirRef = useRef<string>("demo-downloads");

  useDismiss(true, rootRef, onClose, { trap: true });

  useEffect(() => {
    if (!isTauriRuntime()) {
      return;
    }
    downloadDir()
      .then((d) => {
        destDirRef.current = d;
      })
      .catch(() => undefined);
  }, []);

  const browse = useCallback(async (p: string): Promise<void> => {
    setBusy(true);
    setError(null);
    try {
      // path 必须以被控端回传的规范化绝对路径为准（H1）：空请求返回主目录
      // 的绝对路径，用它拼接子路径；用相对名请求会被 Agent 拒绝
      const result = await fileList(p);
      setEntries(result.entries);
      setPath(result.path);
      setInputValue(result.path);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  // 首次打开列主目录
  useEffect(() => {
    void browse("");
  }, [browse]);

  const applyProgress = useCallback((p: XferProgress): void => {
    setTransfers((prev) => {
      const row: TransferRow = {
        xferId: p.xferId,
        name: p.name,
        bytes: p.bytes,
        total: p.total,
        phase: p.phase,
        message: p.message,
      };
      const idx = prev.findIndex((r) => r.xferId === p.xferId);
      let next: TransferRow[];
      if (idx === -1) {
        next = prev.concat([row]);
      } else {
        next = prev.slice();
        next[idx] = row;
      }
      // 条目上限（L5）：done/error 定格不自动清理，长会话防无限增长
      return next.length > 30 ? next.slice(next.length - 30) : next;
    });
  }, []);

  /** 发送：对话框选本机文件（多选）→ 推送，进度入列表。 */
  const onSend = useCallback(async (): Promise<void> => {
    const picked = await openFileDialog({ multiple: true, title: "选择要发送的文件" });
    if (picked === null) {
      return;
    }
    const paths = Array.isArray(picked) ? picked : [picked];
    if (paths.length === 0) {
      return;
    }
    try {
      await fileSend(deviceId, paths, applyProgress);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [deviceId, applyProgress]);

  /** 取回：点击远端文件 → 写本机下载目录。 */
  const onPull = useCallback(
    async (entry: FileEntryDto): Promise<void> => {
      const full = path === "" ? entry.name : joinPath(path, entry.name);
      try {
        await filePull(deviceId, full, destDirRef.current, applyProgress);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    },
    [path, deviceId, applyProgress],
  );

  /** 点目录 → 进入；点文件 → 拉取。 */
  const onEntry = useCallback(
    (entry: FileEntryDto): void => {
      if (entry.dir) {
        void browse(joinPath(path, entry.name));
      } else {
        void onPull(entry);
      }
    },
    [path, browse, onPull],
  );

  /** 上级目录（盘根特判：`C:\x` 的上级是 `C:\`，不是 `C:`——后者不是绝对路径）。 */
  const goUp = useCallback((): void => {
    if (path === "") {
      return;
    }
    const parts = path.split(/[\\/]/).filter((s) => s.length > 0);
    parts.pop();
    if (parts.length === 0) {
      void browse("");
      return;
    }
    const last = parts[parts.length - 1] ?? "";
    const parent =
      last.length === 2 && last.endsWith(":") ? `${last}\\` : parts.join("\\");
    void browse(parent);
  }, [path, browse]);

  const rowClass = useCallback((t: TransferRow): string => {
    if (t.phase === "done") {
      return `${styles.transferRow} ${styles.transferDone}`;
    }
    if (t.phase === "error") {
      return `${styles.transferRow} ${styles.transferError}`;
    }
    return styles.transferRow;
  }, []);

  return (
    <div className={styles.overlay} role="presentation">
      <div className={styles.panel} ref={rootRef} role="dialog" aria-label="文件传输">
        <div className={styles.header}>
          <b className={styles.title}>文件传输</b>
          <button
            type="button"
            className={styles.closeBtn}
            title="关闭"
            aria-label="关闭文件传输面板"
            onClick={onClose}
          >
            <IconClose size={14} />
          </button>
        </div>
        <div className={styles.actions}>
          <button type="button" className={styles.actionBtn} onClick={() => void onSend()}>
            发送文件到远端…
          </button>
        </div>
        <div className={styles.browserRow}>
          <button
            type="button"
            className={styles.miniBtn}
            title="上级目录"
            aria-label="上级目录"
            disabled={path === "" || busy}
            onClick={goUp}
          >
            <IconArrowLeft size={13} />
          </button>
          <input
            className={styles.pathInput}
            value={inputValue}
            placeholder="远端绝对路径（空 = 主目录），回车打开"
            aria-label="远端路径"
            spellCheck={false}
            onChange={(e) => setInputValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                void browse(inputValue);
              }
            }}
          />
          <button
            type="button"
            className={styles.miniBtn}
            title="刷新"
            aria-label="刷新目录"
            disabled={busy}
            onClick={() => void browse(path)}
          >
            <IconRefresh size={13} />
          </button>
        </div>
        {error !== null && <div className={styles.error}>{error}</div>}
        <div className={styles.list} role="list">
          {entries === null ? (
            <div className={styles.empty}>{busy ? "读取中…" : ""}</div>
          ) : entries.length === 0 ? (
            <div className={styles.empty}>（空目录）</div>
          ) : (
            entries.map((entry) => (
              <button
                key={entry.name}
                type="button"
                className={styles.row}
                onClick={() => onEntry(entry)}
              >
                <span className={styles.rowName}>{entry.name}</span>
                <span className={styles.rowSize}>
                  {entry.dir ? "目录" : fmtSize(entry.size)}
                </span>
              </button>
            ))
          )}
        </div>
        {transfers.length > 0 && (
          <div className={styles.transfers}>
            {transfers.map((t) => (
              <div key={t.xferId} className={rowClass(t)}>
                <div className={styles.transferTop}>
                  <span>
                    {t.phase === "sending" ? "发送" : "取回"} {t.name}
                  </span>
                  <span>
                    {fmtSize(t.bytes)}
                    {t.total > 0 ? ` / ${fmtSize(t.total)}` : ""}
                  </span>
                  <button
                    type="button"
                    className={styles.miniBtn}
                    title="取消传输"
                    aria-label={`取消 ${t.name} 的传输`}
                    disabled={t.phase === "done" || t.phase === "error"}
                    onClick={() => void fileCancel(deviceId, t.xferId)}
                  >
                    <IconClose size={11} />
                  </button>
                </div>
                <div className={styles.transferBar}>
                  <div
                    className={styles.transferFill}
                    style={{
                      width:
                        t.total > 0
                          ? `${Math.min(100, (t.bytes / t.total) * 100)}%`
                          : t.phase === "done"
                            ? "100%"
                            : "30%",
                    }}
                  />
                </div>
                {t.message !== null && <div className={styles.transferMsg}>{t.message}</div>}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

/** 远端路径拼接（Windows 反斜杠；主目录相对名直接接）。 */
function joinPath(base: string, name: string): string {
  if (base === "") {
    return name;
  }
  if (base.endsWith("\\") || base.endsWith("/")) {
    return base + name;
  }
  return `${base}\\${name}`;
}
