# T-047 触控手势翻译 crate 与协议预留（改造项 C）
- 所属 Epic/Story：V0.3 / snug-借鉴与改造计划 改造项 C（P1）
- 负责 Agent：A0（crate）+ 协议侧
- 优先级：P1
- 预估：已实现（2026-11-19）；接线（控制端消费 ContinuousScroll）待虚拟屏一期立项后排期
- 前置依赖：无（协议仅预留 tag，线上兼容）
- 可并行组：与改造项 A/B/D/E 无代码交集
- 性质：**纯逻辑 crate 已完成，不接 UI、不动现有链路**。架构决策遵照改造计划：手势翻译放**控制端**（手机触控指令 → 控制端翻译成鼠标/滚动/关闭），被控端零改动。

## 目标
为「手机当触控板控电脑」（虚拟屏一期 / 移动控制端远期）备好手势翻译核心：四相状态机（pending→hold→pan→fling）+ 滚动物理（惯性衰减），纯逻辑零 I/O，未来任何前端（Windows 控制端 / 移动端）都直接喂事件拿命令。

## 上下文（只读）
- `13_handoff/snug-借鉴与改造计划.md` 改造项 C（任务书原文）
- `crates/rd-touch-translate/`（本卡产出：零依赖 crate + 13 集成测试）
- `proto/rd.proto`：`ContinuousScroll`（tag 51，v0.3 预留）+ `05_api/协议契约.md` §8
- snug 参照：`<snug>/Sources/SnugCore/Input/GestureTranslator.swift`、`ScrollWheel.swift`、`Momentum.swift`（`<snug>` = `C:\Users\blank\claude-learn-done\zephyr4123-snug`，实现为 Rust 重写，常数语义保留）

## 修改范围（已完成）
- 新 crate `crates/rd-touch-translate/`（**零依赖**——`[dependencies]` 空节，纯逻辑可直接进任何 host）：
  - `translator.rs`：TouchTranslator 四相状态机——slop=10pt、holdDuration=0.5s、flingWindow=0.1s、minFlingSpan=0.04s、maxFlingSpeed=4000；输出 TouchCommand{ReplayPress, ReplayCurrent, RightClick, Scroll, CloseWindow}，`TouchResponse{pass_original, commands}` 支持原始事件穿透与命令替换双路；CloseButton 前置优先（is_frontmost）
  - `scrolling.rs`：ScrollWheel 小数位移余量累计（trunc）；Momentum 惯性衰减（decay=0.95@60fps、minimumSpeed=200pt/s、stopSpeed=40pt/s），`advance_frame` 供宿主每帧调用
- `proto/rd.proto`：Envelope oneof 加 `ContinuousScroll continuous_scroll = 51;`（tag 遵照 05_api 分配表，**未接线上链路**，仅预留）；`05_api/协议契约.md` §8 同步

## 验收标准
1. 自动化（已过）：13 集成测试全绿——全迁移路径（tap→hold→pan→fling 相互迁移）、阈值边界（0.5s/10pt 恰好临界）、事件扎堆 5ms 假速度防护、scale=2 阈值放大、小数累计、惯性帧序列、关闭按钮、区域外放行
2. clippy -D warnings 零警告（零依赖 crate 无第三方 API 面）
3. 接线（未来卡，不在本卡）：控制端消费 ContinuousScroll 生成真滚动/惯性——待虚拟屏一期立项

## 风险与备注
- 常数取 snug 实测值（iOS 触控板语义），Windows 触屏/手机端手感可能需调——常数集中在文件头，调参不动逻辑
- `Momentum::advance_frame`（非 Iterator::next）是刻意的命名回避
- 协议预留遵循「附加字段线上兼容，PROTO_VER 不变」规则（同 T-044 一期 proto 结论）
