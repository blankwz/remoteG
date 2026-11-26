/**
 * 已修债务回归门禁（R11 Phase 1，防线先行）。
 *
 * 方向说明：与「known-debts 探针仍命中→绿」相反——本清单登记的是**已修复**
 * 的债务，其探针是「回潮时必然重新出现的源码字符串」，门禁断言探针保持
 * 消失（或计数不升）。谁把老病写回来，`pnpm test` 当场红，评审不再扣分。
 *
 * 效度纪律（ux-fastloop §四）：
 * - 探针必须锚定债务症状本身（修复时被删除/改写的串），不是无关锚；
 * - 扫描器是纯函数（scanGate），自带注入活性验证：喂入含探针的内容必须
 *   报违规——扫描器失灵 = 橡皮图章，测试同级拦下。
 */

/** 门禁条目：probe 在 file 中必须消失（maxCount 省略时 0 命中）。 */
export interface DebtGate {
  /** 债务 id（对应 评审记录-round11.md 的 D 编号）。 */
  readonly id: string;
  /** 一句话症状。 */
  readonly title: string;
  /** src/ 相对路径（该文件不存在 = 清单失实，同样红）。 */
  readonly file: string;
  /** 回潮探针：修复时从该文件删除的源码字符串（原样锚）。 */
  readonly probe: string;
  /** 允许的最大命中数（缺省 0；用于「合法残留恰 N 处」的计数语义）。 */
  readonly maxCount?: number;
}

/** 全局不变量：跨文件的口径/纪律族门禁（file 为 src/ 相对目录或前缀）。 */
export interface SourceInvariant {
  readonly id: string;
  readonly title: string;
  /** 约束的文件前缀（如 "pages/"）或精确文件。 */
  readonly scope: string;
  /** 字面探针（与 regex 二选一）。 */
  readonly probe?: string;
  /** 正则探针（与 probe 二选一；字符串形态，扫描器 new RegExp）。 */
  readonly regex?: string;
  readonly maxCount: number;
}

/** 已修债务清单（16 项中可源码锚定者；行为级债务由单测/评审覆盖）。 */
export const DEBT_GATES: readonly DebtGate[] = [
  {
    id: "D01",
    title: "录屏切标签丢文件（假承诺文案与卸载丢弃注释回潮）",
    file: "session/WindowGroupView.tsx",
    probe: "（丢文件）",
  },
  {
    id: "D01b",
    title: "REC 徽标假承诺「切走标签会保存」",
    file: "pages/Session.tsx",
    probe: "切走标签会停止录制并保存",
  },
  {
    id: "D02",
    title: "保留快捷键散抄（绕过 isReservedCombo 唯一出口）",
    file: "input/InputMapper.ts",
    probe: 'k.code === "KeyH" || k.code === "KeyS"',
  },
  {
    id: "D05",
    title: "侧栏徽标一律「只看中」（与真实只看态撞词且语义反）",
    file: "session/WindowSidebar.tsx",
    probe: '{active ? "只看中" : "已打开"}',
  },
  {
    id: "D09",
    title: "捕获 textarea aria-hidden 持焦点（合法残留：纯装饰 spinner / 品牌层 / 0x0 SVG 锐化滤镜 defs 各 1 处，均不持焦点——axe aria-hidden-focus 门禁行为级兜底）",
    file: "session/WindowGroupView.tsx",
    probe: 'aria-hidden="true"',
    maxCount: 3,
  },
  {
    id: "D13",
    title: "第 4 种琥珀色散值（warn 色系碎四套）",
    file: "session/Session.module.css",
    probe: "#ff9830",
  },
  {
    id: "D14",
    title: "手打字符图标 ⟳（图标库旁路）",
    file: "session/WindowSidebar.tsx",
    probe: "⟳",
  },
  {
    id: "D15",
    title: "span[role=button] 假按钮回潮（嵌套交互元素）",
    file: "session/WindowSidebar.tsx",
    probe: 'role="button"',
  },
  {
    id: "D15b",
    title: "标签内 span[role=button] 假按钮回潮",
    file: "pages/Session.tsx",
    probe: 'role="button"',
  },
  {
    id: "D17",
    title: "诊断探针 __fc 回潮（console.log + window 全局计数）",
    file: "session/WindowGroupView.tsx",
    probe: "__fc",
  },
  {
    id: "D17b",
    title: "诊断探针 __dec 回潮",
    file: "video/decoder.ts",
    probe: "__dec",
  },
  {
    id: "D18",
    title: "圆角双分号 6px;; 回潮",
    file: "session/Session.module.css",
    probe: "6px;;",
  },
  {
    id: "D19",
    title: ".version 叠 opacity 致对比度 4.08:1",
    file: "pages/Devices.module.css",
    probe: "opacity: 0.7",
  },
  {
    id: "R12-viewOnly-ref",
    title: "渲染期读 sessionCtlRef.viewOnly（快照化前反模式，慢一拍且切换路径易漏刷）",
    file: "pages/Session.tsx",
    probe: "sessionCtlRef.current?.viewOnly",
  },
  {
    id: "R12-vkeys-reserved",
    title: "保留快捷键绕过 isReservedCombo 散抄（终端放行/页面判定共用唯一出口）",
    file: "term/TermView.tsx",
    probe: 'ev.code === "KeyH"',
  },
];

/** 跨文件纪律不变量。 */
export const SOURCE_INVARIANTS: readonly SourceInvariant[] = [
  {
    id: "INV-bridge",
    title: "页面/会话层禁裸 invoke（必须经 bridge.call 收口，demo 才有人话）",
    scope: "pages/",
    probe: 'from "@tauri-apps/api/core"',
    maxCount: 0,
  },
  {
    id: "INV-bridge-session",
    title: "会话组件层禁裸 invoke（同上）",
    scope: "session/",
    probe: 'from "@tauri-apps/api/core"',
    maxCount: 0,
  },
  {
    id: "INV-hotkey",
    title: "页面级 onKey 只许注册一次（双入口回归：同键本地走两次）",
    scope: "pages/Session.tsx",
    probe: 'addEventListener("keydown", onKey)',
    maxCount: 1,
  },
  {
    id: "INV-console",
    title: "console.log 禁入生产源码（诊断走 decoder.emitStats 通道）",
    scope: "",
    probe: "console.log(",
    maxCount: 0,
  },
  {
    id: "INV-fs",
    title: "裸 font-size px 禁令（字号阶梯唯一走 --rd-fs-* 令牌；正则全段覆盖，R14 校准）",
    scope: "",
    regex: "font-size:\s*\d+px",
    maxCount: 0,
  },
  {
    id: "INV-nul",
    title: "源码禁字面 NUL 字节（曾致整文件被 grep 判 binary，可搜索性自伤）",
    scope: "",
    probe: "\u0000",
    maxCount: 0,
  },
];

export interface GateViolation {
  readonly id: string;
  readonly title: string;
  readonly detail: string;
}

/**
 * 扫描单个门禁（纯函数，注入活性验证的对象）：contents 为「src/ 相对路径 →
 * 文件内容」。返回违规描述，null = 通过。
 */
export function scanGate(
  contents: ReadonlyMap<string, string>,
  gate: DebtGate,
): GateViolation | null {
  const content = contents.get(gate.file);
  if (content === undefined) {
    return {
      id: gate.id,
      title: gate.title,
      detail: `清单失实：${gate.file} 不存在（门禁清单与实态双向锁定）`,
    };
  }
  const hits = content.split(gate.probe).length - 1;
  const max = gate.maxCount ?? 0;
  if (hits > max) {
    return {
      id: gate.id,
      title: gate.title,
      detail: `探针回潮：${gate.file} 中 "${gate.probe}" 命中 ${hits} 处（上限 ${max}）`,
    };
  }
  return null;
}

/** 扫描跨文件不变量：scope 为空 = 全 src；否则按路径前缀过滤。 */
export function scanInvariant(
  contents: ReadonlyMap<string, string>,
  inv: SourceInvariant,
): GateViolation | null {
  const matcher = inv.regex !== undefined ? new RegExp(inv.regex, "g") : null;
  let hits = 0;
  const hitFiles: string[] = [];
  for (const [path, content] of contents) {
    if (inv.scope !== "" && !path.startsWith(inv.scope)) {
      continue;
    }
    const n = matcher !== null
      ? (content.match(matcher) ?? []).length
      : content.split(inv.probe ?? "").length - 1;
    if (n > 0) {
      hits += n;
      hitFiles.push(`${path}×${n}`);
    }
  }
  if (hits > inv.maxCount) {
    return {
      id: inv.id,
      title: inv.title,
      detail: `探针命中 ${hits} 处（上限 ${inv.maxCount}）：${hitFiles.join(", ")}`,
    };
  }
  return null;
}
