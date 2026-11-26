//! rd-agent 核心库（融合模式 D-013 F-1）：被控核心作为库被 rd-app（Tauri 壳）
//! 内嵌；bin 目标（main.rs）保留无头被控模式。
//!
//! 入口 [`start`]：加载配置/身份/信任表 → 绑定 QUIC → 启动服务/发现/预热
//! 任务，返回 [`AgentRuntime`]（状态句柄 + 优雅断开）。调用方负责：
//! - 在 tokio runtime 上下文内调用（复用调用方 runtime，不自建）；
//! - 日志初始化（lib 不抢；
//! - 驻留与退出（UI 事件循环 / Ctrl+C）。

pub mod audit;
pub mod clipboard;
pub mod config;
pub mod discover;
pub mod easytier;
pub mod file_handler;
pub mod host_activity;
pub mod input_handler;
pub mod netinfo;
pub mod power;
pub mod privacy;
pub mod punch_candidates;
pub mod server;
pub mod session;
pub mod term_handler;
pub mod updater;
pub mod vdisplay;
pub mod window_session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context as _;

use rd_security::TrustStore;

pub use config::Config;
pub use session::AgentState;

/// 终端会话空闲保留时长（rd-term 文档建议的生产值）。
pub const TERM_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 一次被控核心启动的运行句柄：状态访问 + 优雅断开。
pub struct AgentRuntime {
    pub state: Arc<AgentState>,
    /// 实际 QUIC 监听地址。
    pub bound: Vec<SocketAddr>,
    /// 启动时的 runtime 句柄（托盘等非 runtime 线程 block_on 用）。
    pub rt: tokio::runtime::Handle,
}

impl AgentRuntime {
    /// 优雅断开：向全部在线控制端推 `AGENT_CLOSING`（`reason`）并取消其连接。
    /// 托盘「退出/断开所有」与 app 关闭路径共用；内部经启动时句柄 block_on，
    /// 因此可从任意线程调用。
    pub fn disconnect_all(&self, reason: &str) {
        self.state.disconnect_all_peers(&self.rt, reason);
    }
}

impl AgentState {
    /// [`AgentRuntime::disconnect_all`] 的实际实现：托盘更新线程只持有
    /// `Arc<AgentState>` + runtime 句柄（升级重启场景），同样需要优雅断开。
    pub fn disconnect_all_peers(&self, rt: &tokio::runtime::Handle, reason: &str) {
        let peers: Vec<_> = self
            .peers
            .lock()
            .expect("peers 锁中毒")
            .values()
            .cloned()
            .collect();
        for peer in peers {
            let ctl = Arc::clone(&peer.ctl);
            let reason = reason.to_string();
            rt.block_on(async move {
                let mut ctl = ctl.lock().await;
                let _ = ctl
                    .send(&rd_proto::v0::Envelope {
                        req_id: 0,
                        msg: Some(rd_proto::v0::envelope::Msg::SessionState(
                            rd_proto::v0::SessionState {
                                kind: 1, // AGENT_CLOSING（proto SessionState::Kind）
                                reason,
                            },
                        )),
                    })
                    .await;
            });
        }
        for peer in self.peers.lock().expect("peers 锁中毒").values() {
            peer.cancel.cancel();
        }
    }
}

/// 启动被控核心。必须在 tokio runtime 上下文内调用（runtime 由调用方提供，
/// 融合模式复用 Tauri 的）。`test_pair_code` 仅 debug 构建生效（端到端测试）。
pub fn start(
    config_dir: &Path,
    cli_listen: Option<SocketAddr>,
    test_pair_code: Option<String>,
) -> anyhow::Result<AgentRuntime> {
    #[cfg(target_os = "macos")]
    rd_mac::permissions::log_permission_status();
    // 后台预热硬件编码能力探测（OnceLock 缓存），避免首个控制端握手 Hello 时同步阻塞数秒
    let _ = std::thread::Builder::new()
        .name("rd-encode-probe".into())
        .spawn(|| {
            let _ = rd_encode::caps::probe_encode_caps();
        });

    let config = config::Config::load(config_dir)?;
    // 本机优先检测（D-013 包5）：幂等（线程已起则空转）
    host_activity::init();
    // 操作审计（合规留痕）：与 tracing 日志同目录，audit.jsonl
    audit::init(&config_dir.join("logs"));
    // 隐私屏黑罩线程（T-028 F9）：进程内一次，常驻待命
    privacy::init(config_dir);
    // E3 重启回看恢复：上次运行时隐私屏开着 → 重新遮蔽（保护跨重启）
    privacy::recover_state(config_dir);
    // E1 窗口账本目录登记：记账/销账（window_session）与 main.rs 的启动恢复
    // 同一目录——`--config-dir` 覆盖时若各自取目录账本永远对不上
    window_session::set_ledger_dir(config_dir);

    let identity = rd_security::load_or_create_identity(config_dir, "rd-agent")
        .context("加载/生成本机身份失败")?;
    let local_fp = identity.fingerprint();
    tracing::info!(fingerprint = local_fp.short_hex(), "本机指纹(前 8 字节)");
    let trust =
        TrustStore::open(config_dir.join("trusted.json").as_path()).context("打开信任列表失败")?;
    // 授权（T-029）：license.key 验签通过 = Pro；缺失/无效一律免费版，不阻断服务
    let license = rd_security::load_license(config_dir);

    let state = Arc::new(AgentState {
        config: config.clone(),
        identity,
        local_fp,
        trust: Mutex::new(trust),
        license,
        pairing: Mutex::new(rd_security::AgentPairing::new()),
        terms: rd_term::TermManager::new(TERM_IDLE_TIMEOUT),
        injector: Mutex::new(input_handler::Injector::new()),
        paused: AtomicBool::new(false),
        peers: Mutex::new(HashMap::new()),
        listen_addrs: Mutex::new(Vec::new()),
    });

    // EasyTier 组网托管（可选）：[easytier] enabled=true 才真正启动；未启用/
    // 失败仅降级——直连与中继路径完全不受影响。控制端可经 mesh 直拨本机
    // 虚拟 IP（TUN 模式），故宿主必须在 relay 块之外无条件托管。
    let easytier_host = match easytier::start(&state.config.easytier, &config_dir.join("easytier"))
    {
        Ok(host) => host,
        Err(e) => {
            tracing::warn!(err = %e, "EasyTier 托管启动失败（降级：仅直连/中继路径）");
            easytier::EasyTierHost {
                enabled: false,
                virtual_ip: Arc::new(Mutex::new(None)),
            }
        }
    };

    #[cfg(debug_assertions)]
    if let Some(code) = &test_pair_code {
        force_test_pair_code(&state, code)?;
        tracing::info!("已注入测试配对码(仅 debug 构建;码值不落日志)");
    }
    #[cfg(not(debug_assertions))]
    let _ = test_pair_code;

    let listen_addrs = server::resolve_listen_addrs(&state.config, cli_listen)?;
    let (servers, direct_socks): (Vec<_>, Vec<_>) =
        server::bind_servers_with_retry(&listen_addrs, &state.identity)?
            .into_iter()
            .unzip();
    let bound: Vec<SocketAddr> = servers.iter().map(|s| s.local_addr()).collect();
    tracing::info!(
        listen = ?bound,
        fingerprint = state.local_fp.short_hex(),
        "QUIC 监听就绪"
    );
    {
        // 托盘弹窗「本机地址」/发现应答端口消费：bind 后回填
        *state.listen_addrs.lock().expect("listen_addrs 锁中毒") = bound.clone();
    }
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            server::serve_all(state, servers).await;
        });
    }
    if cli_listen.is_none() {
        server::spawn_dynamic_interface_watcher(Arc::clone(&state));
    }
    {
        // UDP 局域网发现应答（21119）：失败仅降级（不影响 QUIC 服务）
        let state = Arc::clone(&state);
        tokio::spawn(async move { discover::serve(state).await });
    }
    {
        // 原生中继（T-032）：注册直连 socket 供启动期与后续 UI 免重启热切换中继时共用打洞响应器
        register_agent_direct_socks(state.local_fp, direct_socks);
        let relay_raw = std::env::var("RD_RELAY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| state.config.relay_addr.clone())
            .filter(|s| !s.trim().is_empty());
        if relay_raw.is_some() {
            spawn_relay_with_easytier(Arc::clone(&state), relay_raw, Some(easytier_host));
        }
    }
    {
        // encode_caps 预热（D-013 包2）：ffmpeg 444 探测提前到启动时后台执行
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(rd_encode::caps::probe_encode_caps).await;
        });
    }

    Ok(AgentRuntime {
        state,
        bound,
        rt: tokio::runtime::Handle::current(),
    })
}

/// 默认配置目录：Windows `%LOCALAPPDATA%\rd`（directories::ProjectDirs）。
pub fn default_config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "rd")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("rd"))
}

struct AgentRelaySlot {
    direct_socks: Vec<Arc<tokio::net::UdpSocket>>,
    cancel_tx: Option<tokio::sync::watch::Sender<bool>>,
}

fn relay_slots() -> &'static Mutex<HashMap<rd_transport::Fingerprint, AgentRelaySlot>> {
    static SLOTS: OnceLock<Mutex<HashMap<rd_transport::Fingerprint, AgentRelaySlot>>> =
        OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_agent_direct_socks(
    fp: rd_transport::Fingerprint,
    direct_socks: Vec<Arc<tokio::net::UdpSocket>>,
) {
    let mut map = relay_slots().lock().expect("relay_slots 锁中毒");
    let entry = map.entry(fp).or_insert_with(|| AgentRelaySlot {
        direct_socks: Vec::new(),
        cancel_tx: None,
    });
    entry.direct_socks = direct_socks;
}

/// 免重启热切换（或清除）当前被控核心的自建中继监听：
/// 1. 立即取消旧的中继监听与保活任务；
/// 2. 若 `raw_relay` 非空，启动带自动重连、15s 周期 REG 保活、虚拟 TUN 自动绕行及 P2P 打洞响应器的守护循环。
pub fn set_active_relay(state: Arc<AgentState>, raw_relay: Option<String>) {
    spawn_relay_with_easytier(state, raw_relay, None);
}

fn spawn_relay_with_easytier(
    state: Arc<AgentState>,
    raw_relay: Option<String>,
    easytier_host: Option<easytier::EasyTierHost>,
) {
    let cleaned = raw_relay
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let (direct_socks, cancel_rx) = {
        let mut map = relay_slots().lock().expect("relay_slots 锁中毒");
        let slot = map.entry(state.local_fp).or_insert_with(|| AgentRelaySlot {
            direct_socks: Vec::new(),
            cancel_tx: None,
        });
        if let Some(old_tx) = slot.cancel_tx.take() {
            let _ = old_tx.send(true);
        }
        let Some(_) = &cleaned else {
            tracing::info!("已清除自建中继配置，停止旧中继监听任务");
            return;
        };
        let (tx, rx) = tokio::sync::watch::channel(false);
        slot.cancel_tx = Some(tx);
        (slot.direct_socks.clone(), rx)
    };

    let raw = cleaned.expect("checked Some");
    tokio::spawn(async move {
        let mut cancel_rx = cancel_rx;
        let mut et_opt = easytier_host;
        loop {
            if *cancel_rx.borrow() {
                break;
            }
            let punch_on = rd_transport::relay::punch_enabled();
            let listen_port = state
                .listen_addrs
                .lock()
                .expect("listen_addrs 锁中毒")
                .first()
                .map(|a| a.port())
                .unwrap_or(21118);

            let candidates = if punch_on {
                let et_ip = if let Some(et) = et_opt.take() {
                    et.wait_ready(Duration::from_secs(20)).await
                } else {
                    None
                };
                if let Some(ip) = et_ip {
                    tracing::info!(%ip, "EasyTier 虚拟 IP 纳入直连候选");
                }
                punch_candidates::collect(listen_port, et_ip).await
            } else {
                Vec::new()
            };

            let stripped = raw
                .strip_prefix("udp://")
                .or_else(|| raw.strip_prefix("UDP://"))
                .unwrap_or(&raw);
            let is_ws = stripped.starts_with("http://")
                || stripped.starts_with("https://")
                || stripped.starts_with("ws://")
                || stripped.starts_with("wss://");

            if is_ws {
                match rd_transport::parse_relay_endpoint(stripped) {
                    Ok(ep) => match rd_transport::bind_via_relay_ep(&ep, &state.identity).await {
                        Ok(srv) => {
                            tracing::info!(relay = %raw, "WS/HTTPS 自建中继监听就绪");
                            let st = Arc::clone(&state);
                            tokio::select! {
                                _ = server::serve_all(st, vec![srv]) => {}
                                _ = cancel_rx.changed() => break,
                            }
                        }
                        Err(err) => {
                            tracing::warn!(relay = %raw, "WS 中继注册失败，8s 后自动重试: {err:#}");
                        }
                    },
                    Err(err) => {
                        tracing::warn!(relay = %raw, "中继地址格式无效，停止重试: {err:#}");
                        break;
                    }
                }
            } else {
                let resolved_sock = match rd_transport::relay::parse_relay_addr(stripped) {
                    Ok(s) => Ok(s),
                    Err(_) => {
                        let with_port = if stripped.contains(':') {
                            stripped.to_string()
                        } else {
                            format!("{stripped}:21118")
                        };
                        tokio::net::lookup_host(&with_port)
                            .await
                            .ok()
                            .and_then(|mut it| it.next())
                            .ok_or_else(|| anyhow::anyhow!("无法解析中继地址 {with_port}"))
                    }
                };

                match resolved_sock {
                    Ok(addr) => {
                        match rd_transport::relay::bind_via_relay_ex(
                            addr,
                            &state.identity,
                            candidates,
                        )
                        .await
                        {
                            Ok(relay_server) => {
                                tracing::info!(
                                    %addr,
                                    "原生中继监听就绪（已启用 15s 保活 + P2P 打洞响应器）"
                                );
                                if punch_on && !direct_socks.is_empty() {
                                    tokio::spawn(rd_transport::relay::punch_respond(
                                        relay_server.punch_req,
                                        direct_socks.clone(),
                                    ));
                                }
                                let st = Arc::clone(&state);
                                tokio::select! {
                                    _ = server::serve_all(st, vec![relay_server.server]) => {}
                                    _ = cancel_rx.changed() => break,
                                }
                            }
                            Err(err) => {
                                tracing::warn!(
                                    %addr,
                                    "中继注册失败（直连不受影响），8s 后自动重试: {err:#}"
                                );
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(relay = %raw, "中继域名解析失败，8s 后重试: {err:#}");
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(8)) => {}
                _ = cancel_rx.changed() => break,
            }
        }
    });
}

/// (仅 debug 构建)把 AgentPairing 的配对码"摇"成指定值，供端到端测试。
///
/// rd-security 出于安全不提供注入固定码的 API（生产码必须 CSPRNG 生成）；
/// 这里反复 `new_code()` 直到命中目标值——6 位码期望约 10^6 次调用（毫秒级）。
/// 码值本身不落日志。
#[cfg(debug_assertions)]
fn force_test_pair_code(state: &AgentState, code: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
        "测试配对码必须是 6 位数字"
    );
    let mut pairing = state.pairing.lock().expect("pairing 锁中毒");
    for _ in 0..50_000_000u32 {
        if pairing.new_code() == code {
            return Ok(());
        }
    }
    anyhow::bail!("未能摇出测试配对码(概率极低,请重试)")
}
