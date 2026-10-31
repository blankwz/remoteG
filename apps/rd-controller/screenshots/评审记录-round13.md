# Round 13 修复申报（R12 配额清偿 + 终审前快照）

> 轨迹：R12 综合 ≈87（88/88/87/85）→ 本轮清偿 R12 五官全部配额 → **待终审**。
> 验证基线：tsc 0 错；测试 **131/131 绿**（124 + useDismiss 协议 3 + axe 页面扫描 2 + axe 活性 1 + D07 焦点 1）；截图 [round13-fix](round13-fix/)。

## R12 配额逐包申报

| 包 | 修复 | 验证锚点 |
|---|---|---|
| 字号令牌 codemod（设计#5） | 122 处裸 px 全量入令牌（新增 --rd-fs-md:14px 补全阶梯，1:1 保视觉）；5 枚令牌全部激活 | 门禁 INV-fs1/INV-fs2：`font-size: 1/2` 全 src 0 命中，新裸 px 即红 |
| 终端热键黑洞（交互#1） | （R12 已修，本轮无新增） | 门禁 R12-vkeys-reserved |
| 弹层契约（无障碍） | （R12 已修 useDismiss 升级）本轮补 **DOM 门禁**：useDismiss 焦点还原/menu 键控 3 断言（jsdom+RTL） | domGates.test.tsx |
| 诚实性小项 | （R12 已修 NUL/error 位/demo 剪贴板） | 门禁 INV-nul |
| jsdom 基建（无障碍#3 基建债） | **axe-core 落地**：Devices/Settings 真实渲染树扫描 aria-hidden-focus/nested-interactive 零违规；活性验证（种入必红，incomplete 按保守语义同罪）；axeScan 唯一出口 | domGates.test.tsx ×3 |
| 桌面地图（设计#1） | （R12 已修） | desktopMap 单测 ×2 |

## R13 新增（R12 暂缓项清偿）

| 项 | 修复 | 验证锚点 |
|---|---|---|
| 切签保活（交互#3 P1） | 窗口标签全部挂载、非激活 `display:none`——切签不再 close/open 会话付全价黑屏重连；WindowGroupView 增 `active` 门控：ref 注册（导出/录屏/会话控制）与 Fit 视口上报只属于激活实例（防 1×1 上报与控制器指错标签）、容器 0 尺寸不采样、捕获 textarea 仅激活自动聚焦 | 源码 active 守卫 5 处；tsc |
| 断开一键入口（交互#4） | chip 簇首位「断开」+ 空态第三出口；断开→toast→回窗口列表 | 截图 04 实证 chip 簇「断开」 |
| 空态产品化（设计#6 前半） | 空态显设备名+地址上下文 + 回窗口列表/断开双出口 | 截图 03 |
| 设置页整块（设计#4） | `#/settings` + 导航齿轮入口：新连接默认画质 / Cmd→Ctrl（与 HUD 同一偏好）/ 弱网纠错 FEC / 终端默认字号 stepper / 自定义常用语删除——全接现有状态零新后端 | 截图 01 导航「⚙ 设置」；domGates axe 扫过 Settings 页 |

**R12 申报兑现率**：五官配额全部清偿（2 项本轮、3 项 R12 已修），无虚报。
**残留已知债**（终审判定用）：切签保活仅窗口标签（终端标签仍按需挂载，resume 机制兜底）；axe 仅覆盖 Devices/Settings 两页（Session/Windows 渲染需重型 IPC 桩，记基建债）；对比度自动算未建。

## 终审现场发现并修复的真 bug

- **D07 焦点还原在生产路径失效**：触发点在 effect（post-commit）里取 `document.activeElement`，早被对话框 `autoFocus` 输入框抢走——关闭时还原的是已卸载元素，焦点照样丢 BODY。修法：`openAddDialog()` 在 setState **前**捕获触发点（`addTriggerRef`），5 个入口统一走它。**该 bug 由 DOM 门禁测试逼出**——「测试测真用户路径」的价值实证。

---
*2026-10-31 · R13 快照 [round13-fix](round13-fix/) · 131/131 绿*
