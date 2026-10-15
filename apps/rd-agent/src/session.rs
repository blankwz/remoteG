//! 会话:每连接的状态机(Hello → 信任/配对 → 消息分发)与共享结构(T-015)。
//!
//! 状态机(任务卡步骤 4):
//! 1. `accept_control` → 首条必须是 `Hello`(5 s 超时);
//! 2. 对端指纹在信任列表 → 回 `HelloAck`,进入 Authenticated;
//! 3. 否则进入 Pairing:只接受 `PairRequest`,其他消息一律回 `Error{code:1}`;
//!    配对成功(见下)写入信任列表后进入 Authenticated。
//!
//! 配对接线(T-013 语义,与 T-019 控制端约定一致):
//! - 4 步协议由 rd-security 的 `AgentPairing::on_message` 状态机驱动,消息经
//!   `PairRequest{spake2_msg}` / `PairResult{spake2_msg, ok, reason}` 承载:
//!   1) 控制端发第 1 轮 SPAKE2 → Agent 回 `PairResult{ok:true, spake2_msg:第2轮}`;
//!   2) 控制端发 `MAC_c` → Agent 校验通过后回 `PairResult{ok:true, spake2_msg:MAC_a}`;
//!   3) 失败回 `PairResult{ok:false, reason}`(reason 不含配对码)。
//! - **Agent 判定配对成功的时机**:`on_peer_mac` 校验通过、即将返回 `Send(MAC_a)`
//!   的那一步——此时码已被一次性作废。实现上以 rd-security 内层信封 tag 判定
//!   (tag `0x02` = HMAC 确认标签,该常量未跨 crate 导出,见 [`TAG_MAC`])。
//! - 控制端收到 `MAC_a` 校验 `Done` 后配对完全成立;Agent 侧在发出 `MAC_a`
//!   前就完成信任列表写入,因此控制端断开重连即直接 `HelloAck`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use rd_proto::v0::{envelope, Envelope, Error, Hello, HelloAck, PairResult};
use rd_security::{AgentPairing, PairError, PairStep};
use rd_term::TermManager;
use rd_transport::{Conn, ControlRx, ControlTx, Fingerprint, Identity};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::input_handler::{self, Injector, InputMsg};
use crate::term_handler::{self, TermAttachments, TermMsg};
use crate::window_session::{self, WindowMsg, WindowSessions};

/// 等待首条 `Hello` 的超时。
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// 协议版本(rd.v0)。
const PROTO_VER: u32 = 0;
/// rd-security 配对内层信封的 HMAC 确认标签(pairing.rs 未导出;`[ver=1][tag][payload]`)。
const TAG_MAC: u8 = 0x02;
/// 协议错误码 1:未鉴权(05_api §4)。
const ERR_UNAUTHENTICATED: u32 = 1;
/// 协议错误码 30:终端启动失败(05_api §4;终端功能被关闭时沿用)。
const ERR_TERMINAL_DISABLED: u32 = 30;
/// Agent 侧编出的默认编码能力(硬编探测在 T-016 接入后细化)。
const ENCODE_CAPS: [i32; 1] = [rd_proto::v0::Codec::H264420 as i32];

// ---------------------------------------------------------------------------
// 共享结构(任务卡 §6;T-016/017/018 直接使用,不再改动本文件的结构定义)
// ---------------------------------------------------------------------------

/// Agent 全局状态:配置、身份、信任列表、配对状态机、终端与输入等共享资源。
///
/// 注:部分字段在 handler 桩填充(T-016/017/018)前尚无读者,
/// 故对字段级 `dead_code` 显式豁免。
#[allow(dead_code)]
pub struct AgentState {
    pub config: Config,
    pub identity: Identity,
    pub local_fp: Fingerprint,
    pub trust: Mutex<rd_security::TrustStore>,
    pub pairing: Mutex<AgentPairing>,
    pub terms: TermManager,
    pub injector: Mutex<Injector>,
    pub paused: AtomicBool,
    /// 连接 id → 在线对端(托盘显示与断开清理用)。
    pub peers: Mutex<HashMap<u64, PeerInfo>>,
}

/// 一台在线控制端(name/fp/since 由 T-018 托盘消费;cancel 为本连接取消令牌)。
#[derive(Debug)]
#[allow(dead_code)]
pub struct PeerInfo {
    pub name: String,
    pub fp: Fingerprint,
    pub since: SystemTime,
    /// 该连接的取消令牌:T-016/017 的流任务应 `select` 它,托盘"断开"亦用它。
    pub cancel: CancellationToken,
}

/// 单连接上下文:分发给各 handler 的只读视图 + 控制流发送端。
pub struct SessionCtx {
    pub state: Arc<AgentState>,
    pub conn: Arc<Conn>,
    /// 控制流发送端。用 tokio Mutex:`send` 需要跨 await 持锁(分发循环、
    /// datagram 任务、T-016/017 的流任务都可能并发回包,std 互斥无法做到)。
    pub ctl: tokio::sync::Mutex<ControlTx>,
    pub peer_name: String,
    /// 对端 `Hello`(含 decode_caps,供 T-016 编码选路)。
    #[allow(dead_code)]
    pub hello: Hello,
    /// 窗口子会话(T-016 实现;桩阶段无读者)。
    #[allow(dead_code)]
    pub windows: WindowSessions,
    /// 终端挂接(T-017 实现;桩阶段无读者)。
    #[allow(dead_code)]
    pub terms: TermAttachments,
}

impl SessionCtx {
    /// 在控制流上回一条消息(`req_id` 沿用请求的信封)。
    pub async fn send(&self, msg: envelope::Msg, req_id: u32) -> anyhow::Result<()> {
        let env = Envelope {
            req_id,
            msg: Some(msg),
        };
        self.ctl.lock().await.send(&env).await
    }
}

// ---------------------------------------------------------------------------
// 连接入口
// ---------------------------------------------------------------------------

/// 一个连接的完整生命周期:握手 → (配对) → 分发 → 断开清理。
pub async fn handle_conn(state: Arc<AgentState>, conn: Conn) -> anyhow::Result<()> {
    let (mut ctl_tx, mut ctl_rx) = conn
        .accept_control()
        .await
        .context("接受控制流失败(对端未打开 stream 0)")?;

    let (hello, hello_req_id) = recv_hello(&mut ctl_rx).await?;
    let peer_fp = conn.peer_fingerprint();
    let local_fp = state.local_fp;

    let trusted = {
        let trust = lock_trust(&state);
        trust.is_trusted(&peer_fp)
    };

    let authenticated = if trusted {
        tracing::info!(peer = peer_fp.short_hex(), name = %hello.device_name, "受信任设备接入");
        {
            if let Err(err) = lock_trust(&state).touch(&peer_fp) {
                tracing::warn!("刷新 last_seen 失败: {err:#}");
            }
        }
        // 沿用 Hello 的 req_id:控制端以 RPC(等响应)方式握手
        ctl_tx.send(&hello_ack(hello_req_id)).await?;
        true
    } else {
        // 未信任:先对 Hello 回 Error{1}(05_api §1 未鉴权;控制端以此触发配对流程),
        // 再进入配对循环等待 PairRequest。不回包会让控制端握手 RPC 空等到超时。
        ctl_tx
            .send(&error_env(
                hello_req_id,
                ERR_UNAUTHENTICATED,
                "未鉴权:请先完成配对",
            ))
            .await?;
        run_pairing(&state, &mut ctl_tx, &mut ctl_rx, &hello, local_fp, peer_fp).await?
    };
    if !authenticated {
        return Ok(());
    }

    // --- Authenticated:构造上下文、注册对端、启动 datagram 循环、分发 ---
    // 连接序号:Agent 进程内自增(rd-transport 未暴露 quinn stable_id;
    // 仅用于本进程内 peers 表/托盘展示,唯一性在进程内成立即可)
    static CONN_SEQ: AtomicU64 = AtomicU64::new(1);
    let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    let cancel = CancellationToken::new();
    {
        state.peers.lock().expect("peers 锁中毒").insert(
            conn_id,
            PeerInfo {
                name: hello.device_name.clone(),
                fp: peer_fp,
                since: SystemTime::now(),
                cancel: cancel.clone(),
            },
        );
    }

    let ctx = Arc::new(SessionCtx {
        state: Arc::clone(&state),
        conn: Arc::new(conn),
        ctl: tokio::sync::Mutex::new(ctl_tx),
        peer_name: hello.device_name.clone(),
        hello,
        windows: WindowSessions,
        terms: TermAttachments,
    });

    spawn_datagram_loop(&ctx);

    tracing::info!(peer = peer_fp.short_hex(), name = %ctx.peer_name, "会话进入已鉴权状态");
    let dispatch_result = dispatch_loop(&ctx, &mut ctl_rx).await;

    // --- 断开清理(任务卡步骤 5)---
    window_session::on_disconnect(&ctx);
    input_handler::on_disconnect(&ctx);
    term_handler::on_disconnect(&ctx);
    if let Some(peer) = state.peers.lock().expect("peers 锁中毒").remove(&conn_id) {
        peer.cancel.cancel();
    }
    tracing::info!(peer = peer_fp.short_hex(), "连接断开");

    dispatch_result
}

/// 读取首条消息,必须是 `Hello`(5 s 超时);返回消息与信封 `req_id`
/// (响应需沿用同一 `req_id`,控制端以 RPC 等响应方式握手)。
async fn recv_hello(rx: &mut ControlRx) -> anyhow::Result<(Hello, u32)> {
    let inner = tokio::time::timeout(HELLO_TIMEOUT, rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("等待 Hello 超时({}s)", HELLO_TIMEOUT.as_secs()))?;
    let env = inner?.ok_or_else(|| anyhow::anyhow!("对端在发送 Hello 前断开"))?;
    match env.msg {
        Some(envelope::Msg::Hello(hello)) => Ok((hello, env.req_id)),
        Some(other) => anyhow::bail!("首条消息必须是 Hello,实际 {other:?}"),
        None => anyhow::bail!("收到空信封(无消息体)"),
    }
}

/// Pairing 状态循环:只接受 `PairRequest`,成功时回 `HelloAck` 并转 Authenticated。
/// 返回是否进入已鉴权状态。
async fn run_pairing(
    state: &Arc<AgentState>,
    ctl_tx: &mut ControlTx,
    ctl_rx: &mut ControlRx,
    hello: &Hello,
    local_fp: Fingerprint,
    peer_fp: Fingerprint,
) -> anyhow::Result<bool> {
    tracing::info!(peer = peer_fp.short_hex(), "未受信任设备,进入配对流程");
    loop {
        let env = match ctl_rx.recv().await? {
            Some(env) => env,
            None => return Ok(false),
        };
        match env.msg {
            Some(envelope::Msg::PairReq(req)) => {
                // 配对状态机为同步调用:锁只在临界区内持有,不跨 await。
                let step = {
                    let mut pairing = state.pairing.lock().expect("pairing 锁中毒");
                    pairing.on_message(&req.spake2_msg, &local_fp, &peer_fp)
                };
                match step {
                    PairStep::Send(out) => {
                        // 内层信封 [ver][tag][payload]:tag 0x02(MAC 确认)
                        // = Agent 已校验对端 MAC、配对成功。
                        // 两条出向消息都回 ok:true + spake2_msg 承载(见模块注释)。
                        let paired = out.get(1) == Some(&TAG_MAC);
                        // 发送结果忽略:对端收到 MAC 后可能立即断开重连,
                        // 连接关闭不能阻止下面信任列表的落盘
                        let _ = ctl_tx
                            .send(&Envelope {
                                req_id: env.req_id,
                                msg: Some(envelope::Msg::PairRes(PairResult {
                                    ok: true,
                                    spake2_msg: out,
                                    reason: String::new(),
                                })),
                            })
                            .await;
                        if paired {
                            finish_pairing(state, ctl_tx, hello, peer_fp).await?;
                            return Ok(true);
                        }
                    }
                    // Agent 侧状态机不产生 Done(仅控制端返回);防御性按成功处理。
                    PairStep::Done => {
                        finish_pairing(state, ctl_tx, hello, peer_fp).await?;
                        return Ok(true);
                    }
                    PairStep::Failed(err) => {
                        // reason 不含配对码与任何密钥材料
                        tracing::warn!(peer = peer_fp.short_hex(), "配对失败: {err:?}");
                        ctl_tx
                            .send(&Envelope {
                                req_id: env.req_id,
                                msg: Some(envelope::Msg::PairRes(PairResult {
                                    ok: false,
                                    spake2_msg: Vec::new(),
                                    reason: pair_error_reason(&err),
                                })),
                            })
                            .await?;
                    }
                }
            }
            Some(other) => {
                // 未鉴权一律拒绝(05_api §1:未信任时只允许 PairRequest)
                ctl_tx
                    .send(&error_env(
                        env.req_id,
                        ERR_UNAUTHENTICATED,
                        "未鉴权:请先完成配对",
                    ))
                    .await?;
                tracing::debug!(peer = peer_fp.short_hex(), "配对阶段拒绝消息 {other:?}");
            }
            None => continue,
        }
    }
}

/// 配对成功收尾:写入信任列表并回 `HelloAck`。
async fn finish_pairing(
    state: &Arc<AgentState>,
    ctl_tx: &mut ControlTx,
    hello: &Hello,
    peer_fp: Fingerprint,
) -> anyhow::Result<()> {
    {
        lock_trust(state)
            .add(peer_fp, &hello.device_name)
            .context("配对成功但写入信任列表失败")?;
    }
    tracing::info!(peer = peer_fp.short_hex(), name = %hello.device_name, "配对成功,已加入信任列表");
    // HelloAck 尽力发送:对端配对成功后可能立即断开重连
    // 配对成功后的主动推送(对端可能已断开重连),无对应请求,req_id=0
    let _ = ctl_tx.send(&hello_ack(0)).await;
    Ok(())
}

/// 已鉴权状态的消息分发循环(任务卡步骤 5)。
async fn dispatch_loop(ctx: &SessionCtx, rx: &mut ControlRx) -> anyhow::Result<()> {
    while let Some(env) = rx.recv().await? {
        let Some(msg) = env.msg else { continue };
        match msg {
            // 窗口 → T-016
            envelope::Msg::ListWindows(m) => handle_soft(
                &ctx.peer_name,
                window_session::handle(ctx, env.req_id, WindowMsg::List(m)),
            ),
            envelope::Msg::OpenWindow(m) => handle_soft(
                &ctx.peer_name,
                window_session::handle(ctx, env.req_id, WindowMsg::Open(m)),
            ),
            envelope::Msg::CloseWindow(m) => handle_soft(
                &ctx.peer_name,
                window_session::handle(ctx, env.req_id, WindowMsg::Close(m)),
            ),
            envelope::Msg::Viewport(m) => handle_soft(
                &ctx.peer_name,
                window_session::handle(ctx, env.req_id, WindowMsg::Viewport(m)),
            ),
            // 输入 → T-017(无需回包)
            envelope::Msg::Key(m) => {
                handle_soft(&ctx.peer_name, input_handler::handle(ctx, InputMsg::Key(m)))
            }
            envelope::Msg::MouseBtn(m) => handle_soft(
                &ctx.peer_name,
                input_handler::handle(ctx, InputMsg::MouseButton(m)),
            ),
            envelope::Msg::Wheel(m) => handle_soft(
                &ctx.peer_name,
                input_handler::handle(ctx, InputMsg::MouseWheel(m)),
            ),
            envelope::Msg::Text(m) => handle_soft(
                &ctx.peer_name,
                input_handler::handle(ctx, InputMsg::TextInput(m)),
            ),
            // 终端 → T-017(总开关在分发处统一拦截)
            envelope::Msg::OpenTerm(m) => {
                if ctx.state.config.terminal_enabled {
                    handle_soft(
                        &ctx.peer_name,
                        term_handler::handle(ctx, env.req_id, TermMsg::Open(m)),
                    )
                } else {
                    ctx.send(
                        envelope::Msg::Error(error_msg(ERR_TERMINAL_DISABLED, "终端功能已关闭")),
                        env.req_id,
                    )
                    .await?;
                }
            }
            envelope::Msg::TermResize(m) => handle_soft(
                &ctx.peer_name,
                term_handler::handle(ctx, env.req_id, TermMsg::Resize(m)),
            ),
            // 其余消息:静默/记录,不打断会话
            envelope::Msg::Stats(m) => {
                tracing::debug!(name = %ctx.peer_name, fps = m.fps, rtt_ms = m.rtt_ms, kbps = m.kbps, "控制端统计(码率自适应 T-016 接入)");
            }
            envelope::Msg::Error(m) => {
                tracing::debug!(name = %ctx.peer_name, code = m.code, "控制端报告错误: {}", m.message);
            }
            envelope::Msg::Hello(_) | envelope::Msg::PairReq(_) => {
                tracing::debug!(name = %ctx.peer_name, "已鉴权状态下忽略重复 Hello/PairRequest");
            }
            // Agent→控制端方向的消息不应出现
            other => {
                tracing::debug!(name = %ctx.peer_name, "忽略反向/未知消息 {other:?}");
            }
        }
    }
    Ok(())
}

/// handler 错误不致命:记录告警并继续服务连接(业务错误应由 handler 自行回
/// `Error` 消息;返回 Err 保留给协议级致命场景)。
fn handle_soft(peer: &str, result: anyhow::Result<()>) {
    if let Err(err) = result {
        tracing::warn!(name = peer, "消息处理失败: {err:#}");
    }
}

/// datagram(鼠标移动等低延迟输入)循环:与控制流并行,断开即退出。
fn spawn_datagram_loop(ctx: &Arc<SessionCtx>) {
    let ctx = Arc::clone(ctx);
    tokio::spawn(async move {
        while let Ok(bytes) = ctx.conn.read_datagram().await {
            if let Err(err) = input_handler::on_datagram(&ctx, &bytes) {
                tracing::debug!(name = %ctx.peer_name, "datagram 处理失败: {err:#}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 消息构造辅助
// ---------------------------------------------------------------------------

/// `HelloAck`:主机名 + 静态编码能力(动态探测由 T-016 经 `state.identity`/
/// rd-encode 接入后,再改此函数取 `&AgentState`)。
fn hello_ack(req_id: u32) -> Envelope {
    Envelope {
        req_id,
        msg: Some(envelope::Msg::HelloAck(HelloAck {
            proto_ver: PROTO_VER,
            host_name: host_name(),
            encode_caps: ENCODE_CAPS.to_vec(),
        })),
    }
}

/// 本机显示名(Windows:COMPUTERNAME;其他:HOSTNAME)。
fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// 协议 `Error` 消息体。
fn error_msg(code: u32, message: &str) -> Error {
    Error {
        code,
        message: message.to_string(),
    }
}

/// 协议 `Error` 信封。
fn error_env(req_id: u32, code: u32, message: &str) -> Envelope {
    Envelope {
        req_id,
        msg: Some(envelope::Msg::Error(error_msg(code, message))),
    }
}

/// 配对失败的用户可读原因(不含配对码)。
fn pair_error_reason(err: &PairError) -> String {
    match err {
        PairError::WrongCode => "配对码错误或已被使用".to_string(),
        PairError::Expired => "配对码已过期,请在托盘重新生成".to_string(),
        PairError::Locked { .. } => "配对失败次数过多,已临时锁定 10 分钟".to_string(),
        PairError::Protocol => "配对消息格式错误".to_string(),
    }
}

/// 信任列表锁辅助(临界区不跨 await)。
fn lock_trust(state: &AgentState) -> std::sync::MutexGuard<'_, rd_security::TrustStore> {
    state.trust.lock().expect("trust 锁中毒")
}
