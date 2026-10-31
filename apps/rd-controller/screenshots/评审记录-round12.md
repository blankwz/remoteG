# Round 12 评审记录（四视角 · 快照 r11-gate/db70dd1 · 目标锚定 95）

> 轨迹：… → 84.3（R10 十评委 GO）→ R11 四视角 81.3 → R11 修复+决策+门禁 → **R12 综合 ≈87**（无障碍 **88** / 排错 **88** / 交互 **87** / 设计 **85**）
> **申报复核：16 项零虚报**（两视角各判 D11 一处「措辞超前」记部分——bar 居中基准是窗口中线非画布中线；D08「唯一出口」口径超前；如实记）。
> 门禁实测：四视角均独立复跑 122/122 绿。
> 快照：[round12-audit](round12-audit/)。

## 评审共识（合并去重后的 R12 配额，5 包全做）

1. **viewOnly 真闭环**（排错#1/#4 + 交互#2/#6）：曾组件 state 切标签静默失效（假锁！）、高亮走 ref 慢一拍、工具栏四条注入通道（锁屏/安全键/常用语/虚拟按键）在「键鼠停发」承诺下照常下发
2. **终端热键黑洞**（交互#1/#5）：xterm 消费全部按键，8 条热键在终端标签全灭且 Ctrl+Shift+C/X 折算 ^C/^X 注入远端 shell；Numpad 漏收编；AltGr 布局劫持
3. **弹层后半段契约**（无障碍#1/2/3/4/5）：关闭后焦点丢 BODY、`role="menu"` 无方向键导航（空承诺）、tablist 无键控、错误 toast 该 alert、四状态条无 live
4. **诚实性小项**（排错#2/3/5/6 + 无障碍#6）：录制保存失败吞原因、Session.tsx 字面 NUL 字节害 grep 判 binary、demo 剪贴板假阴性甩锅用户、REC 语义三处矛盾
5. **设计整块**（设计#1/2/3，P0 桌面地图）：窗口页从进程列表升级桌面地图（WindowDto x/y/w/h 几何零后端）、toolbarRow 居中基准移到画布列、历史连接时间感

暂缓（R13 候选）：字号 codemod（118 处裸 px）、设置页、jsdom+axe 基建、切签保活、断开一键入口。

---

## R12 修复申报（逐项验证锚点）

| 包 | 修复 | 验证 |
|---|---|---|
| viewOnly 闭环 | viewOnly 迁入 SessionStore（`setViewOnly` + 快照字段）随标签持久；Session/工具栏/侧栏高亮全部快照驱动（`sessionCtlRef.current?.viewOnly` 渲染读已清零并进门禁）；锁屏/安全键/常用语/虚拟按键 `disabled` 置灰 + hint；Session 层 `viewOnlyNow()` 总闸兜底；viewOnlyBar 文案明示「含锁屏/安全键/常用语」 | 门禁 `R12-viewOnly-ref` 锚：probe 在 Session.tsx 必须保持 0 命中；tsc+124 测试 |
| 终端热键 | TermView `customKeyEventHandler` 头部 `isReservedCombo` 放行（唯一出口，防再双入口）；InputMapper 收编 `Numpad1-9`；`KeyInput.altGraph` 字段（AltGr 让路），canvas/终端两路 keyInputOf 均透传；Session 切签跳过 AltGr | 门禁 `R12-vkeys-reserved` 锚 TermView 禁散抄；InputMapper.test 锁 Numpad/AltGraph 形态 |
| 弹层契约 | useDismiss 升级：焦点还原（open 瞬间记录触发点，关闭 focus 回去）+ `{menu}`（↑↓/Home/End/打开聚焦首项）+ `{trap}`（Tab 循环）；Devices ⋯ 菜单抽 `DeviceRowMenu` 收编（删手抄 effect，「唯一出口」失实消解）；termMenu 接 menu、工具栏面板接 trap、速查浮层接 trap+dialog 语义+卡内点击不误关；错误 toast `role="alert"`；banner/viewOnlyBar/hostBusyBar/reconnectedFlash 四条 `aria-live="polite"`；tablist ←→ 切签 + tab id ↔ tabpanel aria-labelledby | 源码逐处可见；`grep 'role="alert"' Session.tsx`=1 |
| 诚实性小项 | 录制 auto-stop 回调带 `error` 位 → toast 带原因；REC 语义统一「切换/关闭标签自动停录保存，刷新/关页丢失」（徽标 title+注释+toast 三处）；NUL 字节改转义（`file` 判回 UTF-8 text）+ 门禁 `INV-nul`；demo 剪贴板返演示文本（不再甩锅「你的剪贴板是空的」） | `file src/pages/Session.tsx`→text；门禁 INV-nul |
| 设计整块 | **桌面地图**：`desktopMap()` 纯函数（1920×1080 基准百分比映射+越界裁剪+最小尺寸，2 项单测护栏），默认视图+桌面/列表切换；mock 窗口改错落布局（曾全屏堆叠使地图退化）；toolbarRow 随侧栏 `margin-left:220px`（bar 中心=画布中线）；**历史连接**：`recordConnect/lastConnectedAt/relativeTime`（localStorage 上限 50），设备行「上次连接 · n 分钟前」 | 截图 [round12-fix](round12-fix/)/02 桌面地图实锤；desktopMap 单测 ×2 |

**验证基线**：tsc 0 错；测试 **124/124 绿**（+desktopMap 护栏 2）；构建通过；[round12-fix](round12-fix/) 5 张目检。
**距 95 的已知欠账**（R13 配额候选）：字号令牌 codemod（118 处裸 px）、设置页整块、jsdom+axe DOM 级门禁、切签保活（高频动作黑屏重连）、断开一键入口、会话空态产品化。

---
*2026-10-31 · 四视角独立验收（申报复核 16 项零虚报）· 快照 [round12-audit](round12-audit/) / [round12-fix](round12-fix/)*
