# T-015 Agent 核心：配置、监听、鉴权与消息分发
- 所属 Epic/Story：E5 Agent / US-06
- 负责 Agent：A4 Agent 集成
- 优先级：P0
- 预估：1.5d
- 前置依赖：T-012、T-013、T-014
- 可并行组：第 6 批

## 目标
`rd-agent run` 可在 Tailscale 地址上监听，完成 Hello/鉴权/配对，把已鉴权消息分发给各 handler（handler 为桩，由 T-016/T-017/T-018 填充）。**本任务一次性补齐 rd-agent 的全部依赖与共享结构，后续任务不再改 Cargo.toml / main.rs / session.rs。**

## 上下文（只读）
- 05_api/协议契约.md 全文
- 11_security/安全方案.md
- rd-transport、rd-security、rd-term、rd-input-win、rd-window-win、rd-encode、rd-capture-win 的 `lib.rs`

## 修改范围（可写）
- 修改：`apps/rd-agent/Cargo.toml`、`src/main.rs`、`src/config.rs`、`src/server.rs`、`src/session.rs`
- 修改（仅写桩与类型定义）：`src/window_session.rs`、`src/input_handler.rs`、`src/term_handler.rs`、`src/tray.rs`
- 新建：`apps/rd-agent/build.rs` + `apps/rd-agent/rd-agent.manifest`（PerMonitorV2 DPI 感知，`embed-resource` crate）

## 实现步骤
1. Cargo.toml：依赖全部内部 crate + `tokio`、`clap`、`tracing`、`tracing-appender`、`if-addrs`、`tray-icon`、`serde`、`toml`、`directories`。
2. `config.rs`：`%LOCALAPPDATA%\rd\agent.toml`：`listen_port = 21118`、`listen_addrs = []`（空=自动）、`allow_lan = false`、`terminal_enabled = true`、`refine_enabled = true`、`max_fps = 60`、`start_kbps = 8000`。
3. `server.rs`：自动模式下用 `if-addrs` 取 `100.64.0.0/10` 范围的地址；`allow_lan = true` 时额外绑定私网地址；都没有则报错退出并提示安装 Tailscale；启动日志打印监听地址与本机指纹前 8 字节。
4. `session.rs`：每连接一个任务：`accept_control` → 首条必须 `Hello`（5 s 超时）→ 若 `TrustStore::is_trusted(peer)` 则回 `HelloAck` 进入 Authenticated；否则进入 Pairing 状态，只接受 `PairRequest`，其他消息回 `Error{code:1}`；配对成功写入信任列表后进入 Authenticated。
5. 分发：`ListWindows/OpenWindow/CloseWindow/ViewportInfo` → `window_session::handle`；`KeyEvent/MouseButton/MouseWheel/TextInput` → `input_handler::handle`；datagram 循环 → `input_handler::on_datagram`；`OpenTerminal/TerminalResize` → `term_handler::handle`；断开时依次调用 `window_session::on_disconnect`、`input_handler::on_disconnect`、`term_handler::on_disconnect`。
6. 共享结构（定义在 session.rs）：
```rust
pub struct AgentState {
    pub config: Config, pub identity: Identity, pub local_fp: Fingerprint,
    pub trust: Mutex<TrustStore>, pub pairing: Mutex<AgentPairing>,
    pub terms: TermManager, pub injector: Mutex<Injector>,
    pub paused: AtomicBool,
    pub peers: Mutex<HashMap<u64 /*conn id*/, PeerInfo>>, // 托盘显示 & 断开
}
pub struct PeerInfo { pub name: String, pub fp: Fingerprint, pub since: SystemTime, pub cancel: CancellationToken }
pub struct SessionCtx {
    pub state: Arc<AgentState>, pub conn: Arc<Conn>, pub ctl: Mutex<ControlTx>, pub peer_name: String,
    pub hello: Hello,                                      // 含 decode_caps
    pub windows: window_session::WindowSessions,           // T-016 实现
    pub terms: term_handler::TermAttachments,              // T-017 实现
}
impl SessionCtx { pub async fn send(&self, msg: envelope::Msg, req_id: u32) -> anyhow::Result<()>; }
```
7. 桩签名（handler 返回 `anyhow::Result<()>`）：
   - `window_session::{WindowSessions(default), handle(ctx, req_id, msg), on_disconnect(ctx)}`，`WindowSessions::is_open(&self, id: u64) -> bool` 桩返回 false。
   - `input_handler::{handle(ctx, msg), on_datagram(ctx, bytes), on_disconnect(ctx)}`
   - `term_handler::{TermAttachments(default), handle(ctx, req_id, msg), on_disconnect(ctx)}`
   - `tray::run(state: Arc<AgentState>) -> !` 桩：阻塞等待 Ctrl+C。
8. `main.rs`：子命令 `run [--config-dir <dir>] [--listen <addr>] [--no-tray] [--print-port]`；debug 构建额外支持 `--test-pair-code <6位>`（`#[cfg(debug_assertions)]`，供 T-025）；tokio 运行在后台线程，主线程跑 `tray::run`（`--no-tray` 时主线程等待 Ctrl+C）；保留 `spike` 子命令。
9. 日志：`tracing-appender` 按天滚动到 `%LOCALAPPDATA%\rd\logs\`，保留 7 天；禁止记录输入与终端数据。

## 测试要求
- 新增：`apps/rd-agent/tests/auth.rs`：启动二进制（`env!("CARGO_BIN_EXE_rd-agent")`，`--no-tray --listen 127.0.0.1:0 --print-port --test-pair-code 123456`）；未配对客户端发 `ListWindows` 收到 `Error{code:1}`；配对成功后重连直接 `HelloAck`。
- 运行：`cargo test -p rd-agent --test auth`

## 验收标准 DoD
- [ ] 无 Tailscale 地址时明确报错退出
- [ ] 鉴权测试通过
- [ ] 后续任务所需依赖、共享结构、桩签名全部就位（T-016/017/018 无需修改本任务文件）
- [ ] 日志滚动与目录正确

## 风险与回滚
- 托盘与 tokio 线程模型：主线程消息循环由 T-018 实现，本任务桩不依赖。回滚 revert。

## 禁止事项
- 不实现窗口/输入/终端/托盘具体逻辑；不改 crates/。
