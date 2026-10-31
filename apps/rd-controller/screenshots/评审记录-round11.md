# Round 11 全量债务盘点（四视角并行走查 · 评审对象 round11-audit 冻结快照）

> 轨迹：… → 82.2 → 83 → **84.3（R10 GO）** → 本轮 **81.3（回撤 -3.0）**
> 回撤根因：R10 之后混入 35 个未提交改动（工具栏重设计/方案实验室/loopback 穿透提示/api 拆分）**从未评审**。
> 四视角独立打分：无障碍 **79** / 排错效率 **81** / 交互效率 **82** / 设计跃迁 **83**。
> 探针效度：13 个探针逐一经 grep 实证命中（1 个修正为实际字符串，1 个改为文件内计数语义）。
> 截图：[round11-audit](round11-audit/)（5 张）。测试基线 114 项全绿。

## 共同判词

排错官：「容错外壳已修好，反馈诚实性在回潮——三处报喜不报忧的假成功恰在用户最需要排错情报时掩盖真相。」
无障碍官：「第 10 轮之后的改动把『焦点去哪儿了』这条主线丢了，典型『鼠标测过 ≠ 键盘没问题』。」
交互官：「35 个改动把『快』做进去了、把『准』做丢了（热键双入口 P0）。」
设计官：「水位未崩，接缝在松（令牌纪律三处口子 + 调试工具顶在产品门面）。」

---

## known-debts 机检清单（探针已实证）

```ts
// knownDebts 草案 —— 修复后探针应消失（或按注明的计数语义变化）
export const KNOWN_DEBTS = [
  // ── P0 ──
  { id: "D01", title: "录屏切标签丢文件+三信号矛盾", file: "src/session/WindowGroupView.tsx", probe: "切走标签会停止录制并保存" }, // 互斥串:（丢文件）@同文件:1049
  { id: "D02", title: "Ctrl+Shift+V/L 热键泄露远端且本地双触发", file: "src/input/InputMapper.ts", probe: 'k.code === "KeyH" || k.code === "KeyS"' }, // 另: Session.tsx onKey 重复注册 ×2
  // ── P1 假反馈/口径族 ──
  { id: "D03", title: "剪贴板空内容报已发送", file: "src/pages/Session.tsx", probe: "已发送到远端剪贴板（远端 Ctrl+V 粘贴）" },
  { id: "D04", title: "终端 pill 恒显已连接", file: "src/pages/Session.tsx", probe: "终端会话进行中" },
  { id: "D05", title: "侧栏激活窗口标「只看中」语义反了", file: "src/session/WindowSidebar.tsx", probe: '{active ? "只看中" : "已打开"}' },
  { id: "D06", title: "实验室/切断/弱网三处绕过 bridge", file: "src/pages/Devices.tsx", probe: 'from "@tauri-apps/api/core"' }, // 修复后 0 命中; mock.ts 缺 3 条命令
  { id: "D13", title: "第 4 种琥珀色（warn 色系碎成四套）", file: "src/session/Session.module.css", probe: "#ff9830" },
  // ── P1 可达性族 ──
  { id: "D07", title: "添加设备对话框无焦点陷阱/关闭焦点丢 BODY", file: "src/pages/Devices.tsx", probe: "const closeAdd = useCallback(() => {" },
  { id: "D08", title: "新建终端下拉不遵循弹层协议（无 Esc/点外）", file: "src/pages/Session.tsx", probe: "setTermMenuOpen((v) => !v)" },
  { id: "D09", title: "捕获 textarea aria-hidden 持焦点", file: "src/session/WindowGroupView.tsx", probe: 'aria-hidden="true"' }, // 计数语义: 修复后 2→1
  // ── P2 ──
  { id: "D14", title: "开合触发器批量缺 aria-expanded（5 处）+⟳ 字符图标", file: "src/pages/Devices.tsx", probe: 'aria-label={d.name + " 的更多操作"}' },
  { id: "D15", title: "button 内嵌 span[role=button]（tabClose/条目 close）", file: "src/pages/Session.tsx", probe: '<span\n          role="button"\n          tabIndex={0}' },
  { id: "D16", title: "常用语输入框无可访问名", file: "src/session/FloatingToolbar.tsx", probe: 'placeholder="新短语…"' },
  { id: "D17", title: "诊断探针残留 __fc/__fb/__dec + console.log", file: "src/session/WindowGroupView.tsx", probe: '"[probe] first onFrame bytes="' }, // 另: src/video/decoder.ts __dec
  { id: "D18", title: "CSS 重复块+6px;;×13+死令牌 --rd-fs-*+内联野图标 r=1.6", file: "src/session/Session.module.css", probe: ".privacyBadge" }, // 计数 ≤4; 另: 6px;; 0 命中
  { id: "D19", title: ".version 对比度 4.08:1（faint 叠 opacity:0.7）", file: "src/pages/Devices.module.css", probe: "opacity: 0.7" },
];
```

**次级备案**（不占清单）：HUD「画质 适应×100%」与 tooltip 口径不一；toast 定时器卸载后 setState；注释「固定 Fit」实传 SCALE；错误 toast 应 `role="alert"`；TermView 遮罩无 aria-live；速查表与实现脱节（5 条 vs 实有 8+）；会话页断开 4 击且无工具栏入口。

## 分拣

| 去向 | 项 | 说明 |
|---|---|---|
| **Phase 1 门禁（机检）** | D02(监听计数) D05 D06(bridge 收口 grep) D09(计数) D13 D17 D18(双分号/计数) D19(对比度脚本) + aria-expanded 不变量 + axe-core（aria-hidden-focus / nested-interactive）+ 焦点回归断言（弹层关闭焦点回 trigger） | 10 条断言，全绿+注入变红后该族永久出局 |
| **Phase 2 批量手修** | D01 D03 D04 D07 D08 D14 D15 D16 D17(删) D18(删) D19(删 opacity) | 多为一行~小改，每修带护栏 |
| **设计决策（产品）** | D10 只看切换=重开会话（注入门控下放 vs 明示重连代价） · D11 工具栏常驻行→画布内悬浮 · D12 方案实验室去留/入口（沿用 R10 保留决策） · 剪贴板自动同步 · 快捷键扩充（Ctrl+1..9 切签、剪贴板热键） | 需拍板后动工 |

## 本轮修复建议配额（收敛纪律：全做完才开下一轮评审）

1. **D01 录屏落盘**（P0，丢用户文件）
2. **D02 热键双入口**（P0，误触远端）
3. **D03+D04+D05 假反馈三连**（同族「报喜不报忧」，一起修）
4. **D06 bridge 收口**（demo 黑话→人话，mock 表补 3 条）
5. **D07+D08+D09+D14 焦点/弹层协议包**（抽 useDismissable + 焦点陷阱 + aria-expanded，一并落地门禁）

---

## R11 修复申报（Phase 2 批量修复，逐项验证锚点）

| 项 | 修复 | 验证方法（grep/测试/截图） |
|---|---|---|
| D01 录屏丢文件 | 卸载路径 stop→await blob→saveFileToDownloads→回调页面层复位徽标+toast 告知路径 | `grep 切走标签会停止录制并保存`→0；`grep（丢文件）`→0；`src/session/WindowGroupView.tsx` 录屏注册 effect |
| D02 热键双入口 | isReservedCombo 收编 KeyV/KeyL；删除 Session.tsx 重复 addEventListener；effect 补依赖 `[lockArmedAt]` | `grep 'addEventListener("keydown", onKey)' Session.tsx`=1（原 2）；测试「保留组合（V/L）不下发远端」绿 |
| D03 剪贴板假绿灯 | 空内容抛人话错误（本机/远端两个方向），页面层红色 toast | `src/session/WindowGroupView.tsx` onCopyToRemote/onCopyFromRemote 空串 throw |
| D04 终端 pill 说谎 | TermView 增 onStatus 上报四态，pill 按真实状态换文案+语义色 | `grep 终端会话进行中`→仅存于 open 态 tooltip；截图 05 实证 pill 渲染 |
| D05 侧栏「只看中」撞词 | 徽标改 active?（viewOnly?"只看中":"控制中"）:"已打开"，viewOnly 经 props 透传 | 截图 04 实证激活条目显示「控制中」 |
| D06 绕过 bridge | Devices 三处 + WindowGroupView 两处裸 invoke 全部改 `call()`；mock 表补 set_media_fec/set_host_paused/local_clipboard_*，未知命令抛人话 | `grep '@tauri-apps/api/core' Devices.tsx / WindowGroupView.tsx`→0 |
| D07 对话框焦点 | 文档级 Esc + Tab 循环陷阱 + 关闭还原触发点焦点 | `src/pages/Devices.tsx` addDialogRef effect |
| D08 弹层协议 | 新建终端下拉接入新共用 hook `src/ui/useDismiss.ts`；FloatingToolbar 同步收编 | hook 为唯一出口，两组件接入 |
| D09 aria-hidden 持焦点 | 捕获 textarea 去 aria-hidden、加 aria-label；group 容器 role=application | `grep aria-hidden WindowGroupView.tsx`=1（原 2，剩余为纯装饰 spinner） |
| D14 aria-expanded | moreBtn/helpToggle/termMenu/sidebarToggle/工具栏更多 全部补齐；⟳ 字符换 IconRefresh | 源码逐处可见 aria-expanded |
| D15 嵌套交互 | TabButton 与侧栏条目拍平为容器+两枚同级真 button | 截图 04 渲染正常，嵌套 span[role=button] 已消失 |
| D16 输入框无访问名 | 常用语输入加 aria-label | 源码可见 |
| D17 诊断探针 | 删除 __fc/__fb/__dec + console.log | `grep -rE '__fc|__dec|\[probe\]' src/`→0；console.log 残留=0 |
| D13/D18/D19 行级债 | loopback 琥珀色收敛 color-mix(var(--rd-warn))；6px;;×13→var(--rd-radius-sm)；privacyBadge/fecToggle/scanHeading 重复定义收敛；.version 去 opacity；标题 18px→var(--rd-fs-h1)；工具栏裸过渡→动效令牌 | `grep 6px;;`→0；`ff9830`→0；`r="1.6"` pages→0；Devices.module.css `.version` 无 opacity |

**验证基线**：tsc 0 错；测试 115/115 绿（新增保留组合护栏）；生产构建通过；截图 [round11-fix](round11-fix/)（5 张）目检无布局回归。
**未动（设计决策项，待拍板）**：D10 只看切换重开会话 · D11 工具栏悬浮化 · D12 方案实验室去留 · 快捷键扩充（Ctrl+1..9/剪贴板热键） · 终端标签下工具栏子集（交互#5）。
**门禁债**：本轮护栏以单测+grep 锚点为主；DOM 级断言（焦点回归/aria-expanded 不变量/axe 规则）留待 Phase 1 门禁落地。

---

## R11 设计决策落地申报（用户授权按推荐执行）

| 项 | 决策 | 实现与验证 |
|---|---|---|
| D10 只看切换 | **即时生效**：输入门控下放到发送层逐条判定（pointer/key/wheel 入口 + 键盘 mapper 前置检查），删除 epoch++ 重开；Agent open 参数恒 false（客户端不发输入即等效只看） | 误触恢复代价从两次黑屏重连降为零；工具栏 title 同步「即时生效」 |
| D11 工具栏挤偏 | **保留常驻行**（尊重 R2 评审团「零遮挡」共识），加密/延迟/终端 chip 簇绝对定位右缘（`.chipCluster`），bar 真居中 | 截图 [round11-decisions](round11-decisions/)/04 实证：bar 中心回到画布中线，chips 右对齐 |
| D12 实验室入口 | **保留功能、降级入口**：移出主操作行，落页脚高级区（连接指南旁 linkBtn） | 截图 01 实证：header 只余扫描/添加设备，footer 有「方案实验室」 |
| 快捷键扩充 | Ctrl+Alt+1..9 切会话标签（不抢远端 Ctrl+digit）；Ctrl+Shift+C 取回 / X 发送剪贴板；速查表同步 8 条；保留表同步 KeyC/KeyX + Ctrl+Alt+Digit | InputMapper.test 锁定：Ctrl+1（无 Alt）仍照常下发远端 |
| 交互#5 终端假按钮 | 终端标签下工具栏隐藏只看/截屏/录屏 + 更多菜单隐藏剪贴板两项（`windowActions` prop），只余画质/锁屏/安全键/全屏等设备级操作 | 截图 05 实证：终端态工具栏无窗口专属钮 |

**验证基线**：tsc 0 错；测试 115/115 绿（modifiers 透传测试随保留表扩展同步修正）；构建通过；[round11-decisions](round11-decisions/) 5 张目检无回归。

---

## R11 Phase 1 门禁落地申报（防线先行，已修债务永久出局）

**形态**：与 known-debts「探针仍命中→绿」相反——清单登记**已修**债务（13 条源码锚 + 4 条跨文件不变量），断言探针保持消失（计数语义：D09 aria-hidden 合法残留 ≤1、INV 热键单监听 =1）。谁回潮，`pnpm test` 当场红。

- 清单/扫描器：[src/testing/fixedDebts.ts](../src/testing/fixedDebts.ts)（纯函数扫描器）
- 门禁测试：[src/testing/fixedDebts.test.ts](../src/testing/fixedDebts.test.ts)（实态扫描 + 双向锁定）
- 配套护栏：[src/api/mock.test.ts](../src/api/mock.test.ts)（未知命令人话报错）
- 依赖：@types/node（仅类型）；DOM 级断言（焦点回归/aria-expanded/axe）仍留债——需 jsdom+RTL 基建

**注入活性验证（门禁是真的）**：
1. 单测级：种探针必须红 / 干净必须放行 / 清单指向不存在文件必须红（合成内容 3 断言）
2. 端到端级：向 WindowGroupView.tsx 实际种回 `（丢文件）` → 门禁当场报 `D01: 探针回潮，命中 1 处（上限 0）`；拔除后复绿 5/5

**验证基线**：tsc 0 错；测试 **122/122 绿**（115 + 门禁 5 + mock 护栏 2）。

---
*2026-10-31 · 四视角 agent 独立走查 · 探针 13/13 实证 · 快照 [round11-audit](round11-audit/) / [round11-fix](round11-fix/) / [round11-decisions](round11-decisions/)*
