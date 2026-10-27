/**
 * 悬浮工具栏（T-028 F1）：远控画面顶部自动出现，3s 无鼠标收起为小圆点。
 * 画质 / 常用语 / 虚拟按键 / 截屏 / 全屏 / 锁屏 / Ctrl+Alt+Del / 折叠。
 */
import { useCallback, useEffect, useRef, useState } from "react";
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
  /** 只看不控开关（原左侧会话工具条迁移）。 */
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

const QUALITY_LABELS: Record<Quality, string> = {
  auto: "自动", high: "高画质", medium: "中画质", low: "低画质",
};

export function FloatingToolbar(props: Props) {
  const [expanded, setExpanded] = useState(true);
  const [panel, setPanel] = useState<"none" | "phrases" | "vkeys">("none");
  const hideTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const resetHide = useCallback(() => {
    if (hideTimer.current) clearTimeout(hideTimer.current);
    setExpanded(true);
    hideTimer.current = setTimeout(() => setExpanded(false), 3000);
  }, []);

  useEffect(() => {
    resetHide();
    return () => { if (hideTimer.current) clearTimeout(hideTimer.current); };
  }, [resetHide]);

  if (!expanded) {
    return (
      <button className={styles.dot} onMouseEnter={resetHide} onClick={resetHide} title="展开工具栏">⚙</button>
    );
  }

  return (
    <div
      className={styles.bar}
      onMouseEnter={resetHide}
      onMouseLeave={() => { hideTimer.current = setTimeout(() => setExpanded(false), 3000); }}
    >
      <select
        className={styles.select}
        defaultValue="auto"
        onChange={(e) => props.onQualityChange(e.target.value as Quality)}
        title="画质"
      >
        {Object.entries(QUALITY_LABELS).map(([v, label]) => (
          <option key={v} value={v}>{label}</option>
        ))}
      </select>
      <Sep />
      <ToolBtn icon="📋" title="常用语 (Ctrl+Shift+P)" active={panel === "phrases"} onClick={() => setPanel(panel === "phrases" ? "none" : "phrases")} />
      <ToolBtn icon="⌨" title="虚拟按键 (Ctrl+Shift+K)" active={panel === "vkeys"} onClick={() => setPanel(panel === "vkeys" ? "none" : "vkeys")} />
      <Sep />
      <ToolBtn icon="📸" title="截屏 (Ctrl+Alt+S)" onClick={props.onScreenshot} />
      <ToolBtn icon="🖥" title="全屏 (F11)" onClick={props.onFullscreen} />
      <ToolBtn
        icon="⬛"
        title="隐私屏：对方屏幕黑屏，操作照常（专业版）"
        active={props.privacyOn}
        onClick={() => props.onPrivacyToggle(!props.privacyOn)}
      />
      <ToolBtn
        icon="⏺"
        title={props.recordingOn ? "停止录制（存到下载目录）" : "录制远程画面（存到下载目录）"}
        active={props.recordingOn}
        onClick={props.onRecordToggle}
      />
      <ToolBtn
        icon="👁"
        title="只看不控：开启后键鼠不发给远端（防误操作），再点一次恢复"
        active={props.viewOnly}
        onClick={props.onToggleViewOnly}
      />
      <ToolBtn
        icon="📤"
        title="复制到远端：把本机剪贴板文字发给远端（远端 Ctrl+V 粘贴）"
        onClick={props.onCopyToRemote}
      />
      <ToolBtn
        icon="📥"
        title="从远端复制：取回远端剪贴板文字到本机"
        onClick={props.onCopyFromRemote}
      />
      <ToolBtn
        icon="ℹ"
        title={props.hudVisible ? "隐藏画面信息（延迟/码率）" : "常显画面信息（延迟/码率）"}
        active={props.hudVisible}
        onClick={props.onToggleHud}
      />
      <Sep />
      <ToolBtn icon="🔒" title="远程锁屏" onClick={props.onLockScreen} />
      <ToolBtn icon="⚠" title="Ctrl+Alt+Del (Ctrl+Alt+Q)" onClick={props.onCtrlAltDel} />
      <Sep />
      <ToolBtn icon="─" title="收起工具栏" onClick={() => setExpanded(false)} />
      {panel === "phrases" && <PhrasesPanel onSend={(t) => { props.onSendText(t); setPanel("none"); }} onClose={() => setPanel("none")} />}
      {panel === "vkeys" && <VirtualKeysPanel onCombo={(k) => { props.onSendCombo(k); setPanel("none"); }} onClose={() => setPanel("none")} />}
    </div>
  );
}

function ToolBtn({ icon, title, active, onClick }: { icon: string; title: string; active?: boolean; onClick: () => void }) {
  return (
    <button className={`${styles.toolBtn} ${active ? styles.toolBtnOn : ""}`} title={title} onClick={onClick}>{icon}</button>
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
        <button key={i} className={styles.panelItem} onClick={() => { onSend(text); onClose(); }}>{text}</button>
      ))}
      <div className={styles.panelAdd}>
        <input value={input} placeholder="新短语…" onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && add()} />
        <button onClick={add}>＋</button>
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
      <b className={styles.panelTitle}>虚拟按键</b>
      <div className={styles.modsRow}>
        {["Ctrl", "Alt", "Shift", "Win"].map((m) => (
          <button key={m} className={`${styles.modBtn} ${mods.has(m) ? styles.modOn : ""}`}
            onClick={() => toggle(m)}>{m}</button>
        ))}
      </div>
      <div className={styles.keysGrid}>
        {SPECIAL.map(([l, c]) => <button key={c} className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
      </div>
      <div className={styles.keysGrid}>
        {FKEYS.map(([l, c]) => <button key={c} className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
      </div>
      <div className={styles.keysGrid}>
        {MEDIA.map(([l, c]) => <button key={c} className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
        {SYSTEM.map(([l, c]) => <button key={c} className={styles.keyBtn} onClick={() => send(c)}>{l}</button>)}
      </div>
    </div>
  );
}
