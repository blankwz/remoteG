# T-006 Agent 端到端 Spike 管线
- 所属 Epic/Story：E5 Agent / US-02
- 负责 Agent：A4 Agent 集成
- 优先级：P0
- 预估：1.5d
- 前置依赖：T-002、T-003、T-004
- 可并行组：第 3 批

## 目标
`rd-agent spike --title <子串> --listen <addr>`：捕获指定窗口 → 编码 → 通过 QUIC 视频流发给首个连接的客户端，打印各阶段耗时。

## 上下文（只读）
- 三个 crate 的 `src/lib.rs` 公开 API（rd-capture-win、rd-encode、rd-transport）

## 修改范围（可写）
- 修改：`apps/rd-agent/Cargo.toml`、`src/main.rs`（仅加 `spike` 子命令分发）、`src/spike.rs`

## 实现步骤
1. `main.rs`：`clap` 子命令 `spike`；初始化 `tracing`。
2. `spike.rs`：按标题子串 `EnumWindows` 找到 HWND；`Server::bind`；等待连接；`open_data(Video)`。
3. 管线：捕获线程 → `crossbeam` 通道 → 编码线程（`create_best`，1080p 起步 8 Mbps）→ tokio 任务发送；首帧强制 IDR；`resized` 时 `reconfigure`。
4. 每 2 秒打印：捕获 fps、平均编码耗时、发送字节率、`conn.rtt()`。
5. 帧 `pts_us` 使用系统时间（`SystemTime` 微秒）以便控制端估算延迟。

## 接口
- CLI：`rd-agent spike --title <String> [--listen 0.0.0.0:21118] [--fps 60] [--kbps 8000]`

## 测试要求
- 手测（与 T-005）：本机回环 + 局域网两台机器。
- 运行：`cargo run -p rd-agent -- spike --title 记事本`

## 验收标准 DoD
- [ ] 控制端可见实时画面，窗口内容变化 < 100 ms 可见（目测）
- [ ] 窗口缩放后画面自动恢复（新 IDR）
- [ ] 日志每 2 秒输出统计
- [ ] 断开客户端后 Agent 回到等待状态不崩溃

## 风险与回滚
- 仅为 Spike 子命令，M1 正式管线由 T-016 重写复用组件；回滚 revert。

## 禁止事项
- 不实现鉴权、窗口列表、输入（属于 M1）。
