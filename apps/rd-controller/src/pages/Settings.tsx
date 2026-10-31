/**
 * 设置页（R13 设计#4 整块）：把散落各处的真实偏好收进一个可发现的面——
 * 画质默认（曾埋在会话工具栏）/ Cmd→Ctrl（曾埋在 HUD）/ 弱网纠错（曾埋在
 * 设备页 hostCard）/ 终端字号（曾只有终端内快捷键）/ 自定义常用语管理
 * （曾只进不出）。全部接现有状态与存储，无新后端。
 */
import { useCallback, useState } from "react";
import { call } from "../api/bridge";
import {
  CMD_TO_CTRL_KEY,
  loadCmdToCtrl,
  saveCmdToCtrl,
} from "../input/InputMapper";
import {
  FONT_SIZE_MAX,
  FONT_SIZE_MIN,
  loadFontSize,
  saveFontSize,
} from "../term/TermView";
import { Switch } from "../ui/Switch";
import styles from "./Settings.module.css";

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

type Quality = "auto" | "high" | "medium" | "low";
const QUALITY_KEY = "rd.quality";
const QUALITY_LABEL: Record<Quality, string> = {
  auto: "自动（智能自适应）",
  high: "高清",
  medium: "均衡",
  low: "流畅（弱网）",
};
const PHRASES_KEY = "rd-phrases";

function readQuality(): Quality {
  const saved = window.localStorage.getItem(QUALITY_KEY);
  return saved === "high" || saved === "medium" || saved === "low" ? saved : "auto";
}

function readPhrases(): readonly string[] {
  try {
    const parsed: unknown = JSON.parse(window.localStorage.getItem(PHRASES_KEY) ?? "[]");
    return Array.isArray(parsed) ? (parsed as string[]) : [];
  } catch {
    return [];
  }
}

/** macOS 才显示 Cmd→Ctrl 行（Windows 上是无条件噪音）。 */
const IS_MAC =
  typeof navigator !== "undefined" &&
  (navigator.platform?.toLowerCase().includes("mac") ||
    navigator.userAgent.includes("Mac OS X"));

export function Settings() {
  const [quality, setQuality] = useState<Quality>(readQuality);
  const [cmdToCtrl, setCmdToCtrl] = useState<boolean>(() => loadCmdToCtrl());
  const [fec, setFec] = useState<boolean | null>(null);
  const [fontPx, setFontPx] = useState<number>(() => loadFontSize());
  const [phrases, setPhrases] = useState<readonly string[]>(readPhrases);
  const [fecError, setFecError] = useState<string | null>(null);

  void call<boolean>("get_media_fec")
    .then(setFec)
    .catch(() => setFec(null));

  const onQuality = useCallback((q: Quality) => {
    setQuality(q);
    window.localStorage.setItem(QUALITY_KEY, q);
  }, []);

  const onCmdToCtrl = useCallback((v: boolean) => {
    setCmdToCtrl(v);
    saveCmdToCtrl(v);
    try {
      // HUD 内的同一偏好（InputMapper CMD_TO_CTRL_KEY），两处入口一份事实
      window.localStorage.setItem(CMD_TO_CTRL_KEY, v ? "1" : "0");
    } catch {
      // 存储不可用仅失去持久化
    }
  }, []);

  const onFec = useCallback(async (v: boolean) => {
    setFecError(null);
    try {
      await call<void>("set_media_fec", { enabled: v });
      setFec(v);
    } catch (e) {
      setFecError(errText(e));
    }
  }, []);

  const onFont = useCallback((delta: number) => {
    const next = Math.min(FONT_SIZE_MAX, Math.max(FONT_SIZE_MIN, fontPx + delta));
    setFontPx(next);
    saveFontSize(next);
  }, [fontPx]);

  const removePhrase = useCallback((text: string) => {
    const next = readPhrases().filter((p) => p !== text);
    setPhrases(next);
    try {
      window.localStorage.setItem(PHRASES_KEY, JSON.stringify(next));
    } catch {
      // 存储不可用仅失去持久化
    }
  }, []);

  return (
    <main className={styles.page}>
      <h1 className={styles.title}>设置</h1>
      <ul className={styles.list}>
        <li className={styles.section}>连接画质</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>新连接默认画质</b>
            <span className={styles.hint}>会话内可随时切换；此处决定下次连接的初始档位</span>
          </span>
          <select
            className={styles.select}
            value={quality}
            aria-label="新连接默认画质"
            onChange={(e) => onQuality(e.currentTarget.value as Quality)}
          >
            {(Object.keys(QUALITY_LABEL) as Quality[]).map((q) => (
              <option key={q} value={q}>
                {QUALITY_LABEL[q]}
              </option>
            ))}
          </select>
        </li>
        {IS_MAC && (
        <li className={styles.section}>输入映射</li>
        )}
        {IS_MAC && (
        <li className={styles.row}>
          <label className={styles.label}>
            <b>macOS：Cmd 组合键映射为远端 Ctrl</b>
            <span className={styles.hint}>与会话内画面信息面板的开关保持同步</span>
          </label>
          <Switch
            checked={cmdToCtrl}
            onChange={onCmdToCtrl}
            label="Cmd 组合键映射为远端 Ctrl"
          />
        </li>
        )}
        <li className={styles.section}>网络</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>弱网纠错（FEC）</b>
            <span className={styles.hint}>
              信号差时画面仍流畅不花屏（带宽多耗约 20%）；下次连接生效
              {fecError !== null && ` · 保存失败：${fecError}`}
            </span>
          </span>
          <Switch
            checked={fec === true}
            disabled={fec === null}
            onChange={(v) => void onFec(v)}
            label="弱网纠错（FEC）"
          />
        </li>
        <li className={styles.section}>终端与常用语</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>终端默认字号</b>
            <span className={styles.hint}>终端内也可 Ctrl+= / Ctrl+- 调整（{FONT_SIZE_MIN}~{FONT_SIZE_MAX}）</span>
          </span>
          <span className={styles.stepper}>
            <button type="button" className={styles.stepBtn} aria-label="减小字号" onClick={() => onFont(-1)}>
              −
            </button>
            <span className={styles.stepValue}>{fontPx}px</span>
            <button type="button" className={styles.stepBtn} aria-label="增大字号" onClick={() => onFont(1)}>
              +
            </button>
          </span>
        </li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>自定义常用语</b>
            <span className={styles.hint}>悬浮工具栏「常用语」面板的短语（默认 5 条不可删）</span>
          </span>
          <span className={styles.phrases}>
            {phrases.length === 0 && <span className={styles.hint}>暂无自定义短语</span>}
            {phrases.map((p) => (
              <span key={p} className={styles.phrase}>
                {p}
                <button
                  type="button"
                  className={styles.phraseDel}
                  aria-label={`删除常用语「${p}」`}
                  onClick={() => removePhrase(p)}
                >
                  ×
                </button>
              </span>
            ))}
          </span>
        </li>
      </ul>
    </main>
  );
}
