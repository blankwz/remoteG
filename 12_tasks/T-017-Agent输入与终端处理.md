# T-017 Agent 输入与终端处理
- 所属 Epic/Story：E5 Agent / US-02、US-05
- 负责 Agent：A4 Agent 集成
- 优先级：P0
- 预估：1d
- 前置依赖：T-015
- 可并行组：第 7 批

## 目标
把控制流/datagram 中的输入事件注入目标窗口；处理终端打开/恢复/resize 并通过双向流转发 VT 数据。

## 上下文（只读）
- 05_api/协议契约.md（输入、终端消息，§3 datagram 格式）
- `apps/rd-agent/src/session.rs`
- `crates/rd-input-win/src/lib.rs`、`crates/rd-term/src/lib.rs`

## 修改范围（可写）
- 修改：`apps/rd-agent/src/input_handler.rs`、`apps/rd-agent/src/term_handler.rs`

## 实现步骤
1. `input_handler::handle`：`MouseButton`/`MouseWheel` 先检查 `ctx.windows.is_open(window_id)`（否则丢弃并 warn），按下前 `ensure_foreground`；`KeyEvent`/`TextInput` 作用于当前前台窗口，但仅当前台窗口属于本会话已打开窗口（防止误操作其他程序）。`state.paused` 时全部丢弃。
2. `on_datagram`：解析 `type=1` 鼠标移动；按 `seq` 丢弃乱序旧包。
3. `on_disconnect`：`injector.release_all()`。
4. `term_handler::handle(OpenTerminal)`：`config.terminal_enabled=false` → Error 30；`resume_id` 非空 → `attach`，否则 `open` 后 `attach`；`open_bidi(Terminal)` → 先写快照，再持续转发广播输出；读对端输入 → `terms.write`；回 `TerminalOpened{term_id, stream_id}`。
5. `TerminalResize` → `terms.resize`；`TermOutput::Exit` 时关闭流。
6. `on_disconnect`：只 detach（会话由 rd-term 超时回收）。

## 测试要求
- 由 T-025 e2e 覆盖（终端回显、点击计数）。本任务新增 `#[cfg(test)]` datagram 解析单测。
- 运行：`cargo test -p rd-agent`

## 验收标准 DoD
- [ ] 未打开的窗口不接受鼠标注入
- [ ] 断开后无卡键
- [ ] 终端断线重连（同 `resume_id`）可看到之前输出
- [ ] 日志中无按键与终端内容

## 风险与回滚
- 回滚 revert（桩恢复）。

## 禁止事项
- 不修改 window_session.rs / session.rs。
