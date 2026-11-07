/**
 * 设置页（R13 设计#4 整块）：把散落各处的真实偏好收进一个可发现的面——
 * 画质默认（曾埋在会话工具栏）/ Cmd→Ctrl（曾埋在 HUD）/ 弱网纠错（曾埋在
 * 设备页 hostCard）/ 终端字号（曾只有终端内快捷键）/ 自定义常用语管理
 * （曾只进不出）。全部接现有状态与存储，无新后端。
 *
 * P1 批次补：远程访问中继地址保存前格式校验（错误只提示不写入）、生效
 * 文案写实（重启 rd-controller）；外观组新增全局 UI 缩放（P1-4）。
 */
import { useCallback, useEffect, useState } from "react";
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
import {
  PHRASES_MAX,
  readCustomPhrases,
  saveCustomPhrases,
} from "../session/phrases";
import {
  UI_SCALES,
  applyUiScale,
  loadUiScale,
  saveUiScale,
  type UiScale,
} from "../ui/uiScale";
import { Switch } from "../ui/Switch";
import { getRelayAddr, setRelayAddr } from "../api/devices";
import { useUpdater } from "../ui/useUpdater";
import styles from "./Settings.module.css";

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * 中继地址保存前校验（P1-2）：与后端 rd_transport::parse_relay_endpoint
 * 对齐——URL 形式（http/https/ws/wss）或 ip:port（ip 可省端口，后端补
 * 默认 21118；主机名不带协议不支持，后端 SocketAddr 解析会拒绝）。
 * 合法返回 null；空串 = 清除中继，合法。
 */
export function relayAddrError(raw: string): string | null {
  const v = raw.trim();
  if (v.length === 0) {
    return null;
  }
  if (/^(https?|wss?):\/\//i.test(v)) {
    return (v.split("://")[1]?.length ?? 0)
      ? null
      : "缺主机名：示例 https://xxx.workers.dev";
  }
  return /^(\d{1,3}(\.\d{1,3}){3})(:\d{1,5})?$/.test(v)
    ? null
    : "格式不对：请填 https://xxx.workers.dev 或 1.2.3.4:21118";
}

type Quality = "auto" | "high" | "medium" | "low";
const QUALITY_KEY = "rd.quality";
const QUALITY_LABEL: Record<Quality, string> = {
  auto: "自动（智能自适应）",
  high: "高清",
  medium: "均衡",
  low: "流畅（弱网）",
};
function readQuality(): Quality {
  const saved = window.localStorage.getItem(QUALITY_KEY);
  return saved === "high" || saved === "medium" || saved === "low"
    ? saved
    : "auto";
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
  // 原生中继（T-033）：设置 → 远程访问 → 使用自建中继
  const [relayAddr, setRelayAddrState] = useState("");
  const [relayMsg, setRelayMsg] = useState<string | null>(null);
  const [relayDirty, setRelayDirty] = useState(false);
  const [fontPx, setFontPx] = useState<number>(() => loadFontSize());
  const [phrases, setPhrases] = useState<readonly string[]>(readCustomPhrases);
  /** 新增常用语文案输入（R16：设置页补添加入口，避免"只能删不能加"死胡同）。 */
  const [phraseInput, setPhraseInput] = useState("");
  /** 中继挂载期读取失败（R16：从 relayMsg 独立出来，不再混进保存类反馈）。 */
  const [relayReadFailed, setRelayReadFailed] = useState(false);
  const [fecError, setFecError] = useState<string | null>(null);
  /** 全局 UI 缩放（P1-4；切换即时生效并持久化）。 */
  const [uiScale, setUiScale] = useState<UiScale>(loadUiScale);
  /** 中继地址保存前的格式错误（P1-2；有错不写入）。 */
  const [relayError, setRelayError] = useState<string | null>(null);
  /** 应用更新（T-038）：手动检查/下载/重启共用状态机（静默检查在 App 横幅）。 */
  const updater = useUpdater();

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

  // 读当前中继地址（挂载一次）
  useEffect(() => {
    void getRelayAddr()
      .then((v) => setRelayAddrState(v ?? ""))
      .catch(() => setRelayReadFailed(true)); // 读取失败独立提示，不再混进 relayMsg
  }, []);

  const onSaveRelay = useCallback(async () => {
    const invalid = relayAddrError(relayAddr);
    if (invalid !== null) {
      setRelayError(invalid);
      return; // 格式错误不写入
    }
    setRelayError(null);
    try {
      await setRelayAddr(relayAddr.trim());
      setRelayMsg("已保存：重启 rd-controller 后生效");
      setRelayDirty(false);
    } catch (e) {
      setRelayMsg(`保存失败：${errText(e)}`);
    }
  }, [relayAddr]);

  /** 全局 UI 缩放（P1-4）：改档即时生效 + 持久化。 */
  const onUiScale = useCallback((s: UiScale) => {
    setUiScale(s);
    applyUiScale(s);
    saveUiScale(s);
  }, []);

  const onFont = useCallback(
    (delta: number) => {
      const next = Math.min(
        FONT_SIZE_MAX,
        Math.max(FONT_SIZE_MIN, fontPx + delta),
      );
      setFontPx(next);
      saveFontSize(next);
    },
    [fontPx],
  );

  const removePhrase = useCallback((text: string) => {
    const next = readCustomPhrases().filter((p) => p !== text);
    setPhrases(next);
    saveCustomPhrases(next);
  }, []);

  /** 新增常用语（R16，与 removePhrase 对称）：空串不添加、重复去重、达上限不添加。 */
  const addPhrase = useCallback(() => {
    const text = phraseInput.trim();
    if (text.length === 0) {
      return; // 空串不添加
    }
    const current = readCustomPhrases();
    if (current.includes(text)) {
      setPhraseInput(""); // 已存在：去重（短语文本即渲染 key，重复既撞 key 也无意义）
      return;
    }
    if (current.length >= PHRASES_MAX) {
      return; // 对齐存储上限（20 条），不再追加
    }
    const next = [...current, text];
    setPhrases(next);
    saveCustomPhrases(next);
    setPhraseInput("");
  }, [phraseInput]);

  return (
    <main className={styles.page}>
      <h1 className={styles.title}>设置</h1>
      <ul className={styles.list}>
        <li className={styles.section}>外观</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>界面缩放</b>
            <span className={styles.hint}>
              放大整个界面的文字与控件（低视力 / 高分屏适用）；选完立即生效
            </span>
          </span>
          <select
            className={styles.select}
            value={uiScale}
            aria-label="界面缩放"
            onChange={(e) =>
              onUiScale(Number(e.currentTarget.value) as UiScale)
            }
          >
            {UI_SCALES.map((s) => (
              <option key={s} value={s}>
                {Math.round(s * 100)}%
              </option>
            ))}
          </select>
        </li>
        <li className={styles.section}>连接画质</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>新连接默认画质</b>
            <span className={styles.hint}>
              会话内可随时切换；此处决定下次连接的初始档位
            </span>
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
        {IS_MAC && <li className={styles.section}>输入映射</li>}
        {IS_MAC && (
          <li className={styles.row}>
            <label className={styles.label}>
              <b>macOS：Cmd 组合键映射为远端 Ctrl</b>
              <span className={styles.hint}>
                与会话内画面信息面板的开关保持同步
              </span>
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
        <li className={styles.section}>远程访问（跨网中继）</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>使用自建中继</b>
            <span className={styles.hint}>
              跨网打洞失败时的兜底通道（本机作为被控端使用）。支持 Cloudflare
              Workers（https://xxx.workers.dev）或 VPS 直连地址（ip:21118）；
              连接时直连优先，失败自动走中继；中继只见密文。
              {relayMsg !== null && ` · ${relayMsg}`}
            </span>
            {relayReadFailed && (
              <span className={styles.hint}>
                未能读取当前中继设置（被控服务未启动）——可直接填写并保存，重启后生效
              </span>
            )}
            {relayError !== null && (
              <span className={styles.hintError} role="alert">
                {relayError}
              </span>
            )}
          </span>
          <span className={styles.relayEdit}>
            <input
              className={styles.relayInput}
              value={relayAddr}
              placeholder="https://xxx.workers.dev 或 1.2.3.4:21118"
              spellCheck={false}
              aria-label="原生中继地址"
              aria-invalid={relayError !== null}
              onChange={(e) => {
                setRelayAddrState(e.currentTarget.value);
                setRelayDirty(true);
                setRelayError(null);
              }}
            />
            <button
              type="button"
              className={styles.stepBtn}
              disabled={!relayDirty}
              onClick={() => void onSaveRelay()}
            >
              保存
            </button>
          </span>
        </li>
        <li className={styles.section}>应用更新</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>检查更新</b>
            <span className={styles.hint}>
              应用启动时会静默检查（每天最多一次）；此处手动检查不受限制。
              {updater.state.phase === "idle" && " 当前已是最新。"}
              {updater.state.phase === "available" &&
                ` 发现新版本 v${updater.state.pending.info.version}。`}
              {updater.state.phase === "downloading" &&
                updater.state.pct !== null &&
                ` 正在下载（${updater.state.pct}%）…`}
              {updater.state.phase === "error" &&
                ` 检查失败：${updater.state.message}`}
            </span>
          </span>
          <span className={styles.relayEdit}>
            {updater.state.phase === "available" && (
              <button
                type="button"
                className={styles.stepBtn}
                onClick={() => void updater.install()}
              >
                下载并安装
              </button>
            )}
            {updater.state.phase === "ready" && (
              <button
                type="button"
                className={styles.stepBtn}
                onClick={updater.restart}
              >
                重启应用
              </button>
            )}
            <button
              type="button"
              className={styles.stepBtn}
              disabled={
                updater.state.phase === "checking" ||
                updater.state.phase === "downloading"
              }
              onClick={() => void updater.checkNow()}
            >
              {updater.state.phase === "checking" ? "检查中…" : "检查更新"}
            </button>
          </span>
        </li>
        <li className={styles.section}>终端与常用语</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>终端默认字号</b>
            <span className={styles.hint}>
              终端内也可 Ctrl+= / Ctrl+- 调整（{FONT_SIZE_MIN}~{FONT_SIZE_MAX}）
            </span>
          </span>
          <span className={styles.stepper}>
            <button
              type="button"
              className={styles.stepBtn}
              aria-label="减小字号"
              onClick={() => onFont(-1)}
            >
              −
            </button>
            <span className={styles.stepValue}>{fontPx}px</span>
            <button
              type="button"
              className={styles.stepBtn}
              aria-label="增大字号"
              onClick={() => onFont(1)}
            >
              +
            </button>
          </span>
        </li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>自定义常用语</b>
            <span className={styles.hint}>
              本页可直接新增与删除，与悬浮工具栏「常用语」面板同一份（内置 5
              条不可删）
            </span>
          </span>
          <span className={styles.phrases}>
            {phrases.length === 0 && (
              <span className={styles.hint}>暂无自定义短语</span>
            )}
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
            <span className={styles.phraseAdd}>
              <input
                className={styles.phraseInput}
                value={phraseInput}
                placeholder="新增常用语…"
                aria-label="新增常用短语"
                onChange={(e) => setPhraseInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    addPhrase();
                  }
                }}
              />
              <button
                type="button"
                className={styles.phraseAddBtn}
                onClick={addPhrase}
              >
                添加
              </button>
            </span>
          </span>
        </li>
      </ul>
    </main>
  );
}
