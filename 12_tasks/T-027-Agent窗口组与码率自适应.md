# T-027 Agent 窗口组与码率自适应接入
- 所属 Epic/Story：E5 Agent / US-04、US-02
- 负责 Agent：A4 Agent 集成
- 优先级：P0
- 预估：1.5d
- 前置依赖：T-016、T-011
- 可并行组：第 8 批（T-027 ∥ T-021）

## 目标
打开的窗口自动带上其弹出菜单/对话框（各自独立视频流，按相对偏移由控制端合成）；码率/帧率随网络自适应；按握手结果启用 4:4:4。

## 上下文（只读）
- 03_architecture/架构方案.md §3.3
- 05_api/协议契约.md `WindowEvent`（CHILD_ADDED/CHILD_REMOVED，`root_id`）
- `crates/rd-window-win/src/lib.rs`（`window_group`）、`crates/rd-encode/src/lib.rs`（`RateController`、`negotiate`）

## 修改范围（可写）
- 修改：`apps/rd-agent/src/window_session.rs`

## 实现步骤
1. 每个已打开根窗口：收到 MenuStart/Created 事件或每 250 ms，计算 `window_group(root)`，与上次结果比较：新增成员 → 发 `WindowEvent{CHILD_ADDED, info(x,y 为相对根客户区偏移), root_id}` 并为其开管线（不做 Fit，不精修菜单类 `#32768`）；消失成员 → `CHILD_REMOVED` 并停管线；偏移变化 → `MOVED`。
2. 输入：`is_open` 对子窗口 id 也返回 true（T-017 的点击即可作用于菜单）。
3. 码率：每会话一个 `RateController`；每 200 ms 用 `conn.rtt()`、发送队列积压（记录 `send_video` 等待时间/未发送字节）更新，结果应用到该会话所有编码器（按窗口面积比例分配）。
4. 4:4:4：`negotiate(probe_encode_caps(), hello.decode_caps, hello.supports_444)`；结果为 `H264_444` 时编码器以 4:4:4 配置创建，`OpenWindowAck.codec` 反映实际值。
5. HUD 支持：每 1 s 发 `StatsReport`（fps、rtt、kbps）。

## 测试要求
- 新增 `#[cfg(test)]`：组成员 diff 逻辑（新增/移除/移动）单测。
- 手测：记事本右键菜单、“另存为”对话框出现在控制端；clumsy 注入 5% 丢包/200 ms 延迟时码率下降并恢复。
- 运行：`cargo test -p rd-agent`

## 验收标准 DoD
- [ ] 右键菜单在控制端 300 ms 内出现且可点击
- [ ] 模态对话框可见可操作，关闭后消失
- [ ] 弱网下不出现持续积压（延迟不持续增长）
- [ ] diff 单测通过

## 风险与回滚
- 某些程序菜单不属于同 PID（如 Shell 扩展）：记录为已知限制。回滚 revert 到 T-016 版本。

## 禁止事项
- 不修改其他文件；不修改协议。
