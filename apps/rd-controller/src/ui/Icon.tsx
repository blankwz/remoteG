/**
 * 统一线性图标库（UX Round 1）：替代散落各页的 emoji 图标。
 *
 * 设计约束：24 viewBox / stroke 1.8 / currentColor / 尺寸经 size prop
 * （默认 16），视觉重量与文字协调；所有图标单色，状态色由外层 color 控制。
 */

interface IconProps {
  /** 像素尺寸（宽=高），默认 16。 */
  size?: number;
  className?: string;
}

function base(size: number | undefined, className: string | undefined, children: React.ReactNode) {
  return (
    <svg
      width={size ?? 16}
      height={size ?? 16}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className={className}
    >
      {children}
    </svg>
  );
}

/** 显示器（设备/桌面）。 */
export function IconMonitor({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="2.5" y="4" width="19" height="13" rx="2" />
      <path d="M9 21h6M12 17.5V21" />
    </>
  ));
}

/** 加号（添加）。 */
export function IconPlus({ size, className }: IconProps) {
  return base(size, className, <path d="M12 5v14M5 12h14" />);
}

/** 雷达/扫描。 */
export function IconScan({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <circle cx="12" cy="12" r="4.5" />
      <path d="M12 12l6-6" />
    </>
  ));
}

/** 实验烧瓶（方案实验室）。 */
export function IconLab({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M10 3h4M11 3v6.5L5.5 18a2 2 0 0 0 1.8 3h9.4a2 2 0 0 0 1.8-3L13 9.5V3" />
      <path d="M8 15h8" />
    </>
  ));
}

/** 链路/信号。 */
export function IconSignal({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M4 18v-3M9.5 18v-7M15 18v-11M20.5 18V4" />
    </>
  ));
}

/** 插头/测试连接。 */
export function IconPlug({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M9 7V3M15 7V3" />
      <path d="M7 7h10v4a5 5 0 0 1-10 0V7zM12 16v5" />
    </>
  ));
}

/** 铅笔（编辑）。 */
export function IconEdit({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M15.5 5.5l3 3L8 19l-3.6.6L5 16 15.5 5.5z" />
      <path d="M13.5 7.5l3 3" />
    </>
  ));
}

/** 断开（链条断开）。 */
export function IconUnlink({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M8 12a4 4 0 0 1 0-5.6l2.4-2.4a4 4 0 0 1 5.6 5.6M16 12a4 4 0 0 1 0 5.6L13.6 20a4 4 0 0 1-5.6-5.6" />
    </>
  ));
}

/** 垃圾桶（删除）。 */
export function IconTrash({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M4 7h16M9.5 7V5a1.5 1.5 0 0 1 1.5-1.5h2A1.5 1.5 0 0 1 14.5 5v2" />
      <path d="M6.5 7l1 12a1.5 1.5 0 0 0 1.5 1.4h6a1.5 1.5 0 0 0 1.5-1.4l1-12" />
      <path d="M10 11v6M14 11v6" />
    </>
  ));
}

/** 问号圆圈（帮助）。 */
export function IconHelp({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M9.5 9.2a2.6 2.6 0 0 1 5.1.7c0 1.7-2.6 2.2-2.6 3.6" />
      <path d="M12 17h.01" strokeWidth={2.4} />
    </>
  ));
}

/** 复制。 */
export function IconCopy({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="9" y="9" width="12" height="12" rx="2" />
      <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" transform="translate(2 0)" />
    </>
  ));
}

/** 终端（>_）。 */
export function IconTerminal({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="2.5" y="4" width="19" height="16" rx="2" />
      <path d="M6.5 9l3 3-3 3M12 15h5" />
    </>
  ));
}

/** 相机（截屏）。 */
export function IconCamera({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M3 8.5A2 2 0 0 1 5 6.5h2l1.5-2h7L17 6.5h2a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8.5z" />
      <circle cx="12" cy="13" r="3.5" />
    </>
  ));
}

/** 录制圆点。 */
export function IconRecord({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <circle cx="12" cy="12" r="3.5" fill="currentColor" stroke="none" />
    </>
  ));
}

/** 眼睛（只看）。 */
export function IconEye({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z" />
      <circle cx="12" cy="12" r="3" />
    </>
  ));
}

/** 眼睛关闭（隐私屏）。 */
export function IconEyeOff({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M4 4l16 16" />
      <path d="M10.6 5.8A9.8 9.8 0 0 1 12 5.5c6 0 9.5 6.5 9.5 6.5a17.4 17.4 0 0 1-3.2 3.9M6.6 6.9A16.6 16.6 0 0 0 2.5 12S6 18.5 12 18.5a9.4 9.4 0 0 0 3.4-.6" />
      <path d="M9.9 9.9a3 3 0 0 0 4.2 4.2" />
    </>
  ));
}

/** 信息圆圈。 */
export function IconInfo({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5M12 7.6h.01" strokeWidth={2.2} />
    </>
  ));
}

/** 全屏（四角）。 */
export function IconExpand({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M4 9V5.5A1.5 1.5 0 0 1 5.5 4H9M15 4h3.5A1.5 1.5 0 0 1 20 5.5V9M20 15v3.5a1.5 1.5 0 0 1-1.5 1.5H15M9 20H5.5A1.5 1.5 0 0 1 4 18.5V15" />
    </>
  ));
}

/** 锁（锁屏）。 */
export function IconLock({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="5" y="10.5" width="14" height="10" rx="2" />
      <path d="M8 10.5V7.5a4 4 0 0 1 8 0v3M12 14.5v2.5" />
    </>
  ));
}

/** 键盘（虚拟键）。 */
export function IconKeyboard({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="2.5" y="6" width="19" height="12" rx="2" />
      <path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M6 14h.01M18 14h.01M9 14h6" strokeWidth={2} />
    </>
  ));
}

/** 消息气泡（常用语）。 */
export function IconMessage({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M4 5.5h16a1.5 1.5 0 0 1 1.5 1.5v9a1.5 1.5 0 0 1-1.5 1.5h-9L6 21v-3.5H4A1.5 1.5 0 0 1 2.5 16V7A1.5 1.5 0 0 1 4 5.5z" />
      <path d="M7.5 10h9M7.5 13.5h5" />
    </>
  ));
}

/** 剪贴板上传（发文字）。 */
export function IconClipboardOut({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="5" y="4.5" width="14" height="16" rx="2" />
      <path d="M9 4.5a3 3 0 0 1 6 0M12 16v-6M9.5 12.5L12 10l2.5 2.5" />
    </>
  ));
}

/** 剪贴板下载（取文字）。 */
export function IconClipboardIn({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="5" y="4.5" width="14" height="16" rx="2" />
      <path d="M9 4.5a3 3 0 0 1 6 0M12 10v6M9.5 13.5L12 16l2.5-2.5" />
    </>
  ));
}

/** 仪表（画质）。 */
export function IconGauge({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M4.5 17.5a8.5 8.5 0 1 1 15 0" />
      <path d="M12 14l4-5" />
      <circle cx="12" cy="14.5" r="1.6" fill="currentColor" stroke="none" />
    </>
  ));
}

/** 左箭头（返回）。 */
export function IconArrowLeft({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M19 12H5M11 6l-6 6 6 6" />
    </>
  ));
}

/** 刷新。 */
export function IconRefresh({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M20 12a8 8 0 1 1-2.34-5.66" />
      <path d="M20 4v4.5h-4.5" />
    </>
  ));
}

/** 搜索。 */
export function IconSearch({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <circle cx="11" cy="11" r="6.5" />
      <path d="M20 20l-4.4-4.4" />
    </>
  ));
}

/** 关闭 ×。 */
export function IconClose({ size, className }: IconProps) {
  return base(size, className, <path d="M6 6l12 12M18 6L6 18" />);
}

/** 侧栏开关。 */
export function IconSidebar({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="2.5" y="4" width="19" height="16" rx="2" />
      <path d="M9.5 4v16" />
      <path d="M5 7.5h2M5 10.5h2" strokeWidth={2} />
    </>
  ));
}

/** 收起（chevron 双上）。 */
export function IconChevronUp({ size, className }: IconProps) {
  return base(size, className, <path d="M6 14.5l6-6 6 6" />);
}

/** 展开（chevron 双下）。 */
export function IconChevronDown({ size, className }: IconProps) {
  return base(size, className, <path d="M6 9.5l6 6 6-6" />);
}

/** 电源（CAD/系统）。 */
export function IconPower({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <path d="M12 3v8" />
      <path d="M6.3 6.8a8 8 0 1 0 11.4 0" />
    </>
  ));
}

/** 图像（适配模式）。 */
export function IconFit({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="3" y="5" width="18" height="14" rx="2" />
      <path d="M3 9h18" transform="translate(0 -1.5)" strokeWidth={1.4} opacity={0.5} />
      <path d="M8 15.5h8M12 13v5" opacity={0.9} />
    </>
  ));
}

/** 更多（⋯）。 */
export function IconMore({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <circle cx="5.5" cy="12" r="1.4" fill="currentColor" stroke="none" />
      <circle cx="12" cy="12" r="1.4" fill="currentColor" stroke="none" />
      <circle cx="18.5" cy="12" r="1.4" fill="currentColor" stroke="none" />
    </>
  ));
}

/** 手机/平板（被控中的对端）。 */
export function IconTablet({ size, className }: IconProps) {
  return base(size, className, (
    <>
      <rect x="5" y="2.5" width="14" height="19" rx="2" />
      <path d="M10.5 18.5h3" />
    </>
  ));
}
