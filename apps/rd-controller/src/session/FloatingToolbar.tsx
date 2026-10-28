/**
 * 悬浮工具栏（T-028 F1，2026-10-28 重设计）：
 * - 图标+文字双行按钮（小白不用猜图标）
 * - **常驻显示**，不再 3s 自动收起（用户反馈：莫名其妙收起来不方便）；
 *   仅右上角「─」手动收起为小圆点，点击/悬停展开
 * - 分组排列：画质│会话操作│画面捕获│系统│收起
 */
import { useState } from "react";
import styles from "./FloatingToolbar.module.css";

export type Quality = "auto" | "high" | "medium" | "low";

interface Props {
  onScreenshot: () => void;
  onFullscreen: () => void;
  onLockScreen: () => void;
  onCtrlAltDel: () => void;
  onQualityChange: (q: Quality) => void;
  onSendText: (text: string) => void;
  onSendCombo: (keys: string[]) => void;
  /** 隐私屏开关（T-028 F9；Pro 功能）。 */
  onPrivacyToggle: (on: boolean) => void;
  /** 当前是否已开启隐私屏（按钮高亮态）。 */
  privacyOn: boolean;
  /** 录屏开关（T-028 F10）。 */
  onRecordToggle: () => void;
  /** 当前是否正在录制（按钮高亮态）。 */
  recordingOn: boolean;
  /** 只看不控开关。 */
  onToggleViewOnly: () => void;
  /** 当前只看态（高亮）。 */
  viewOnly: boolean;
  /** 控制端剪贴板 → 远端。 */
  onCopyToRemote: () => void;
  /** 远端剪贴板 → 控制端。 */
  onCopyFromRemote: () => void;
  /** HUD 信息显隐切换。 */
  onToggleHud: () => void;
  /** 当前 HUD 显隐（高亮 = 常显）。 */
  hudVisible: boolean;
}

export function FloatingToolbar(props: Props) {
  // 手动收起（无自动收起）：展开是默认态
  const [expanded, setExpanded] = useState(true);
  const [panel, setPanel] = useState<"none" | "phrases" | "vkeys">("none");

  if (!expanded) {
    return (
      <button
        type="button"
        className={styles.dot}
        title="展开工具栏"
        onClick={() => setExpanded(true)}
      >
        ⚙
      </button>
    );
  }

  return (
    <div className={styles.bar}>
      <label className={styles.qualityWrap} title="画质：自动=智能自适应；弱网选流畅">
        <select
          className={styles.select}
          defaultValue="auto"
          onChange={(e) => props.onQualityChange(e.target.value as Quality)}
        >
          <option value="auto">自动</option>
          <option value="high">高清</option>
          <option value="medium">均衡</option>
          <option value="low">流畅</option>
        </select>
        <span className={styles.qualityLabel}>画质</span>
      </label>
      <Sep />
      <ToolBtn icon="📋" label="常用语" title="常用短语一键发送到远端" active={panel === "phrases"} onClick={() => setPanel(panel === "phrases" ? "none" : "phrases")} />
      <ToolBtn icon="⌨" label="虚拟键" title="虚拟按键（组合键/特殊键）" active={panel === "vkeys"} onClick={() => setPanel(panel === "vkeys" ? "none" : "vkeys")} />
      <ToolBtn icon="👁" label="只看" title="只看不控：键鼠不发给远端（防误操作），再点一次恢复" active={props.viewOnly} onClick={props.onToggleViewOnly} />
      <ToolBtn icon="📤" label="发文字" title="把本机剪贴板文字发给远端（远端 Ctrl+V 粘贴）" onClick={props.onCopyToRemote} />
      <ToolBtn icon="📥" label="取文字" title="取回远端剪贴板文字到本机" onClick={props.onCopyFromRemote} />
      <Sep />
      <ToolBtn icon="📸" label="截屏" title="保存当前远程画面到下载文件夹" onClick={props.onScreenshot} />
      <ToolBtn icon="⏺" label="录屏" title={props.recordingOn ? "停止录制（存到下载文件夹）" : "录制远程画面（存到下载文件夹）"} active={props.recordingOn} onClick={props.onRecordToggle} />
      <ToolBtn icon="⬛" label="隐私屏" title="隐私屏：对方屏幕变黑，操作照常（专业版）" active={props.privacyOn} onClick={() => props.onPrivacyToggle(!props.privacyOn)} />
      <ToolBtn icon="ℹ" label="信息" title="常显画面信息（延迟/码率/帧率）" active={props.hudVisible} onClick={props.onToggleHud} />
      <Sep />
      <ToolBtn icon="🖥" label="全屏" title="全屏切换（F11）" onClick={props.onFullscreen} />
      <ToolBtn icon="🔒" label="锁屏" title="远端锁屏（锁屏后输入自动停）" onClick={props.onLockScreen} />
      <ToolBtn icon="⚠" label="CAD" title="发送 Ctrl+Alt+Del 到远端" onClick={props.onCtrlAltDel} />
      <button type="button" className={styles.collapse} title="收起工具栏（点 ⚙ 展开）" onClick={() => setExpanded(false)}>
        ─
      </button>
      {panel === "phrases" && <PhrasesPanel onSend={(t) => { props.onSendText(t); setPanel("none"); }} onClose={() => setPanel("none")} />}
      {panel === "vkeys" && <VirtualKeysPanel onCombo={(k) => { props.onSendCombo(k); setPanel("none"); }} onClose={() => setPanel("none")} />}
    </div>
  );
}

/** 图文双行按钮（图标上、文字下；active 高亮）。 */
function ToolBtn({ icon, label, title, active, onClick }: { icon: string; label: string; title: string; active?: boolean; onClick: () => void }) {
  return (
    <button type="button" className={`${styles.toolBtn} ${active ? styles.toolBtnOn : ""}`} title={title} onClick={onClick}>
      <span className={styles.toolIcon}>{icon}</span>
      <span className={styles.toolLabel}>{label}</span>
    </button>
  );
}

function Sep() {
  return <span className={styles.sep} />;
}

// ── 常用语面板 ──
function PhrasesPanel({ onSend, onClose }: { onSend: (t: string) => void; onClose: () => void }) {
  const DEFAULTS = ["你好", "请稍等", "我来看一下", "操作完成", "谢谢"];
  const [custom, setCustom] = useState<string[]>(() => {
    try { return JSON.parse(localStorage.getItem("rd-phrases") ?? "[]"); } catch { return []; }
  });
  const [input, setInput] = useState("");

  const add = () => {
    if (!input.trim()) return;
    const next = [...custom, input.trim()].slice(-20);
    setCustom(next);
    localStorage.setItem("rd-phrases", JSON.stringify(next));
    setInput("");
  };

  return (
    <div className={styles.panel} onMouseLeave={onClose}>
      <b className={styles.panelTitle}>常用语（点击发送）</b>
      {[...DEFAULTS, ...custom].map((text, i) => (
        <button key={i} type="button" className={styles.panelItem} onClick={() => { onSend(text); onClose(); }}>{text}</button>
      ))}
      <div className={styles.panelAdd}>
        <input value={input} placeholder="新短语…" onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && add()} />
        <button type="button" onClick={add}>＋</button>
      </div>
    </div>
  );
}

// ── 虚拟按键面板 ──
function VirtualKeysPanel({ onCombo, onClose }: { onCombo: (k: string[]) => void; onClose: () => void }) {
  const [mods, setMods] = useState<Set<string>>(new Set());
  const toggle = (m: string) => setMods(prev => { const n = new Set(prev); n.has(m) ? n.delete(m) : n.add(m); return n; });

  const send = (...keys: string[]) => { onCombo([...mods, ...keys]); };

  const SPECIAL: [string, string][] = [
    ["Tab", "Tab"], ["Esc", "Escape"], ["Enter", "Enter"], ["⌫", "Backspace"],
    ["Del", "Delete"], ["Home", "Home"], ["End", "End"],
    ["PgUp", "PageUp"], ["PgDn", "PageDown"],
    ["↑", "ArrowUp"], ["↓", "ArrowDown"], ["←", "ArrowLeft"], ["→", "ArrowRight"],
  ];
  const FKEYS = Array.from({ length: 12 }, (_, i) => [`F${i + 1}`, `F${i + 1}`] as [string, string]);
  const MEDIA: [string, string][] = [
    ["🔊+", "VolumeUp"], ["🔊-", "VolumeDown"], ["🔇", "VolumeMute"], ["▶", "MediaPlayPause"],
  ];
  const SYSTEM: [string, string][] = [
    ["⊞", "MetaLeft"], ["⊞D", "WinD"], ["⇄", "AltTab"], ["CAD", "CtrlAltDel"],
  ];

  return (
    <div className={styles.panel} onMouseLeave={onClose}>
      <b className={styles.panelTitle}>虚拟按键（先点修饰键组合，再点目标键）</b>
      <div className={styles.modsRow}>
        {["Ctrl", "Alt", "Shift", "Win"].map((m) => (
          <button key={m} type="button" className={`${styles.modBtn} ${mods.has(m) ? styles.modOn : ""}`}
            onClick={() => toggle(m)}>{m}</button>
        ))}
      </div>
      <div className={styles.keysGrid}>
        {SPECIAL.map(([l, c]) => <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
      </div>
      <div className={styles.keysGrid}>
        {FKEYS.map(([l, c]) => <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
      </div>
      <div className={styles.keysGrid}>
        {MEDIA.map(([l, c]) => <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
        {SYSTEM.map(([l, c]) => <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
      </div>
    </div>
  );
}
