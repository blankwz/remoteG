# T-047 触控手势翻译 crate 与协议接线（改造项 C）
- 所属 Epic/Story：V0.3 / snug-借鉴与改造计划 改造项 C（P1）
- 负责 Agent：A0（crate + 全链路接线）
- 优先级：P1
- 预估：crate 已实现（2026-11-19）；**接线已于同日提前完成**（原归虚拟屏一期，用户指令提前）
- 前置依赖：无
- 可并行组：与改造项 A/B/D/E 无代码交集
- 性质：**纯逻辑 crate + 全链路接线已完成**。架构决策遵照改造计划：手势翻译放**控制端**（触屏指令 → 控制端翻译成鼠标/滚动），被控端无状态消费；惯性物理永远在发送侧。

## 目标
触屏设备（触屏笔记本/平板模式）上，远程画面支持完整触控板式手势：轻点=点击、拖动=惯性滚动、长按=右键、按住拖动=选择。翻译为纯逻辑 crate（零依赖），协议 `ContinuousScroll`（tag 51）全链路接线。

## 上下文（只读）
- `13_handoff/snug-借鉴与改造计划.md` 改造项 C（任务书原文）
- `crates/rd-touch-translate/`（零依赖 crate，13 集成测试）
- `apps/rd-controller/src-tauri/src/commands/touch.rs`（控制端翻译编排：TouchSession + 惯性驱动任务，6 单测）
- `apps/rd-controller/src/session/WindowGroupView.tsx`（前端 pointerType 路由与 16ms 批量采集）
- `crates/rd-input-win/src/lib.rs::scroll_pixel`（被控端像素滚动注入，1px=1delta）
- `apps/rd-agent/src/input_handler.rs::continuous_scroll`（守卫与路由，与 Wheel 同策略）
- proto/rd.proto：`ContinuousScroll`（tag 51）+ `05_api/协议契约.md` §8（已标注接线）

## 修改范围（已完成）
- **crate**（改造项 C 原交付）：TouchTranslator 四相状态机（slop=10pt、hold=0.5s、flingWindow=0.1s、maxFlingSpeed=4000）+ ScrollWheel 小数累计 + Momentum 惯性衰减（0.95@60fps）；13 集成测试覆盖全迁移路径与阈值边界
- **控制端 Rust**：`commands/touch.rs`——TouchSession（translator 持有 + press 位置记忆 + 命令展开：ReplayPress/ReplayCurrent→MouseBtn、RightClick→右键按下抬起、Scroll→ContinuousScroll；放行路径转基础鼠标保触屏兜底可用）；`touch_gesture` 命令（按 device/window 存状态表，锁内翻译锁外发送）；`spawn_coast` 惯性驱动（60fps interval，translator 状态自愈退出，连接失败自停）
- **控制端前端**：WindowGroupView pointerType==="touch" 路由（与鼠标路径完全隔离）——press 钉住命中窗口（含 ox/oy 偏移、客户区 w/h、scale=远程物理宽/画面 css 宽），move/release 走 16ms 批量 flush（release 立即发）；api/input.ts `touchGesture` 封装
- **被控端**：`input_handler.rs` InputMsg::ContinuousScroll 分支（is_view_only/is_open 守卫与 Wheel 同款）+ session.rs 路由（软失败）；`rd-input-win` `scroll_pixel`（先绝对移动再注入像素 delta，**1px=1delta**——Chrome 观感近似 1:1，系数调参点）
- **协议**：ContinuousScroll 全链路激活（05_api §8 更新）

## 验收标准
1. 自动化（已过）：crate 13 集成测试 + 控制端 TouchSession 6 单测（tap 补发/拖动滚动+惯性起/长按右键/区域外放行/scale 阈值放大/惯性 End 收尾）+ 被控端 lib 全绿 + 前端 tsc/198 测试全绿
2. 手测（待做，需触屏设备）：`09_test/手测清单.md` §5.9（轻点/拖动惯性/长按右键/选择/鼠标不受影响/只看不控）

## 风险与备注
- 常数取 snug 实测值（iOS 触控板语义），Windows 触屏手感可能需调——常数集中在 crate 文件头；**1px=1delta 滚动系数**在 rd-input-win `scroll_pixel` 文档注明调参点
- CloseWindow 手势未接：is_frontmost 核对需要屏上窗口 z 序列表（snug 的 macOS 窗口服务器语义），Windows 控制端无对应物——触屏关窗走普通点击；translator 侧命令保留，控制端防御性忽略
- proto 预留遵循「附加字段线上兼容，PROTO_VER 不变」规则；旧 Agent 收到 ContinuousScroll 按未知 oneof 静默丢弃（前向兼容）
- `Momentum::advance_frame`（非 Iterator::next）是刻意的命名回避
