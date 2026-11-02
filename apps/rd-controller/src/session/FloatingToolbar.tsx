/**
 * 悬浮工具栏（UX Round 1 重做）：
 * - 一行高频 6 组：画质│只看 截屏 录屏 锁屏 CAD│⋯更多 收起——单行不折行
 * - 全量 SVG 线性图标（Icon.tsx），废除 emoji；标签 11px 起步
 * - 画质受控（value 由页面层持有，重连/外部切换后显示真实档位）
 * - 更多菜单收纳低频：常用语/虚拟键/发文字/取文字/隐私屏/画面信息/全屏
 */
import { useRef, useState } from "react";
import { useDismiss } from "../ui/useDismiss";
import {
  IconCamera,
  IconChevronDown,
  IconChevronUp,
  IconClipboardIn,
  IconClipboardOut,
  IconClose,
  IconEye,
  IconEyeOff,
  IconExpand,
  IconFolder,
  IconGauge,
  IconInfo,
  IconKeyboard,
  IconLock,
  IconMessage,
  IconMore,
  IconRecord,
} from "../ui/Icon";
import styles from "./FloatingToolbar.module.css";
import {
  PHRASE_DEFAULTS,
  PHRASES_MAX,
  readCustomPhrases,
  saveCustomPhrases,
} from "./phrases";

export type Quality = "auto" | "high" | "medium" | "low";

/** 画质档位中文名（P2-① 页面层 toast 与下拉共用一套词）。 */
export const QUALITY_LABEL: Record<Quality, string> = {
  auto: "自动",
  high: "高清",
  medium: "均衡",
  low: "流畅",
};

interface Props {
  onScreenshot: () => void;
  onFullscreen: () => void;
  onLockScreen: () => void;
  onCtrlAltDel: () => void;
  onQualityChange: (q: Quality) => void;
  /** 当前画质档位（受控：真实状态由页面层持有）。 */
  quality: Quality;
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
  /** 打开文件传输面板（T-031）。 */
  onFiles: () => void;
  /** HUD 信息显隐切换。 */
  onToggleHud: () => void;
  /** 当前 HUD 显隐（高亮 = 常显）。 */
  hudVisible: boolean;
  /** 打开快捷键速查（P1-1：? 键的唯一可见入口；速查卡本体在页面层）。 */
  onShortcuts: () => void;
  /** 激活标签是否为窗口组（R11 交互#5：终端标签下隐藏只看/截屏/录屏/
   * 剪贴板等窗口专属操作——可点但必错的假可用性）。 */
  windowActions: boolean;
  /** 只看态（R12：设备级注入通道一并停发——锁屏/安全键/常用语/虚拟按键
   * 置灰，与「键鼠停发」的承诺对齐）。 */
  inputLocked: boolean;
}

export function FloatingToolbar(props: Props) {
  // 手动收起（无自动收起）：展开是默认态
  const [expanded, setExpanded] = useState(true);
  const [panel, setPanel] = useState<"none" | "phrases" | "vkeys" | "more">("none");
  const rootRef = useRef<HTMLDivElement>(null);

  // 点外/Escape 关闭弹出面板——协议统一走 useDismiss（R11 D08 唯一出口；
  // 早年不依赖 onMouseLeave——鼠标出界丢输入，Walter R2）
  useDismiss(panel !== "none", rootRef, () => setPanel("none"), { trap: true });

  if (!expanded) {
    return (
      <button
        type="button"
        className={styles.dot}
        title="展开工具栏"
        // 收起时原「收起」钮被卸载，autoFocus 接住焦点防丢 BODY（R11 无障碍）
        autoFocus
        onClick={() => setExpanded(true)}
      >
        <IconChevronDown size={16} />
      </button>
    );
  }

  return (
    <div className={styles.bar} ref={rootRef}>
      <label className={styles.qualityWrap} title="画质：自动=智能自适应；弱网选流畅">
        <IconGauge size={15} className={styles.qualityIcon} />
        <select
          className={styles.select}
          value={props.quality}
          onChange={(e) => props.onQualityChange(e.target.value as Quality)}
        >
          {(Object.keys(QUALITY_LABEL) as Quality[]).map((q) => (
            <option key={q} value={q}>
              {QUALITY_LABEL[q]}
            </option>
          ))}
        </select>
      </label>
      <Sep />
      {props.windowActions && (
        <>
          <ToolBtn
            icon={<IconEye size={16} />}
            label="只看"
            title="只看不控：键鼠不发给远端（Ctrl+Shift+V），即时生效，再点一次恢复"
            active={props.viewOnly}
            onClick={props.onToggleViewOnly}
          />
          <ToolBtn
            icon={<IconCamera size={16} />}
            label="截屏"
            title="保存当前远程画面到下载文件夹（Ctrl+Shift+S）"
            onClick={props.onScreenshot}
          />
          <ToolBtn
            icon={<IconRecord size={16} className={props.recordingOn ? styles.recOn : undefined} />}
            label={props.recordingOn ? "停止" : "录屏"}
            title={props.recordingOn ? "停止录制（存到下载文件夹）" : "录制远程画面（存到下载文件夹）"}
            active={props.recordingOn}
            onClick={props.onRecordToggle}
          />
        </>
      )}
      <ToolBtn
        icon={<IconLock size={16} />}
        label="锁屏"
        title="远端锁屏（Ctrl+Shift+L，3 秒内再点确认）"
        disabled={props.inputLocked}
        onClick={props.onLockScreen}
      />
      <ToolBtn
        icon={<IconKeyboard size={16} />}
        label="安全键"
        title="向远端发送 Ctrl+Alt+Del（远端卡死时呼出系统安全界面/任务管理器）"
        disabled={props.inputLocked}
        onClick={props.onCtrlAltDel}
      />
      <Sep />
      <ToolBtn
        icon={<IconMore size={16} />}
        label="更多"
        title="常用语、虚拟按键、剪贴板、隐私屏、全屏"
        active={panel === "more"}
        ariaExpanded={panel === "more"}
        ariaHaspopup="menu"
        onClick={() => setPanel(panel === "more" ? "none" : "more")}
      />
      <button
        type="button"
        className={styles.collapse}
        title="收起工具栏"
        onClick={() => setExpanded(false)}
      >
        <IconChevronUp size={14} />
      </button>
      {panel === "phrases" && (
        <PhrasesPanel
          onSend={(t) => {
            props.onSendText(t);
            setPanel("none");
          }}
          onClose={() => setPanel("none")}
        />
      )}
      {panel === "vkeys" && (
        <VirtualKeysPanel
          onCombo={(k) => {
            props.onSendCombo(k);
            setPanel("none");
          }}
          onClose={() => setPanel("none")}
        />
      )}
      {panel === "more" && (
        <MorePanel
          {...props}
          onPhrases={() => setPanel("phrases")}
          onVkeys={() => setPanel("vkeys")}
          onClose={() => setPanel("none")}
        />
      )}
    </div>
  );
}

/** 图文双行按钮（图标上、文字下；active 高亮）。 */
function ToolBtn({
  icon,
  label,
  title,
  active,
  disabled,
  onClick,
  ariaExpanded,
  ariaHaspopup,
}: {
  icon: React.ReactNode;
  label: string;
  title: string;
  active?: boolean;
  disabled?: boolean;
  onClick: () => void;
  /** 弹出面板触发钮的展开态语义（R11 D14）。 */
  ariaExpanded?: boolean;
  ariaHaspopup?: "menu";
}) {
  const hint = disabled ? `${title}（只看中：恢复控制后可用）` : title;
  return (
    <button
      type="button"
      className={`${styles.toolBtn} ${active ? styles.toolBtnOn : ""}`}
      title={hint}
      disabled={disabled}
      onClick={onClick}
      aria-expanded={ariaExpanded}
      aria-haspopup={ariaHaspopup}
    >
      <span className={styles.toolIcon}>{icon}</span>
      <span className={styles.toolLabel}>{label}</span>
    </button>
  );
}

function Sep() {
  return <span className={styles.sep} />;
}

// ── 更多菜单 ──
function MorePanel(
  props: Props & { onPhrases: () => void; onVkeys: () => void; onClose: () => void },
) {
  const items: Array<{
    key: string;
    icon: React.ReactNode;
    label: string;
    hint: string;
    active?: boolean;
    disabled?: boolean;
    on: () => void;
  }> = [
    {
      key: "phrases",
      icon: <IconMessage size={15} />,
      label: "常用语",
      hint: props.inputLocked ? "只看中：恢复控制后可用" : "常用短语一键发送",
      disabled: props.inputLocked,
      on: props.onPhrases,
    },
    {
      key: "vkeys",
      icon: <IconKeyboard size={15} />,
      label: "虚拟按键",
      hint: props.inputLocked ? "只看中：恢复控制后可用" : "组合键 / 特殊键",
      disabled: props.inputLocked,
      on: props.onVkeys,
    },
    // 剪贴板走窗口组控制器，终端标签下不可用（交互#5：不亮假按钮）
    ...(props.windowActions
      ? [
          {
            key: "copyTo",
            icon: <IconClipboardOut size={15} />,
            label: "发送剪贴板",
            hint: "本机文字 → 远端（Ctrl+Shift+X）",
            on: props.onCopyToRemote,
          },
          {
            key: "copyFrom",
            icon: <IconClipboardIn size={15} />,
            label: "取回剪贴板",
            hint: "远端文字 → 本机（Ctrl+Shift+C）",
            on: props.onCopyFromRemote,
          },
        ]
      : []),
    {
      key: "files",
      icon: <IconFolder size={15} />,
      label: "文件传输",
      hint: "发送到远端 / 浏览远端文件",
      on: props.onFiles,
    },
    {
      key: "privacy",
      icon: props.privacyOn ? <IconEye size={15} /> : <IconEyeOff size={15} />,
      label: props.privacyOn ? "关闭隐私屏" : "隐私屏",
      hint: props.privacyOn ? "恢复对方屏幕显示" : "对方屏幕变黑，操作照常",
      active: props.privacyOn,
      on: () => props.onPrivacyToggle(!props.privacyOn),
    },
    {
      key: "hud",
      icon: <IconInfo size={15} />,
      label: "画面信息",
      hint: "常显指标 · Ctrl+Shift+H",
      active: props.hudVisible,
      on: props.onToggleHud,
    },
    {
      key: "fullscreen",
      icon: <IconExpand size={15} />,
      label: "全屏",
      hint: "全屏切换",
      on: props.onFullscreen,
    },
    {
      key: "shortcuts",
      icon: <IconKeyboard size={15} />,
      label: "快捷键",
      hint: "速查卡 · 也可随时按 ?",
      on: props.onShortcuts,
    },
  ];
  return (
    <div className={styles.panel} role="menu" aria-label="更多操作">
      <b className={styles.panelTitle}>更多操作</b>
      {items.map((it) => (
        <button
          key={it.key}
          type="button"
          className={`${styles.moreItem} ${it.active ? styles.moreItemOn : ""}`}
          disabled={it.disabled}
          onClick={() => {
            if (it.disabled) {
              return;
            }
            it.on();
            if (it.key === "phrases" || it.key === "vkeys") {
              return; // 面板切换由对应 panel 接管
            }
            props.onClose();
          }}
        >
          <span className={styles.moreIcon}>{it.icon}</span>
          <span className={styles.moreText}>
            <span className={styles.moreLabel}>{it.label}</span>
            <span className={styles.moreHint}>{it.hint}</span>
          </span>
        </button>
      ))}
      <button type="button" className={styles.closeRow} onClick={props.onClose}>
        <IconClose size={13} /> 关闭菜单
      </button>
    </div>
  );
}

// ── 常用语面板 ──
/** 自定义条目行：发送按钮 + 删除小钮（同级兄弟，避免嵌套可交互）。 */
function CustomPhraseRow({
  text,
  onSend,
  onRemove,
}: {
  text: string;
  onSend: (t: string) => void;
  onRemove: (t: string) => void;
}) {
  return (
    <span className={styles.phraseRow} role="presentation">
      <button
        type="button"
        className={styles.panelItem}
        onClick={() => onSend(text)}
      >
        {text}
      </button>
      <button
        type="button"
        className={styles.phraseDel}
        aria-label={`删除常用语「${text}」`}
        title="删除该常用语"
        onClick={() => onRemove(text)}
      >
        ×
      </button>
    </span>
  );
}

function PhrasesPanel({
  onSend,
  onClose,
}: {
  onSend: (t: string) => void;
  onClose: () => void;
}) {
  const [custom, setCustom] = useState<readonly string[]>(readCustomPhrases);
  const [input, setInput] = useState("");

  const add = () => {
    const text = input.trim();
    if (text.length === 0) {
      return;
    }
    const next = [...custom, text].slice(-PHRASES_MAX);
    setCustom(next);
    saveCustomPhrases(next);
    setInput("");
  };

  const remove = (text: string) => {
    const next = custom.filter((p) => p !== text);
    setCustom(next);
    saveCustomPhrases(next);
  };

  return (
    <div className={styles.panel} role="menu" aria-label="常用语">
      <b className={styles.panelTitle}>常用语（点击发送到远端）</b>
      {PHRASE_DEFAULTS.map((text) => (
        <button
          key={text}
          type="button"
          className={styles.panelItem}
          onClick={() => {
            onSend(text);
            onClose();
          }}
        >
          {text}
        </button>
      ))}
      {custom.map((text) => (
        <CustomPhraseRow
          key={text}
          text={text}
          onSend={(t) => {
            onSend(t);
            onClose();
          }}
          onRemove={remove}
        />
      ))}
      <div className={styles.panelAdd}>
        <input
          value={input}
          placeholder="新短语…"
          aria-label="新增常用短语"
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && add()}
        />
        <button type="button" onClick={add}>
          添加
        </button>
      </div>
      <span className={styles.panelHint}>
        内置 5 条不可删；自定义短语可点 × 删除（与设置页同步）
      </span>
    </div>
  );
}

// ── 虚拟按键面板 ──
function VirtualKeysPanel({
  onCombo,
  onClose,
}: {
  onCombo: (k: string[]) => void;
  onClose: () => void;
}) {
  const [mods, setMods] = useState<Set<string>>(new Set());
  const toggle = (m: string) =>
    setMods((prev) => {
      const n = new Set(prev);
      if (n.has(m)) {
        n.delete(m);
      } else {
        n.add(m);
      }
      return n;
    });

  const send = (...keys: string[]) => {
    onCombo([...mods, ...keys]);
    setMods(new Set()); // 发送后清修饰键，防下次误发（Walter R2）
  };

  const SPECIAL: Array<[string, string]> = [
    ["Tab", "Tab"],
    ["Esc", "Escape"],
    ["Enter", "Enter"],
    ["⌫", "Backspace"],
    ["Del", "Delete"],
    ["Home", "Home"],
    ["End", "End"],
    ["PgUp", "PageUp"],
    ["PgDn", "PageDown"],
    ["↑", "ArrowUp"],
    ["↓", "ArrowDown"],
    ["←", "ArrowLeft"],
    ["→", "ArrowRight"],
  ];
  const FKEYS = Array.from({ length: 12 }, (_, i) => [`F${i + 1}`, `F${i + 1}`] as [string, string]);
  const MEDIA: Array<[string, string]> = [
    ["声音+", "VolumeUp"],
    ["声音-", "VolumeDown"],
    ["静音", "VolumeMute"],
    ["播放", "MediaPlayPause"],
  ];
  const SYSTEM: Array<[string, string]> = [
    ["Win", "MetaLeft"],
    ["Win+D", "WinD"],
    ["Alt+Tab", "AltTab"],
  ];

  return (
    <div className={`${styles.panel} ${styles.panelWide}`} role="menu" aria-label="虚拟按键">
      <b className={styles.panelTitle}>虚拟按键（先点修饰键组合，再点目标键）</b>
      <div className={styles.modsRow}>
        {["Ctrl", "Alt", "Shift", "Win"].map((m) => (
          <button
            key={m}
            type="button"
            className={`${styles.modBtn} ${mods.has(m) ? styles.modOn : ""}`}
            onClick={() => toggle(m)}
          >
            {m}
          </button>
        ))}
      </div>
      <div className={styles.keysGrid}>
        {SPECIAL.map(([l, c]) => (
          <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>
            {l}
          </button>
        ))}
      </div>
      <div className={styles.keysGrid}>
        {FKEYS.map(([l, c]) => (
          <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>
            {l}
          </button>
        ))}
      </div>
      <div className={styles.keysGrid}>
        {MEDIA.map(([l, c]) => (
          <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>
            {l}
          </button>
        ))}
        {SYSTEM.map(([l, c]) => (
          <button key={c} type="button" className={styles.keyBtn} onClick={() => send(c)}>
            {l}
          </button>
        ))}
      </div>
      <button
        type="button"
        className={styles.closeRow}
        onClick={onClose}
        title="关闭面板"
      >
        <IconClose size={13} /> 关闭
      </button>
    </div>
  );
}
