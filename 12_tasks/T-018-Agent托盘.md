# T-018 Agent 托盘
- 所属 Epic/Story：E5 Agent / US-06
- 负责 Agent：A4 Agent 集成
- 优先级：P1
- 预估：1d
- 前置依赖：T-015
- 可并行组：第 7 批

## 目标
托盘图标提供：生成并显示配对码、已连接设备列表、断开所有、暂停/恢复共享、打开日志目录、退出。

## 上下文（只读）
- 02_product/产品方案.md §3
- `apps/rd-agent/src/session.rs`（`AgentState`、`PeerInfo`）

## 修改范围（可写）
- 修改：`apps/rd-agent/src/tray.rs`
- 新建：`apps/rd-agent/assets/tray.ico`、`apps/rd-agent/assets/tray-active.ico`

## 实现步骤
1. `tray-icon` + Win32 消息循环（`GetMessageW`）在主线程；菜单：`生成配对码`、`已连接：<n>`（子菜单列出设备名，点击断开该设备）、`断开所有`、`暂停共享`（勾选）、`打开日志目录`、`退出`。
2. 生成配对码：`pairing.new_code()`，用 `MessageBoxW`（非模态线程）显示“配对码 123456，5 分钟内有效”及本机指纹前 8 字节。
3. 有连接时切换为 active 图标，tooltip 显示“正在被 <设备名> 控制”。每秒刷新菜单状态。
4. 断开：调用 `PeerInfo.cancel.cancel()`。
5. 退出：取消所有会话后 `std::process::exit(0)`。

## 测试要求
- 手测清单（写入 RUN_LOG）：上述每个菜单项。

## 验收标准 DoD
- [ ] 菜单项全部可用
- [ ] 连接时图标与 tooltip 变化
- [ ] 暂停后控制端画面停止、输入无效；恢复后继续

## 风险与回滚
- 回滚 revert（桩恢复，`--no-tray` 模式不受影响）。

## 禁止事项
- 配对码不写日志；不修改其他文件。
