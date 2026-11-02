/**
 * 「新建终端」shell 选择下拉（P2-③ 控件统一）：会话页与窗口列表页共用。
 *
 * 结构与键控协议全站一份：自绘下拉（原生 <details> 无法做打开聚焦/方向键
 * 契约）+ useDismiss menu 契约——打开即聚焦首项，↑↓/Home/End 在项间移焦，
 * Esc 关闭且焦点回触发钮，点外关闭。两页只差触发钮外观（variant）与菜单
 * 对齐方向（primary 靠右），样式由本组件的 module.css 持有。
 */
import { useCallback, useRef, useState } from "react";
import { useDismiss } from "./useDismiss";
import { IconChevronDown, IconTerminal } from "./Icon";
import styles from "./ShellMenu.module.css";

/** 下拉选项：id 为页面语义的 shell 标识（会话页传白名单 exe 名，窗口页传 URL 短名）。 */
export interface ShellOption<ID extends string = string> {
  readonly id: ID;
  readonly label: string;
}

interface Props<ID extends string = string> {
  shells: readonly ShellOption<ID>[];
  onPick: (id: ID) => void;
  /** 触发钮外观：toolbar=会话页工具钮（描边）；primary=窗口页主操作钮（实心）。 */
  variant?: "toolbar" | "primary";
  /** 触发钮文字。 */
  label?: string;
}

export function ShellMenu<ID extends string = string>({
  shells,
  onPick,
  variant = "toolbar",
  label = "新建终端",
}: Props<ID>) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  /** 菜单本体：键控/初始聚焦只在此范围内循环（不含触发钮，APG menu 语义）。 */
  const menuRef = useRef<HTMLDivElement>(null);
  useDismiss(open, wrapRef, useCallback(() => setOpen(false), []), {
    menu: true,
    menuScope: menuRef,
  });

  return (
    <div className={styles.wrap} ref={wrapRef}>
      <button
        type="button"
        className={variant === "primary" ? styles.primaryBtn : styles.toolbarBtn}
        onClick={() => setOpen((v) => !v)}
        title="打开远端终端"
        aria-haspopup="menu"
        aria-expanded={open}
      >
        <IconTerminal size={variant === "primary" ? 14 : 13} />
        {label}
        <IconChevronDown size={12} className={styles.caret} />
      </button>
      {open && (
        <div
          ref={menuRef}
          className={
            variant === "primary"
              ? `${styles.menu} ${styles.menuRight}`
              : styles.menu
          }
          role="menu"
          aria-label="选择终端 shell"
        >
          {shells.map((s) => (
            <button
              key={s.id}
              type="button"
              className={styles.item}
              role="menuitem"
              onClick={() => {
                setOpen(false);
                onPick(s.id);
              }}
            >
              <IconTerminal size={13} />
              {s.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
