/**
 * 自绘 switch（R8 图标评委判词的组件化收口：原生 checkbox 在深色主题下
 * 是「无字形灰方块」）。Devices 弱网纠错与设置页共用——一份事实。
 */
import { useId } from "react";
import styles from "./Switch.module.css";

interface Props {
  readonly checked: boolean;
  readonly onChange: (next: boolean) => void;
  readonly label: string;
  readonly disabled?: boolean;
}

export function Switch({ checked, onChange, label, disabled }: Props) {
  const id = useId();
  return (
    <label className={styles.wrap} htmlFor={id}>
      <input
        id={id}
        type="checkbox"
        role="switch"
        className={styles.input}
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.currentTarget.checked)}
      />
      <span className={styles.track} data-on={checked} aria-hidden="true">
        <span className={styles.thumb} />
      </span>
      <span className={styles.srOnly}>{label}</span>
    </label>
  );
}
