# T-014 ConPTY 终端会话
- 所属 Epic/Story：E8 终端 / US-05
- 负责 Agent：A3 Windows 系统层
- 优先级：P0
- 预估：2d
- 前置依赖：T-007
- 可并行组：第 5 批

## 目标
`rd-term` 能启动 shell（pwsh/cmd/wsl）为伪终端会话，提供 VT 输出流、输入写入、resize，以及断线后 10 分钟内凭 `term_id` 重连并回放 1 MB 缓冲。

## 上下文（只读）
- 03_architecture/架构方案.md §3.5
- 05_api/协议契约.md `OpenTerminal`、`TerminalOpened`、`TerminalResize`

## 修改范围（可写）
- 修改：`crates/rd-term/Cargo.toml`、`src/lib.rs`、`src/conpty.rs`、`src/session.rs`、`src/ring.rs`
- 新建：`crates/rd-term/tests/session.rs`、`crates/rd-term/tests/ring.rs`

## 实现步骤
1. `conpty.rs`：使用 `portable-pty` crate（Windows 下即 ConPTY，macOS/Linux 下 openpty，便于跨平台测试）；`spawn(shell, cols, rows, env: TERM=xterm-256color)`。
2. `ring.rs`：固定 1 MB 环形缓冲，`push(&[u8])`、`snapshot() -> Vec<u8>`；截断时从下一个 `\n` 或 ESC 序列边界开始，避免回放出半截转义序列。
3. `session.rs`：`TermManager` 持有 `HashMap<term_id, TermSession>`；`term_id` = 128-bit 随机 hex；每个会话一个读线程把输出写入 ring 并广播（`tokio::sync::broadcast`，容量 256）；无订阅者时开始计时，10 分钟后杀进程并移除；进程退出时广播 `Exit(code)`。
4. `attach(term_id)` 返回 `(snapshot, Receiver)`，保证 snapshot 与后续流不丢不重（加锁同时取快照与订阅）。
5. shell 白名单：`pwsh.exe`、`powershell.exe`、`cmd.exe`、`wsl.exe`（Windows）；`$SHELL`（其他平台）；不在白名单返回错误码 30。

## 接口
```rust
pub enum TermOutput { Data(bytes::Bytes), Exit(i32) }
pub struct TermManager; impl TermManager {
    pub fn new(idle_timeout: Duration) -> Self;
    pub fn open(&self, shell: &str, cols: u16, rows: u16) -> anyhow::Result<String /*term_id*/>;
    pub fn attach(&self, term_id: &str) -> anyhow::Result<(Vec<u8>, tokio::sync::broadcast::Receiver<TermOutput>)>;
    pub fn write(&self, term_id: &str, data: &[u8]) -> anyhow::Result<()>;
    pub fn resize(&self, term_id: &str, cols: u16, rows: u16) -> anyhow::Result<()>;
    pub fn close(&self, term_id: &str);
}
```

## 测试要求
- `tests/session.rs`：打开 shell → 写 `echo rd_ok\r\n` → 输出含 `rd_ok`；resize 不报错；detach 后 attach 快照包含历史；超时（测试设 1 s）后会话被清理；非白名单 shell 报错。
- `tests/ring.rs`：溢出截断边界。
- 运行：`cargo test -p rd-term`

## 验收标准 DoD
- [ ] 测试在 Windows 与 macOS 上通过
- [ ] Windows 上 pwsh 中运行 `vim`/`htop`（WSL）等全屏程序输出完整 VT 序列（配合 T-023 验证）
- [ ] 无订阅时 CPU 占用不增长（读线程阻塞读）

## 风险与回滚
- ConPTY 对部分序列重写（会重绘整屏）：可接受。回滚 revert。

## 禁止事项
- 不接管已存在的终端窗口（D-005）；不改其他 crate。
