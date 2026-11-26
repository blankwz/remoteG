/**
 * 移动端极客快捷键盘条 + 系统输入法桥接（借鉴 RustDesk v1.4.5+ Mobile Extra Key Bar）。
 *
 * - 粘滞修饰键（Ctrl / Alt / Win / Shift）：单拇指点亮后，再点任意键自动组合发送并释放；
 * - 常用一键组合：Esc / Tab / Ctrl+C / Ctrl+V / Alt+Tab / 方向键 / Del / 终端符号；
 * - 原生输入法（IME）直输：点「输入文字」拉起 Android/iOS 中文/英文软键盘，回车直发远端。
 */
import { useRef, useState, type FormEvent } from "react";
import styles from "./MobileKeyboardBar.module.css";

interface MobileKeyboardBarProps {
  onSendCombo: (keys: string[]) => void;
  onSendText: (text: string) => void;
  onClose: () => void;
}

const MODIFIERS = [
  { id: "Control", label: "Ctrl" },
  { id: "Alt", label: "Alt" },
  { id: "Meta", label: "Win" },
  { id: "Shift", label: "Shift" },
] as const;

const QUICK_KEYS: readonly { label: string; keys: string[] }[] = [
  { label: "Esc", keys: ["Escape"] },
  { label: "Tab", keys: ["Tab"] },
  { label: "Ctrl+C", keys: ["Control", "c"] },
  { label: "Ctrl+V", keys: ["Control", "v"] },
  { label: "Alt+Tab", keys: ["Alt", "Tab"] },
  { label: "↑", keys: ["ArrowUp"] },
  { label: "↓", keys: ["ArrowDown"] },
  { label: "←", keys: ["ArrowLeft"] },
  { label: "→", keys: ["ArrowRight"] },
  { label: "Home", keys: ["Home"] },
  { label: "End", keys: ["End"] },
  { label: "Del", keys: ["Delete"] },
  { label: "F5", keys: ["F5"] },
];

const TERM_SYMBOLS = ["/", "-", "|", "~", ":", "?", "_", "&&"] as const;

export function MobileKeyboardBar({
  onSendCombo,
  onSendText,
  onClose,
}: MobileKeyboardBarProps) {
  const [armedMods, setArmedMods] = useState<string[]>([]);
  const [textDraft, setTextDraft] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const toggleMod = (mod: string) => {
    setArmedMods((prev) =>
      prev.includes(mod) ? prev.filter((m) => m !== mod) : [...prev, mod],
    );
  };

  const fireKey = (keys: string[]) => {
    const merged = Array.from(new Set([...armedMods, ...keys]));
    onSendCombo(merged);
    if (armedMods.length > 0) {
      setArmedMods([]);
    }
  };

  const submitText = (e: FormEvent) => {
    e.preventDefault();
    if (!textDraft) return;
    onSendText(textDraft);
    setTextDraft("");
  };

  return (
    <div className={styles.bar} role="region" aria-label="移动端快捷键盘条">
      <div className={styles.topRow}>
        <form className={styles.imeForm} onSubmit={submitText}>
          <input
            ref={inputRef}
            type="text"
            className={styles.imeInput}
            value={textDraft}
            onChange={(e) => setTextDraft(e.currentTarget.value)}
            placeholder="点此拉起手机输入法（中/英文直接发往远端）…"
            aria-label="移动端文本输入"
          />
          <button
            type="submit"
            className={styles.sendBtn}
            disabled={textDraft.length === 0}
          >
            发送
          </button>
        </form>
        <button
          type="button"
          className={styles.closeBtn}
          onClick={onClose}
          aria-label="收起快捷键盘条"
        >
          收起
        </button>
      </div>

      {/* 粘滞修饰键 + 核心功能键 */}
      <div className={styles.keyStrip}>
        {MODIFIERS.map((m) => {
          const active = armedMods.includes(m.id);
          return (
            <button
              key={m.id}
              type="button"
              className={active ? `${styles.keyBtn} ${styles.keyBtnActive}` : styles.keyBtn}
              aria-pressed={active}
              onClick={() => toggleMod(m.id)}
            >
              {m.label}
            </button>
          );
        })}
        {QUICK_KEYS.map((k) => (
          <button
            key={k.label}
            type="button"
            className={styles.keyBtn}
            onClick={() => fireKey(k.keys)}
          >
            {k.label}
          </button>
        ))}
      </div>

      {/* 终端/编程高频符号行 */}
      <div className={styles.keyStrip}>
        {TERM_SYMBOLS.map((sym) => (
          <button
            key={sym}
            type="button"
            className={styles.symBtn}
            onClick={() => onSendText(sym)}
          >
            {sym}
          </button>
        ))}
      </div>
    </div>
  );
}
