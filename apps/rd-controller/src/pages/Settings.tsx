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
import { loadAdvancedMode, saveAdvancedMode } from "../ui/advancedMode";
import { deployRelaySsh, type SshDeployResult } from "../api/relayDeploy";
import {
  activateLicense,
  getEasytier,
  getLicenseStatus,
  getRelayAddr,
  setEasytier,
  setRelayAddr,
  type EasyTierSettings,
  type LicenseStatusDto,
} from "../api/devices";
import { useUpdater } from "../ui/useUpdater";
import {
  crashClear,
  crashDumps,
  crashReportUrl,
  type CrashEntry,
} from "../api/crash";
import { openUrl } from "@tauri-apps/plugin-opener";
import styles from "./Settings.module.css";

/** 把 Error/unknown 统一成消息文本。 */
/** 后端返回归一化（桥接层历史 mock/异常值兜底：字段缺失时取默认，不崩渲染）。 */
function normalizeEasytier(v: unknown): EasyTierSettings {
  const o = (typeof v === "object" && v !== null ? v : {}) as Record<string, unknown>;
  const str = (k: string): string => (typeof o[k] === "string" ? (o[k] as string) : "");
  const bool = (k: string): boolean => o[k] === true;
  const strs = (k: string): string[] =>
    Array.isArray(o[k]) ? (o[k] as unknown[]).filter((x): x is string => typeof x === "string") : [];
  return {
    enabled: bool("enabled"),
    binaryPath: str("binaryPath"),
    cliPath: str("cliPath"),
    networkName: str("networkName"),
    networkSecret: str("networkSecret"),
    peerUrls: strs("peerUrls"),
    virtualIp: str("virtualIp"),
    noTun: o["noTun"] === undefined ? true : bool("noTun"),
    forwards: strs("forwards"),
  };
}

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
  /** EasyTier 组网（T-043）：设置 → EasyTier 组网；null = 挂载期读取中/失败。 */
  const [et, setEt] = useState<EasyTierSettings | null>(null);
  const [etMsg, setEtMsg] = useState<string | null>(null);
  const [etDirty, setEtDirty] = useState(false);
  const [etError, setEtError] = useState<string | null>(null);
  /** 全局 UI 缩放（P1-4；切换即时生效并持久化）。 */
  const [uiScale, setUiScale] = useState<UiScale>(loadUiScale);
  /** 中继地址保存前的格式错误（P1-2；有错不写入）。 */
  const [relayError, setRelayError] = useState<string | null>(null);
  /** 应用更新（T-038）：手动检查/下载/重启共用状态机（静默检查在 App 横幅）。 */
  const updater = useUpdater();
  /** 崩溃报告（T-040）：本地 dump 列表；null=读取中/读取失败（按钮禁用）。 */
  const [crashes, setCrashes] = useState<CrashEntry[] | null>(null);
  /** 授权状态与激活输入（告别手建 AppData\...\license.key）。 */
  const [license, setLicense] = useState<LicenseStatusDto | null>(null);
  const [licenseInput, setLicenseInput] = useState("");
  const [licenseMsg, setLicenseMsg] = useState<string | null>(null);
  const [licenseError, setLicenseError] = useState<string | null>(null);
  const [licenseBusy, setLicenseBusy] = useState(false);

  /** RustDesk 风格设置分类锚点高亮 */
  const [activeCat, setActiveCat] = useState<string>("sec-general");
  const jumpCat = useCallback((id: string) => {
    setActiveCat(id);
    const el = document.getElementById(id);
    if (el && typeof el.scrollIntoView === "function") {
      el.scrollIntoView({ behavior: "smooth", block: "start" });
    }
  }, []);

  /** RustDesk 风格安全与密码配置（一次性码 / 固定无人值守密码 / 两者皆可） */
  type AuthMode = "both" | "one-time" | "fixed";
  const [authMode, setAuthMode] = useState<AuthMode>(() => {
    const v = window.localStorage.getItem("rd.authMode");
    return v === "one-time" || v === "fixed" ? v : "both";
  });
  const [fixedPwd, setFixedPwd] = useState<string>(
    () => window.localStorage.getItem("rd.fixedPassword") ?? "",
  );
  const [fixedPwdMsg, setFixedPwdMsg] = useState<string | null>(null);
  const [relayKey, setRelayKey] = useState<string>(
    () => window.localStorage.getItem("rd.relayKey") ?? "",
  );
  const [listenPort, setListenPort] = useState<string>(
    () => window.localStorage.getItem("rd.listenPort") ?? "21118",
  );
  const [confirmIncoming, setConfirmIncoming] = useState<boolean>(
    () => window.localStorage.getItem("rd.confirmIncoming") === "1",
  );
  const [allowPty, setAllowPty] = useState<boolean>(
    () => window.localStorage.getItem("rd.allowPty") !== "0",
  );

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

  // 崩溃记录（挂载一次；读取失败保持 null，按钮禁用）
  useEffect(() => {
    void crashDumps()
      .then((v) => setCrashes(v ?? []))
      .catch(() => setCrashes(null));
  }, []);

  // EasyTier 设置（挂载一次；读取失败保持 null，仅提示不阻塞其他节）
  useEffect(() => {
    void getEasytier()
      .then((v) => setEt(normalizeEasytier(v)))
      .catch(() => setEt(null));
  }, []);

  // 授权状态（挂载一次）
  useEffect(() => {
    void getLicenseStatus()
      .then((v) => {
        if (v !== null && typeof v === "object" && "plan" in v) {
          setLicense(v);
        }
      })
      .catch(() => setLicense(null));
  }, []);

  const onActivateLicense = useCallback(async () => {
    const trimmed = licenseInput.trim();
    if (trimmed.length === 0) {
      setLicenseError("请先粘贴授权码");
      return;
    }
    setLicenseError(null);
    setLicenseMsg(null);
    setLicenseBusy(true);
    try {
      const next = await activateLicense(trimmed);
      setLicense(next);
      setLicenseInput("");
      setLicenseMsg("已激活专业版：重启 rd-controller 后生效");
    } catch (e) {
      setLicenseError(errText(e));
    } finally {
      setLicenseBusy(false);
    }
  }, [licenseInput]);

  /** 保存 EasyTier：启用时网络名/二进制路径必填；转发规则逐行格式校验。 */
  const onSaveEasytier = useCallback(async () => {
    if (et === null) {
      return;
    }
    setEtError(null);
    if (et.enabled && et.networkName.trim() === "") {
      setEtError("启用 EasyTier 必须填网络名");
      return;
    }
    if (et.enabled && et.binaryPath.trim() === "") {
      setEtError("启用 EasyTier 必须填 easytier-core 路径");
      return;
    }
    for (const f of et.forwards) {
      if (!/^\s*\d{1,3}(\.\d{1,3}){3}:\d+\@\d+\s*$/.test(f)) {
        setEtError(`转发规则格式错误：${f}（应为 对端虚拟IP:端口@本地端口，如 10.126.126.3:21118@21130）`);
        return;
      }
    }
    try {
      await setEasytier(et);
      setEtMsg("已保存：重启 rd 后生效");
      setEtDirty(false);
    } catch (e) {
      setEtMsg(`保存失败：${errText(e)}`);
    }
  }, [et]);

  const onReportCrash = useCallback(async () => {
    try {
      const url = await crashReportUrl();
      await openUrl(url);
    } catch {
      // 打开失败静默：记录仍在本地，用户可再试
    }
  }, []);

  const onClearCrash = useCallback(async () => {
    try {
      await crashClear();
      setCrashes([]);
    } catch {
      // 清空失败保持原列表
    }
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

  /** 交互模式（宪法 #2）：false=简洁模式（小白默认）；true=高级模式（极客调优）。 */
  const [advancedMode, setAdvancedMode] = useState<boolean>(loadAdvancedMode);
  /** SSH 一键部署自建中转服务器（宪法 #3）。 */
  const [sshHost, setSshHost] = useState("");
  const [sshPort, setSshPort] = useState("22");
  const [sshUser, setSshUser] = useState("root");
  const [sshKeyPath, setSshKeyPath] = useState("");
  const [sshMode, setSshMode] = useState<"udp" | "docker">("udp");
  const [sshDomain, setSshDomain] = useState("");
  const [sshBusy, setSshBusy] = useState(false);
  const [sshResult, setSshResult] = useState<SshDeployResult | null>(null);
  const [sshError, setSshError] = useState<string | null>(null);

  const onToggleAdvancedMode = useCallback((v: boolean) => {
    setAdvancedMode(v);
    saveAdvancedMode(v);
  }, []);

  const onDeployRelaySsh = useCallback(async () => {
    setSshError(null);
    setSshResult(null);
    if (sshHost.trim().length === 0) {
      setSshError("请先填写服务器 IP 或域名");
      return;
    }
    if (sshMode === "docker" && sshDomain.trim().length === 0) {
      setSshError("Docker HTTPS 模式需要填写解析到该服务器的域名");
      return;
    }
    setSshBusy(true);
    try {
      const res = await deployRelaySsh({
        host: sshHost.trim(),
        port: Number(sshPort) || 22,
        user: sshUser.trim() || "root",
        keyPath: sshKeyPath.trim(),
        relayPort: 21118,
        mode: sshMode,
        domain: sshDomain.trim(),
      });
      setSshResult(res);
      if (res.ok && res.relayAddr) {
        setRelayAddrState(res.relayAddr);
        setRelayDirty(false);
        setRelayError(null);
        try {
          await setRelayAddr(res.relayAddr);
          setRelayMsg("已自动回填并保存（重启 rd-controller 后生效）");
        } catch (e) {
          setRelayDirty(true);
          setRelayMsg(`已回填，请手动点击保存（自动保存失败：${errText(e)}）`);
        }
      }
    } catch (e) {
      setSshError(errText(e));
    } finally {
      setSshBusy(false);
    }
  }, [sshDomain, sshHost, sshKeyPath, sshMode, sshPort, sshUser]);

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
      <div className={styles.settingsShell}>
        {/* RustDesk 风格左侧设置分类导航 */}
        <nav className={styles.settingsNav} aria-label="设置分类">
          <span className={styles.navHeading}>设置分类</span>
          {[
            { id: "sec-general", label: "⚙️ 常规与外观" },
            { id: "sec-security", label: "🛡️ 安全与密码" },
            { id: "sec-display", label: "🖥️ 显示与画质" },
            { id: "sec-network", label: "🌐 网络与中转" },
            { id: "sec-about", label: "ℹ️ 授权与关于" },
            { id: "sec-control", label: "⌨️ 键鼠与终端" },
          ].map((item) => (
            <button
              key={item.id}
              type="button"
              className={`${styles.navBtn} ${
                activeCat === item.id ? styles.navBtnActive : ""
              }`}
              onClick={() => jumpCat(item.id)}
            >
              {item.label}
            </button>
          ))}
        </nav>

        <div className={styles.settingsContent}>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: "12px", flexWrap: "wrap" }}>
            <h1 className={styles.title}>设置</h1>
            <div style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "12px", color: "var(--rd-text-dim)" }}>
              <span style={{ padding: "3px 8px", borderRadius: "999px", background: "rgba(70, 209, 126, 0.12)", color: "var(--rd-ok)", border: "1px solid rgba(70, 209, 126, 0.3)" }}>
                ● 自动引擎已开启（LAN 自动发现 · 多路竞速 · 自适应 4K@144Hz）
              </span>
              <button
                type="button"
                className={styles.stepBtn}
                onClick={() => {
                  if (!advancedMode) {
                    onToggleAdvancedMode(true);
                  }
                  window.location.hash = "#/lab";
                }}
              >
                打开方案实验室 →
              </button>
            </div>
          </div>
          <ul className={styles.list}>
            <li id="sec-general" className={styles.section}>⚙️ 常规与外观</li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>高级模式（极客调优）</b>
                <span className={styles.hint}>
                  {advancedMode
                    ? "当前为高级模式：已解锁 EasyTier 虚拟局域网、FEC 底层调优与方案实验室（144Hz/200Mbps 压测）。"
                    : "当前为简洁模式（默认）：保留高频设置与自建中转；开启高级模式可解锁 EasyTier 与方案实验室。"}
                </span>
              </span>
              <Switch
                checked={advancedMode}
                onChange={onToggleAdvancedMode}
                label="高级模式（极客调优）"
              />
            </li>
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

            <li id="sec-security" className={styles.section}>🛡️ 安全与密码（对标 RustDesk）</li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>密码认证方式</b>
                <span className={styles.hint}>
                  一次性配对码可直接在首页左侧「你的桌面」一键生成与刷新（无需去系统托盘找）；搭配固定密码可用于无人值守。
                </span>
              </span>
              <select
                className={styles.select}
                aria-label="密码认证方式"
                value={authMode}
                onChange={(e) => {
                  const v = e.currentTarget.value as AuthMode;
                  setAuthMode(v);
                  window.localStorage.setItem("rd.authMode", v);
                }}
              >
                <option value="both">一次性配对码 + 固定密码（推荐）</option>
                <option value="one-time">仅使用一次性配对码</option>
                <option value="fixed">仅使用固定访问密码</option>
              </select>
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>固定访问密码（无人值守）</b>
                <span className={styles.hint}>
                  设置后可配合「本机 ID / IP」直接远程连接本机，无需有人在旁点击生成 6 位码。
                  {fixedPwdMsg !== null && ` · ${fixedPwdMsg}`}
                </span>
              </span>
              <span className={styles.relayEdit}>
                <input
                  type="password"
                  className={styles.relayInput}
                  value={fixedPwd}
                  placeholder="输入 6 位以上无人值守密码"
                  aria-label="固定访问密码"
                  onChange={(e) => {
                    setFixedPwd(e.currentTarget.value);
                    setFixedPwdMsg(null);
                  }}
                />
                <button
                  type="button"
                  className={styles.stepBtn}
                  onClick={() => {
                    const v = fixedPwd.trim();
                    if (v.length === 0) {
                      window.localStorage.removeItem("rd.fixedPassword");
                      setFixedPwdMsg("已清除固定密码");
                    } else if (v.length < 6) {
                      setFixedPwdMsg("密码至少 6 位");
                    } else {
                      window.localStorage.setItem("rd.fixedPassword", v);
                      setFixedPwdMsg("已保存固定密码");
                    }
                  }}
                >
                  保存密码
                </button>
              </span>
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>新设备连接前需本机确认</b>
                <span className={styles.hint}>
                  开启后，陌生/新设备接入本机时弹出确认提示，防止未经允许的连接。
                </span>
              </span>
              <Switch
                checked={confirmIncoming}
                onChange={(v) => {
                  setConfirmIncoming(v);
                  window.localStorage.setItem("rd.confirmIncoming", v ? "1" : "0");
                }}
                label="新设备连接前需本机确认"
              />
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>允许远程原生终端（PTY）</b>
                <span className={styles.hint}>
                  关闭后，远端控制端将无法在本机打开原生命令行终端标签。
                </span>
              </span>
              <Switch
                checked={allowPty}
                onChange={(v) => {
                  setAllowPty(v);
                  window.localStorage.setItem("rd.allowPty", v ? "1" : "0");
                }}
                label="允许远程原生终端"
              />
            </li>

            <li id="sec-display" className={styles.section}>🖥️ 显示与画质</li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>新连接默认画质</b>
                <span className={styles.hint}>
                  局域网自动解锁 4K@144Hz / 200Mbps 高刷上限，会话内可随时切换
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

            <li id="sec-network" className={styles.section}>🌐 网络与自建中转（开启中转自动启用 ID 连接）</li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>使用自建中继（开启 9 位设备 ID 直连）</b>
                <span className={styles.hint}>
                  配置中继地址后，首页左侧将自动显示「本机 9 位设备 ID」，对方可像 RustDesk 一样用「ID + 密码/配对码」跨网连接（直连优先，打洞失败自动走中继，端到端加密中继只见密文）。
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
            <li className={styles.row}>
              <span className={styles.label}>
                <b>中转服务器鉴权 Key 与默认监听端口</b>
                <span className={styles.hint}>
                  默认监听端口 21118（连接时无需填写端口）；如自建中转启用了私有 Key，可在此填入。
                </span>
              </span>
              <span className={styles.relayEdit}>
                <input
                  className={styles.relayInput}
                  value={relayKey}
                  placeholder="中转 Key（选填）"
                  spellCheck={false}
                  aria-label="中转服务器 Key"
                  onChange={(e) => {
                    setRelayKey(e.currentTarget.value);
                    window.localStorage.setItem("rd.relayKey", e.currentTarget.value);
                  }}
                />
                <input
                  className={styles.relayInput}
                  style={{ maxWidth: "88px" }}
                  value={listenPort}
                  placeholder="21118"
                  aria-label="监听端口"
                  onChange={(e) => {
                    setListenPort(e.currentTarget.value);
                    window.localStorage.setItem("rd.listenPort", e.currentTarget.value);
                  }}
                />
              </span>
            </li>
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
            {advancedMode && (
              <>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>SSH 一键部署自建中转服务器</b>
                <span className={styles.hint}>
                  填入 Linux 服务器 SSH 信息，点击一键上传并部署 systemd 守护 + 防火墙放行，成功后自动回填上方中继地址。
                </span>
            {sshError !== null && (
              <span className={styles.hintError} role="alert">
                {sshError}
              </span>
            )}
            {sshResult !== null && (
              <span className={sshResult.ok ? styles.hint : styles.hintError}>
                {sshResult.summary}
              </span>
            )}
            {sshResult !== null && sshResult.log.trim().length > 0 && (
              <pre
                style={{
                  margin: "6px 0 0",
                  padding: "8px",
                  maxHeight: "140px",
                  overflow: "auto",
                  fontSize: "11px",
                  background: "rgba(0,0,0,0.25)",
                  borderRadius: "6px",
                  whiteSpace: "pre-wrap",
                }}
              >
                {sshResult.log}
              </pre>
            )}
          </span>
          <span className={styles.relayEdit} style={{ flexWrap: "wrap", gap: "6px" }}>
            <input
              className={styles.relayInput}
              value={sshHost}
              placeholder="服务器 IP（如 1.2.3.4）"
              spellCheck={false}
              aria-label="SSH 服务器地址"
              onChange={(e) => setSshHost(e.currentTarget.value)}
            />
            <input
              className={styles.relayInput}
              style={{ width: "72px" }}
              value={sshPort}
              placeholder="22"
              spellCheck={false}
              aria-label="SSH 端口"
              onChange={(e) => setSshPort(e.currentTarget.value)}
            />
            <input
              className={styles.relayInput}
              style={{ width: "84px" }}
              value={sshUser}
              placeholder="root"
              spellCheck={false}
              aria-label="SSH 用户名"
              onChange={(e) => setSshUser(e.currentTarget.value)}
            />
            <input
              className={styles.relayInput}
              value={sshKeyPath}
              placeholder="私钥路径（可选，留空用默认 ~/.ssh）"
              spellCheck={false}
              aria-label="SSH 私钥路径"
              onChange={(e) => setSshKeyPath(e.currentTarget.value)}
            />
            <select
              className={styles.select}
              value={sshMode}
              aria-label="中继部署模式"
              onChange={(e) => setSshMode(e.currentTarget.value as "udp" | "docker")}
            >
              <option value="udp">原生 UDP（延迟最低）</option>
              <option value="docker">Docker + 自动 HTTPS</option>
            </select>
            {sshMode === "docker" && (
              <input
                className={styles.relayInput}
                value={sshDomain}
                placeholder="域名（如 relay.example.com）"
                spellCheck={false}
                aria-label="中继域名"
                onChange={(e) => setSshDomain(e.currentTarget.value)}
              />
            )}
            <button
              type="button"
              className={styles.stepBtn}
              disabled={sshBusy || sshHost.trim().length === 0}
              onClick={() => void onDeployRelaySsh()}
            >
              {sshBusy ? "部署中…" : "一键上传并部署"}
            </button>
          </span>
        </li>
        <li className={styles.section}>EasyTier 组网（跨网 mesh）</li>
        {et === null ? (
          <li className={styles.row}>
            <span className={styles.label}>
              <span className={styles.hint}>EasyTier 设置读取中（或被控服务未启动——保存不受影响）</span>
            </span>
          </li>
        ) : (
          <>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>启用 EasyTier 组网</b>
                <span className={styles.hint}>
                  两端加入同一 EasyTier 网络（去中心化 mesh，内置打洞/中继/多协议回退）后，
                  控制端可经 mesh 直拨被控端虚拟 IP。需安装 EasyTier（不随 rd 分发）；
                  重启 rd 后生效。免管理员模式默认开（不建虚拟网卡）。
                  {etMsg !== null && ` · ${etMsg}`}
                </span>
                {etError !== null && (
                  <span className={styles.hintError} role="alert">
                    {etError}
                  </span>
                )}
              </span>
              <Switch
                checked={et.enabled}
                onChange={(v) => {
                  setEt({ ...et, enabled: v });
                  setEtDirty(true);
                  setEtError(null);
                }}
                label="启用 EasyTier 组网"
              />
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>网络名</b>
                <span className={styles.hint}>同一网络名的节点互相可见（ rd 专用网络名即可）。</span>
              </span>
              <span className={styles.relayEdit}>
                <input
                  className={styles.relayInput}
                  value={et.networkName}
                  placeholder="rd-mesh"
                  spellCheck={false}
                  aria-label="EasyTier 网络名"
                  onChange={(e) => {
                    setEt({ ...et, networkName: e.currentTarget.value });
                    setEtDirty(true);
                  }}
                />
              </span>
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>网络密钥</b>
                <span className={styles.hint}>同网络名 + 同密钥才可入网（防陌生人蹭网）。</span>
              </span>
              <span className={styles.relayEdit}>
                <input
                  className={styles.relayInput}
                  type="password"
                  value={et.networkSecret}
                  placeholder="随意但两端一致"
                  spellCheck={false}
                  aria-label="EasyTier 网络密钥"
                  onChange={(e) => {
                    setEt({ ...et, networkSecret: e.currentTarget.value });
                    setEtDirty(true);
                  }}
                />
              </span>
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>easytier-core 路径</b>
                <span className={styles.hint}>
                  EasyTier 可执行文件目录中的 easytier-core(.exe)；easytier-cli
                  同目录时虚拟 IP 自动发现，否则请填「cli 路径」。
                </span>
              </span>
              <span className={styles.relayEdit}>
                <input
                  className={styles.relayInput}
                  value={et.binaryPath}
                  placeholder="C:\\tools\\easytier\\easytier-core.exe"
                  spellCheck={false}
                  aria-label="easytier-core 路径"
                  onChange={(e) => {
                    setEt({ ...et, binaryPath: e.currentTarget.value });
                    setEtDirty(true);
                  }}
                />
              </span>
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>引导节点（可选）</b>
                <span className={styles.hint}>
                  逗号分隔的 peer 地址（如 tcp://public.easytier.top:11010 或自建节点）；
                  留空 = 仅同网段局域网内组网。
                </span>
              </span>
              <span className={styles.relayEdit}>
                <input
                  className={styles.relayInput}
                  value={et.peerUrls.join(",")}
                  placeholder="tcp://public.easytier.top:11010"
                  spellCheck={false}
                  aria-label="EasyTier 引导节点"
                  onChange={(e) => {
                    const urls = e.currentTarget.value
                      .split(",")
                      .map((s) => s.trim())
                      .filter((s) => s.length > 0);
                    setEt({ ...et, peerUrls: urls });
                    setEtDirty(true);
                  }}
                />
              </span>
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>静态虚拟 IP（可选）</b>
                <span className={styles.hint}>
                  如 10.126.126.2/24；留空 = 自动分配。两端约定不同地址，设备地址直接填对端虚拟
                  IP 即可经 mesh 连接（免管理员模式不适用，见转发规则）。
                </span>
              </span>
              <span className={styles.relayEdit}>
                <input
                  className={styles.relayInput}
                  value={et.virtualIp}
                  placeholder="10.126.126.2/24"
                  spellCheck={false}
                  aria-label="EasyTier 静态虚拟 IP"
                  onChange={(e) => {
                    setEt({ ...et, virtualIp: e.currentTarget.value });
                    setEtDirty(true);
                  }
                  }
                />
              </span>
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>免管理员模式</b>
                <span className={styles.hint}>
                  不建虚拟网卡（无需管理员权限）。控制端此时不能直拨虚拟 IP，
                  用下方「转发规则」把本机端口映射到对端 rd。
                </span>
              </span>
              <Switch
                checked={et.noTun}
                onChange={(v) => {
                  setEt({ ...et, noTun: v });
                  setEtDirty(true);
                }}
                label="免管理员模式"
              />
            </li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>转发规则（免管理员模式用）</b>
                <span className={styles.hint}>
                  每行一条「对端虚拟IP:rd端口@本地端口」，如 10.126.126.3:21118@21130：
                  本机 127.0.0.1:21130 经 mesh 转发到对端 rd；连接时自动作为候选。
                  仅控制端机器需要配置。
                </span>
              </span>
              <span className={styles.relayEdit}>
                <textarea
                  className={styles.relayInput}
                  value={et.forwards.join("\n")}
                  placeholder={"10.126.126.3:21118@21130"}
                  rows={2}
                  spellCheck={false}
                  aria-label="EasyTier 转发规则"
                  onChange={(e) => {
                    const lines = e.currentTarget.value.split(/\r?\n/).filter((s) => s.trim().length > 0);
                    setEt({ ...et, forwards: lines });
                    setEtDirty(true);
                  }}
                />
                <button
                  type="button"
                  className={styles.stepBtn}
                  disabled={!etDirty}
                  aria-label="保存 EasyTier 设置"
                  onClick={() => void onSaveEasytier()}
                >
                  保存
                </button>
                </span>
            </li>
          </>
        )}
              </>
            )}
        <li id="sec-about" className={styles.section}>ℹ️ 授权与关于</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>
              {license?.plan === "pro"
                ? `专业版（Pro）${license.name ? ` · ${license.name}` : ""}`
                : "免费版（Free）"}
            </b>
            <span className={styles.hint}>
              {license?.plan === "pro"
                ? `已解锁全部专业能力，支持最多 ${license.devicesMax} 台受信任设备。`
                : `永久免费使用全部核心功能（限 ${license?.devicesMax ?? 2} 台受信任设备、1 路并发连接）。粘贴授权码即可一键升级专业版，无需手动找配置目录。`}
              {licenseMsg !== null && ` · ${licenseMsg}`}
            </span>
            {licenseError !== null && (
              <span className={styles.hintError} role="alert">
                {licenseError}
              </span>
            )}
          </span>
          <span className={styles.relayEdit}>
            <input
              className={styles.relayInput}
              value={licenseInput}
              placeholder="粘贴 RD1. 开头的授权码"
              spellCheck={false}
              aria-label="授权码"
              aria-invalid={licenseError !== null}
              onChange={(e) => {
                setLicenseInput(e.currentTarget.value);
                setLicenseError(null);
                setLicenseMsg(null);
              }}
            />
            <button
              type="button"
              className={styles.stepBtn}
              disabled={licenseBusy || licenseInput.trim().length === 0}
              onClick={() => void onActivateLicense()}
            >
              {licenseBusy ? "激活中…" : "激活"}
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
        {advancedMode && (
          <>
        <li className={styles.section}>崩溃报告</li>
        <li className={styles.row}>
          <span className={styles.label}>
            <b>本机崩溃记录</b>
            <span className={styles.hint}>
              {crashes === null
                ? "读取失败（被控服务未启动）"
                : crashes.length === 0
                  ? "无崩溃记录。"
                  : `共 ${crashes.length} 条（只含元数据：时间/线程/出错位置，无任何屏幕或输入内容）。最新：${crashes[0]?.message ?? ""}`}
              上报 = 打开浏览器预填 Issue，内容可先检查再提交。
            </span>
          </span>
          <span className={styles.relayEdit}>
            <button
              type="button"
              className={styles.stepBtn}
              disabled={crashes === null || crashes.length === 0}
              onClick={() => void onReportCrash()}
            >
              在浏览器上报
            </button>
            <button
              type="button"
              className={styles.stepBtn}
              disabled={crashes === null || crashes.length === 0}
              onClick={() => void onClearCrash()}
            >
              清除记录
            </button>
          </span>
        </li>
          </>
        )}
        <li id="sec-control" className={styles.section}>⌨️ 键鼠与终端</li>
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
        {advancedMode && (
          <>
            <li className={styles.section}>🧪 高级与调试</li>
            <li className={styles.row}>
              <span className={styles.label}>
                <b>方案实验室（开发者工具）</b>
                <span className={styles.hint}>
                  链路选路、拥塞控制、弱网模拟、144Hz 高刷压测与 1:1 像素微距检视台
                </span>
              </span>
              <a className={styles.stepBtn} href="#/lab">
                打开实验室
              </a>
            </li>
          </>
        )}
      </ul>
        </div>
      </div>
    </main>
  );
}
