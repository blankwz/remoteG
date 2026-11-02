/**
 * 文件传输面板（T-031）：会话内浮层。
 *
 * 两个动作：发送（本机文件 → 被控端 Downloads\rd接收）与取回（远端路径
 * → 本机下载目录，重名自动 " (n)" 去重）。远端浏览从主目录起步，点目录
 * 进入；点文件先弹「确认取回到 <目录>」（P0-1：曾直接落盘无确认），可改
 * 目录（Tauri 目录选择对话框），选择记忆到 localStorage；进度条目随传输
 * 完成保留（done/error 定格），取回完成条目提供「打开文件夹」（Tauri
 * opener reveal 定位落盘文件；浏览器 demo 环境为禁用态）。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
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

/** 取回落盘目录的记忆键（localStorage）。 */
const PULL_DIR_KEY = "rd.pullDir";

/** 「确认取回」待确认条目（点文件后先确认落盘位置，不再直接传输）。 */
interface PendingPull {
  /** 文件名（展示用）。 */
  name: string;
  /** 远端完整路径（取回用）。 */
  full: string;
}

/** 单条传输进度行。 */
interface TransferRow {
  xferId: number;
  name: string;
  bytes: number;
  total: number;
  phase: string;
  message: string | null;
  /** 方向（首条进度时定格）：send=发送到远端，pull=取回到本机。 */
  direction: "send" | "pull";
  /** 本机落盘完整路径（仅 pull 行有；「打开文件夹」定位用）。 */
  localPath: string | null;
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
  // 拉取的落盘目录：本机下载目录（Tauri 运行时才有；demo 模式为演示占位）。
  // P0-1：取回前先确认落盘位置，选择记忆到 localStorage
  const [destDir, setDestDir] = useState("demo-downloads");
  /** 待确认取回的文件（null = 无待确认）。 */
  const [pendingPull, setPendingPull] = useState<PendingPull | null>(null);
  /** destDir 的同步镜像：进度回调（useCallback 恒定身份）读最新落盘目录。 */
  const destDirRef = useRef(destDir);
  destDirRef.current = destDir;

  useDismiss(true, rootRef, onClose, { trap: true });

  // 落盘目录回显：优先记忆的选择，无记忆才取本机下载目录（demo 模式占位）
  useEffect(() => {
    if (!isTauriRuntime()) {
      return;
    }
    let saved: string | null = null;
    try {
      saved = window.localStorage.getItem(PULL_DIR_KEY);
    } catch {
      // localStorage 不可用仅失去记忆
    }
    if (saved !== null && saved.length > 0) {
      setDestDir(saved);
      return;
    }
    downloadDir()
      .then(setDestDir)
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
      const idx = prev.findIndex((r) => r.xferId === p.xferId);
      const known = idx === -1 ? undefined : prev[idx];
      // 方向与落盘路径只在首条进度时定格（phase=done 无法判方向，沿用首条）
      const direction: TransferRow["direction"] =
        known?.direction ?? (p.phase === "sending" ? "send" : "pull");
      const localPath =
        known?.localPath ??
        (direction === "pull" ? joinPath(destDirRef.current, p.name) : null);
      const row: TransferRow = {
        xferId: p.xferId,
        name: p.name,
        bytes: p.bytes,
        total: p.total,
        phase: p.phase,
        message: p.message,
        direction,
        localPath,
      };
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
    const picked = await tauriOpen({ multiple: true, title: "选择要发送的文件" });
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

  /** 取回确认：点文件先挂待确认条目（P0-1——曾点击即落盘无确认）。 */
  const onRequestPull = useCallback(
    (entry: FileEntryDto): void => {
      setPendingPull({
        name: entry.name,
        full: path === "" ? entry.name : joinPath(path, entry.name),
      });
    },
    [path],
  );

  /** 更改落盘目录（Tauri 目录选择对话框；demo 模式无对话框，保持现状）。 */
  const onChangeDest = useCallback(async (): Promise<void> => {
    if (!isTauriRuntime()) {
      return;
    }
    try {
      const picked = await tauriOpen({
        directory: true,
        multiple: false,
        title: "选择保存位置",
        defaultPath: destDir,
      });
      if (typeof picked === "string" && picked.length > 0) {
        setDestDir(picked);
      }
    } catch (e) {
      setError(
        `选择目录没有成功：${e instanceof Error ? e.message : String(e)}。可直接确认取回当前显示的目录`,
      );
    }
  }, [destDir]);

  /** 确认取回：记忆本次目录 → 发起传输（进度条与取消钮沿用）。 */
  const onConfirmPull = useCallback(async (): Promise<void> => {
    if (pendingPull === null) {
      return;
    }
    const target = pendingPull;
    setPendingPull(null);
    try {
      window.localStorage.setItem(PULL_DIR_KEY, destDir);
    } catch {
      // localStorage 不可用仅失去记忆，不影响本次传输
    }
    try {
      await filePull(deviceId, target.full, destDir, applyProgress);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [pendingPull, destDir, deviceId, applyProgress]);

  /** 点目录 → 进入；点文件 → 弹取回确认。 */
  const onEntry = useCallback(
    (entry: FileEntryDto): void => {
      if (entry.dir) {
        void browse(joinPath(path, entry.name));
      } else {
        onRequestPull(entry);
      }
    },
    [path, browse, onRequestPull],
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

  /** 取回完成的条目：在文件管理器中定位落盘文件（Tauri opener reveal；
   * 失败如实报错，不打断面板）。 */
  const onOpenFolder = useCallback((t: TransferRow): void => {
    if (t.localPath === null) {
      return;
    }
    revealItemInDir(t.localPath).catch((e: unknown) => {
      setError(
        `打开文件夹没有成功：${e instanceof Error ? e.message : String(e)}`,
      );
    });
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
        {pendingPull !== null && (
          <div className={styles.pullConfirm} role="region" aria-label="确认取回位置">
            <div className={styles.pullConfirmText}>
              取回「{pendingPull.name}」保存到：
              <code className={styles.pullConfirmPath}>{destDir}</code>
            </div>
            <div className={styles.pullConfirmActions}>
              {isTauriRuntime() && (
                <button
                  type="button"
                  className={styles.miniBtn}
                  onClick={() => void onChangeDest()}
                >
                  更改…
                </button>
              )}
              <button
                type="button"
                className={styles.confirmBtn}
                onClick={() => void onConfirmPull()}
              >
                确认取回
              </button>
              <button
                type="button"
                className={styles.miniBtn}
                onClick={() => setPendingPull(null)}
              >
                取消
              </button>
            </div>
          </div>
        )}
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
                  {t.phase === "done" && t.direction === "pull" && (
                    <button
                      type="button"
                      className={styles.miniBtn}
                      title={
                        isTauriRuntime()
                          ? "在文件管理器中显示该文件"
                          : "浏览器演示环境不可用"
                      }
                      disabled={!isTauriRuntime()}
                      onClick={() => onOpenFolder(t)}
                    >
                      打开文件夹
                    </button>
                  )}
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
