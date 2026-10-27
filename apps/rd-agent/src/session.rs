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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use rd_proto::v0::{envelope, ClipboardData, Envelope, Error, Hello, HelloAck, PairResult, SetPrivacy};
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
/// 协议错误码 50:剪贴板操作失败(05_api §4,v0.1 追加)。
const ERR_CLIPBOARD: u32 = 50;
/// 协议错误码 70:免费版权益限制(T-029 商业化门控;升级 Pro 解除)。
const ERR_LICENSE_LIMIT: u32 = 70;
/// 连接确认等待时长:超时未确认视为拒绝。
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// 被控端确认框:独立线程弹 MessageBox(Yes/No),30s 超时视为拒绝。
/// 模态于该线程,不阻塞 Agent 其余会话。
async fn confirm_connection(peer_name: &str, peer_fp: &Fingerprint) -> bool {
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let name = peer_name.to_string();
    let fp = peer_fp.short_hex();
    std::thread::spawn(move || {
        #[cfg(windows)]
        {
            use windows::core::HSTRING;
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST,
                MB_YESNO,
            };
            let text = HSTRING::from(format!(
                "设备「{name}」({fp}…) 请求远程控制这台电脑。\n\n允许这次连接吗?"
            ));
            let title = HSTRING::from("rd 远程控制 · 连接请求");
            let yes = unsafe {
                MessageBoxW(
                    None,
                    &text,
                    &title,
                    MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST | MB_DEFBUTTON2,
                ) == IDYES
            };
            let _ = tx.send(yes);
        }
        #[cfg(not(windows))]
        {
            let _ = (name.clone(), fp.clone(), tx);
        }
    });
    tokio::time::timeout(CONFIRM_TIMEOUT, rx)
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false)
}

/// device_name 消毒:trim + 去控制字符 + 限长 64,空值回落占位
/// (D-013 包3/E4:防日志伪造与存储塞爆)。
fn sanitize_device_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}
/// Agent 侧编出的编码能力缓存(T-015/T-016 遗留收尾):rd-encode 动态探测
/// 结果(`H264420` 恒有;`H264444` 以硬编实际编一帧验证,依本机环境而定)。
static ENCODE_CAPS: OnceLock<Vec<i32>> = OnceLock::new();
/// 配对空闲超时(D-013 包3):每条消息间隔上限,超时断开未鉴权连接。
const PAIRING_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 同进程并发配对数上限(D-013 包3):防垃圾报文洪水中配对循环堆积。
const PAIRING_MAX: usize = 4;
/// 当前并发配对数。
static PAIRING_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 编码能力(进程内首次调用触发 rd-encode 的 4:4:4 硬编探测并缓存)。
///
/// 副作用:进程首个 `HelloAck` 会等待 ~1.5s 的 ffmpeg 探测——控制端握手
/// RPC 超时 5s,可接受;此后握手零开销。
fn encode_caps() -> &'static [i32] {
    ENCODE_CAPS.get_or_init(|| {
        rd_encode::caps::probe_encode_caps()
            .into_iter()
            .map(|c| c as i32)
            .collect()
    })
}

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
    /// 授权状态（`None` = 免费版；`license.key` 验签通过才有值）。
    /// 免费版门控：并发连接 1、信任设备 2（见 rd-security license 常量）。
    pub license: Option<rd_security::License>,
    pub pairing: Mutex<AgentPairing>,
    pub terms: TermManager,
    pub injector: Mutex<Injector>,
    pub paused: AtomicBool,
    /// 连接 id → 在线对端(托盘显示与断开清理用)。
    pub peers: Mutex<HashMap<u64, PeerInfo>>,
    /// 实际 QUIC 监听地址(bind 后由 main 回填;托盘"本机地址"展示用,
    /// 构建期未知故为 Mutex+空初始化)。
    pub listen_addrs: Mutex<Vec<std::net::SocketAddr>>,
}

/// 一台在线控制端(name/fp/since 由 T-018 托盘消费;cancel 为本连接取消令牌)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PeerInfo {
    pub name: String,
    pub fp: Fingerprint,
    pub since: SystemTime,
    /// 该连接的取消令牌:T-016/017 的流任务应 `select` 它,托盘"断开"亦用它。
    pub cancel: CancellationToken,
    /// 控制流发送端(托盘"断开所有"/暂停共享前推 SessionState,v0.1)。
    pub ctl: Arc<tokio::sync::Mutex<ControlTx>>,
    /// 所属 runtime(托盘线程为非 runtime 线程,block_on 用)。
    pub rt: tokio::runtime::Handle,
}

/// 单连接上下文:分发给各 handler 的只读视图 + 控制流发送端。
pub struct SessionCtx {
    pub state: Arc<AgentState>,
    pub conn: Arc<Conn>,
    /// 控制流发送端。用 tokio Mutex:`send` 需要跨 await 持锁(分发循环、
    /// datagram 任务、T-016/017 的流任务都可能并发回包,std 互斥无法做到);
    /// Arc 包装:PeerInfo(托盘推 SessionState)共享同一发送端。
    pub ctl: Arc<tokio::sync::Mutex<ControlTx>>,
    pub peer_name: String,
    /// 对端指纹短码（审计埋点的真实身份锚点；name 可伪造、fp 不可）。
    pub peer_fp: String,
    /// 自控自（对端是本机 loopback）：豁免「本机优先」注入挂起——同一块
    /// 键鼠不存在抢占，busy 门控只会让工具栏常用语/虚拟按键全部失效
    /// （2026-10-27 用户手测实锤）。
    pub loopback: bool,
    /// 对端 `Hello`(含 decode_caps,供 T-016 编码选路)。
    #[allow(dead_code)]
    pub hello: Hello,
    /// T-027：视频是否走 fec-shards（Hello 协商 + 被控端开关）
    pub media_fec: bool,
    /// T-028 F5：控制端手动画质档位（`QualityPreset` as i32，默认 AUTO）。
    /// 非 AUTO 时会话码率自适应停用，`apply_rate` 直接按档位下发。
    pub user_quality: Arc<AtomicU32>,
    /// T-028 F9：本会话是否开启了隐私屏（断开时据此自动解除黑罩）。
    pub privacy_on: AtomicBool,
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

    let (mut hello, hello_req_id) = recv_hello(&mut ctl_rx).await?;
    // device_name 消毒(D-013 包3/E4):原样进日志/信任列表/托盘,可换行伪造
    // 日志行、塞爆存储——trim + 去控制字符 + 限长 64,空值回落占位。
    hello.device_name = sanitize_device_name(&hello.device_name);
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
        ctl_tx.send(&hello_ack(hello_req_id, hello.media_fec)).await?;
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

    // 连接确认(D-013 包3/E4:消除"静默被控"——信任设备此前连上即得
    // 屏幕+输入+终端全权,唯一信号是托盘图标变色)。30s 未确认即断开;
    // debug 构建默认关(e2e),生产默认开。
    if state.config.confirm_connection && !confirm_connection(&hello.device_name, &peer_fp).await {
        tracing::info!(peer = peer_fp.short_hex(), "对方未确认控制请求,断开");
        crate::audit::log("confirm_denied", &hello.device_name, &peer_fp.short_hex(), serde_json::json!({}));
        return Ok(());
    }

    // 会话保活(D-013 包2):有受信任连接期间阻止屏保/息屏/睡眠
    crate::power::acquire();

    // --- Authenticated:构造上下文、注册对端、启动 datagram 循环、分发 ---
    // 连接序号:Agent 进程内自增(rd-transport 未暴露 quinn stable_id;
    // 仅用于本进程内 peers 表/托盘展示,唯一性在进程内成立即可)
    static CONN_SEQ: AtomicU64 = AtomicU64::new(1);
    let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    let cancel = CancellationToken::new();
    // 自控自判定（conn move 前取地址）：loopback 连接豁免「本机优先」挂起
    let is_loopback = conn.peer_addr().ip().is_loopback();

    // 免费版门控 1/2:并发控制端连接数(T-029;Pro 不限)。在注册与任何
    // 能力授予前拒绝,提示大白话 + 升级路径。
    let is_pro = state
        .license
        .as_ref()
        .is_some_and(|l| l.plan == rd_security::Plan::Pro);
    if !is_pro && state.peers.lock().expect("peers 锁中毒").len() >= rd_security::license::FREE_CONN_MAX {
        tracing::info!(peer = peer_fp.short_hex(), "免费版并发连接已达上限,拒绝");
        crate::audit::log(
            "license_reject_conn",
            &hello.device_name,
            &peer_fp.short_hex(),
            serde_json::json!({}),
        );
        ctl_tx
            .send(&error_env(
                0,
                ERR_LICENSE_LIMIT,
                "免费版同一时间只能 1 台电脑连接。请在被控端放入授权码升级(托盘→授权)。",
            ))
            .await?;
        return Ok(());
    }

    // 控制流发送端 Arc:SessionCtx(分发回包)与 PeerInfo(托盘推 SessionState)共享
    let ctl_shared = Arc::new(tokio::sync::Mutex::new(ctl_tx));

    let media_fec = hello.media_fec;
    if media_fec {
        tracing::info!("视频路径协商：fec-shards（T-027）");
    }
    let ctx = Arc::new(SessionCtx {
        state: Arc::clone(&state),
        conn: Arc::new(conn),
        ctl: Arc::clone(&ctl_shared),
        peer_name: hello.device_name.clone(),
        peer_fp: peer_fp.short_hex().to_string(),
        loopback: is_loopback,
        hello,
        media_fec,
        user_quality: Arc::new(AtomicU32::new(0)),
        privacy_on: AtomicBool::new(false),
        windows: WindowSessions,
        terms: TermAttachments,
    });

    {
        state.peers.lock().expect("peers 锁中毒").insert(
            conn_id,
            PeerInfo {
                name: ctx.peer_name.clone(),
                fp: peer_fp,
                since: SystemTime::now(),
                cancel: cancel.clone(),
                ctl: Arc::clone(&ctl_shared),
                rt: tokio::runtime::Handle::current(),
            },
        );
    }

    spawn_datagram_loop(&ctx);

    tracing::info!(peer = peer_fp.short_hex(), name = %ctx.peer_name, "会话进入已鉴权状态");
    // 审计（合规留痕）：会话开始。media_fec 单列，画质档位变化另有事件。
    crate::audit::log(
        "session_start",
        &ctx.peer_name,
        &ctx.peer_fp,
        serde_json::json!({ "conn": conn_id, "media_fec": media_fec }),
    );
    let started_at = SystemTime::now();
    let dispatch_result = dispatch_loop(&ctx, &mut ctl_rx).await;

    // --- 断开清理(任务卡步骤 5)---
    crate::power::release();
    window_session::on_disconnect(&ctx);
    input_handler::on_disconnect(&ctx);
    term_handler::on_disconnect(&ctx);
    if let Some(peer) = state.peers.lock().expect("peers 锁中毒").remove(&conn_id) {
        peer.cancel.cancel();
    }
    {
        let secs = started_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
        crate::audit::log(
            "session_end",
            &ctx.peer_name,
            &peer_fp.short_hex(),
            serde_json::json!({ "conn": conn_id, "duration_s": secs }),
        );
    }
    // 隐私屏随会话解除（T-028 F9）：绝不允许黑罩在无会话时残留——家人屏幕
    // 被永远盖住是最坏故障形态
    if ctx.privacy_on.load(Ordering::SeqCst) {
        crate::privacy::set(false);
        ctx.privacy_on.store(false, Ordering::SeqCst);
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
///
/// 资源防护(D-013 包3/E4):每连接空闲 60s 未完成配对即断开(此前可无限
/// 挂);同进程并发配对数上限 [`PAIRING_MAX`](恶意节点用垃圾报文把配对锁
/// 死的洪水面)。
async fn run_pairing(
    state: &Arc<AgentState>,
    ctl_tx: &mut ControlTx,
    ctl_rx: &mut ControlRx,
    hello: &Hello,
    local_fp: Fingerprint,
    peer_fp: Fingerprint,
) -> anyhow::Result<bool> {
    if PAIRING_COUNT.load(Ordering::Relaxed) >= PAIRING_MAX {
        tracing::warn!(peer = peer_fp.short_hex(), "并发配对数已达上限,拒绝");
        return Ok(false);
    }
    struct PairingGuard;
    impl Drop for PairingGuard {
        fn drop(&mut self) {
            PAIRING_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
    }
    PAIRING_COUNT.fetch_add(1, Ordering::Relaxed);
    let _guard = PairingGuard;

    tracing::info!(peer = peer_fp.short_hex(), "未受信任设备,进入配对流程");
    loop {
        let env = match tokio::time::timeout(PAIRING_IDLE_TIMEOUT, ctl_rx.recv()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => return Ok(false),
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                tracing::info!(peer = peer_fp.short_hex(), "配对空闲超时,断开");
                return Ok(false);
            }
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
    // 免费版门控 2/2:受信任设备数上限(T-029;Pro 不限)。配对码已验证
    // 正确仍在此时拦下——码是当场授权,门控是长期权益,两回事。
    let is_pro = state
        .license
        .as_ref()
        .is_some_and(|l| l.plan == rd_security::Plan::Pro);
    if !is_pro {
        let count = lock_trust(state).list().len();
        if count >= rd_security::license::FREE_DEVICE_MAX as usize {
            tracing::info!(peer = peer_fp.short_hex(), count, "免费版信任设备已达上限,拒绝配对");
            crate::audit::log(
                "license_reject_pair",
                &hello.device_name,
                &peer_fp.short_hex(),
                serde_json::json!({ "trusted": count }),
            );
            ctl_tx
                .send(&Envelope {
                    req_id: 0,
                    msg: Some(envelope::Msg::PairRes(PairResult {
                        ok: false,
                        spake2_msg: Vec::new(),
                        reason: format!(
                            "免费版最多记住 {} 台电脑。删除不再用的设备(托盘→信任设备),或放入授权码升级。",
                            rd_security::FREE_DEVICE_MAX
                        ),
                    })),
                })
                .await?;
            return Ok(());
        }
    }
    {
        lock_trust(state)
            .add(peer_fp, &hello.device_name)
            .context("配对成功但写入信任列表失败")?;
    }
    tracing::info!(peer = peer_fp.short_hex(), name = %hello.device_name, "配对成功,已加入信任列表");
    crate::audit::log("pair_success", &hello.device_name, &peer_fp.short_hex(), serde_json::json!({}));
    // HelloAck 尽力发送:对端配对成功后可能立即断开重连
    // 配对成功后的主动推送(对端可能已断开重连),无对应请求,req_id=0
    // 配对期 ack 同样回带协商位：会话上下文以 hello.media_fec 生效，ack 若
    // 撒谎 false，控制端会按非 fec 路径解码而收不到视频流（两端认知分裂）
    let _ = ctl_tx.send(&hello_ack(0, hello.media_fec)).await;
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
            // 画质档位（T-028 F5）：请求-应答（echo），失败回 Error{60}
            envelope::Msg::SetQuality(m) => handle_soft(
                &ctx.peer_name,
                window_session::set_quality(ctx, env.req_id, m).await,
            ),
            // 隐私屏（T-028 F9，Pro）：机器级黑罩；断开自动解除
            envelope::Msg::SetPrivacy(m) => handle_soft(
                &ctx.peer_name,
                handle_privacy(ctx, env.req_id, m).await,
            ),
            // 输入 → T-017;UIPI 拦截(管理员窗口)按 30s 去抖回一次性 Error
            envelope::Msg::Key(m) => {
                if let Some((code, message)) =
                    input_handler::handle_with_notify(ctx, InputMsg::Key(m))
                {
                    ctx.send(envelope::Msg::Error(error_msg(code, &message)), env.req_id)
                        .await?;
                }
            }
            envelope::Msg::MouseBtn(m) => {
                if let Some((code, message)) =
                    input_handler::handle_with_notify(ctx, InputMsg::MouseButton(m))
                {
                    ctx.send(envelope::Msg::Error(error_msg(code, &message)), env.req_id)
                        .await?;
                }
            }
            envelope::Msg::Wheel(m) => handle_soft(
                &ctx.peer_name,
                input_handler::handle(ctx, InputMsg::MouseWheel(m)),
            ),
            envelope::Msg::Text(m) => {
                if let Some((code, message)) =
                    input_handler::handle_with_notify(ctx, InputMsg::TextInput(m))
                {
                    ctx.send(envelope::Msg::Error(error_msg(code, &message)), env.req_id)
                        .await?;
                }
            }
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
            // 剪贴板同步(v0.1/D-013 包4)
            envelope::Msg::Clipboard(m) => handle_clipboard(ctx, env.req_id, m).await?,
            // 其余消息:静默/记录,不打断会话
            envelope::Msg::Stats(m) => {
                tracing::debug!(name = %ctx.peer_name, fps = m.fps, rtt_ms = m.rtt_ms, kbps = m.kbps, "控制端统计(码率自适应 T-016 接入)");
            }
            envelope::Msg::Error(m) => {
                tracing::debug!(name = %ctx.peer_name, code = m.code, "控制端报告错误: {}", m.message);
            }
            envelope::Msg::SessionState(m) => {
                tracing::debug!(name = %ctx.peer_name, kind = m.kind, "忽略反向 SessionState");
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

/// 隐私屏开关（T-028 F9，Pro 功能）：黑罩盖住被控机全屏幕（点击穿透、
/// 远控画面不受影响），会话断开自动解除。
///
/// 开启时先弹系统提示框（家人点「确定」后才进入黑屏）——被控端知情是
/// 合规底线；关闭即时生效无提示。
async fn handle_privacy(ctx: &SessionCtx, req_id: u32, m: SetPrivacy) -> anyhow::Result<()> {
    let is_pro = ctx
        .state
        .license
        .as_ref()
        .is_some_and(|l| l.plan == rd_security::Plan::Pro);
    if !is_pro {
        return ctx
            .send(
                envelope::Msg::Error(error_msg(
                    ERR_LICENSE_LIMIT,
                    "隐私屏是专业版功能。可在被控端放入授权码升级(托盘→授权)。",
                )),
                req_id,
            )
            .await;
    }
    if m.on {
        // 知情提示先行（异步弹窗，不阻塞回包）；黑罩随后升起
        crate::privacy::notify_user(&ctx.peer_name);
        if !crate::privacy::set(true) {
            return ctx
                .send(
                    envelope::Msg::Error(error_msg(60, "隐私屏开启失败(黑罩线程不可用)")),
                    req_id,
                )
                .await;
        }
        ctx.privacy_on.store(true, Ordering::SeqCst);
        crate::audit::log("privacy_on", &ctx.peer_name, &ctx.peer_fp, serde_json::json!({}));
    } else {
        let _ = crate::privacy::set(false);
        ctx.privacy_on.store(false, Ordering::SeqCst);
        crate::audit::log("privacy_off", &ctx.peer_name, &ctx.peer_fp, serde_json::json!({}));
    }
    tracing::info!(on = m.on, name = %ctx.peer_name, "隐私屏已切换");
    ctx.send(
        envelope::Msg::SetPrivacy(SetPrivacy { window_id: m.window_id, on: m.on }),
        req_id,
    )
    .await
}

/// 剪贴板同步(v0.1/D-013 包4):write=写本机剪贴板;read=回发当前文本。
/// 内容绝不落日志(DoD 同按键纪律),只记操作与字符数。
async fn handle_clipboard(ctx: &SessionCtx, req_id: u32, m: ClipboardData) -> anyhow::Result<()> {
    if m.write {
        let chars = m.text.chars().count();
        let ok = crate::clipboard::write_text(&m.text);
        tracing::info!(name = %ctx.peer_name, ok, chars, "写入本机剪贴板");
        // 审计：只记方向与字符数，内容绝不落痕
        crate::audit::log(
            "clipboard_write",
            &ctx.peer_name,
            &ctx.peer_fp,
            serde_json::json!({ "chars": chars, "ok": ok }),
        );
        if !ok {
            ctx.send(
                envelope::Msg::Error(error_msg(ERR_CLIPBOARD, "写入被控端剪贴板失败")),
                req_id,
            )
            .await?;
        }
        Ok(())
    } else {
        let text = crate::clipboard::read_text().unwrap_or_default();
        tracing::info!(name = %ctx.peer_name, chars = text.chars().count(), "回读本机剪贴板");
        crate::audit::log(
            "clipboard_read",
            &ctx.peer_name,
            &ctx.peer_fp,
            serde_json::json!({ "chars": text.chars().count() }),
        );
        ctx.send(
            envelope::Msg::Clipboard(ClipboardData { write: false, text }),
            req_id,
        )
        .await
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

/// `HelloAck`:主机名 + 版本 + 动态探测的编码能力(见 `encode_caps`)。
fn hello_ack(req_id: u32, media_fec: bool) -> Envelope {
    Envelope {
        req_id,
        msg: Some(envelope::Msg::HelloAck(HelloAck {
            proto_ver: PROTO_VER,
            host_name: host_name(),
            encode_caps: encode_caps().to_vec(),
            host_version: env!("CARGO_PKG_VERSION").to_string(),
            // T-027：fec-shards 协商确认（默认关，RD_MEDIA_FEC=1 才声明支持）
            media_fec,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// encode_caps 内容断言:`H264420` 恒在(OpenH264 软编兜底);`H264444`
    /// 依本机硬编探测结果(有则追加、无则缺省),且结果缓存后零开销稳定。
    /// 首次调用触发真实 ffmpeg 探测(~1.5s,仅一次)。
    #[test]
    fn encode_caps_always_advertise_420() {
        let caps = encode_caps();
        assert!(
            caps.contains(&(rd_proto::v0::Codec::H264420 as i32)),
            "H264420(软编兜底)必须始终上报,实际 {caps:?}"
        );
        // 444 有无不依赖测试环境:只断言内容合法(仅两个已知 codec、无重复)。
        let known = [
            rd_proto::v0::Codec::H264420 as i32,
            rd_proto::v0::Codec::H264444 as i32,
        ];
        assert!(
            caps.iter().all(|c| known.contains(c)),
            "只允许上报探测过的 codec,实际 {caps:?}"
        );
        assert!(
            !rd_encode::caps::probe_encode_caps().is_empty(),
            "探测结果不得为空(至少 420)"
        );
        assert_eq!(caps, encode_caps(), "OnceLock 缓存后结果应稳定");
    }
}
