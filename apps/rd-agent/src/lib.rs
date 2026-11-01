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
pub mod file_handler;
pub mod host_activity;
pub mod input_handler;
pub mod netinfo;
pub mod power;
pub mod privacy;
pub mod server;
pub mod session;
pub mod term_handler;
pub mod window_session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
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
        let peers: Vec<_> = self
            .state
            .peers
            .lock()
            .expect("peers 锁中毒")
            .values()
            .cloned()
            .collect();
        for peer in peers {
            let ctl = Arc::clone(&peer.ctl);
            let reason = reason.to_string();
            self.rt.block_on(async move {
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
        for peer in self.state.peers.lock().expect("peers 锁中毒").values() {
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
    let config = config::Config::load(config_dir)?;
    // 本机优先检测（D-013 包5）：幂等（线程已起则空转）
    host_activity::init();
    // 操作审计（合规留痕）：与 tracing 日志同目录，audit.jsonl
    audit::init(&config_dir.join("logs"));
    // 隐私屏黑罩线程（T-028 F9）：进程内一次，常驻待命
    privacy::init();

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

    #[cfg(debug_assertions)]
    if let Some(code) = &test_pair_code {
        force_test_pair_code(&state, code)?;
        tracing::info!("已注入测试配对码(仅 debug 构建;码值不落日志)");
    }
    #[cfg(not(debug_assertions))]
    let _ = test_pair_code;

    let listen_addrs = server::resolve_listen_addrs(&state.config, cli_listen)?;
    let servers = server::bind_servers(&listen_addrs, &state.identity)?;
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
    {
        // UDP 局域网发现应答（21119）：失败仅降级（不影响 QUIC 服务）
        let state = Arc::clone(&state);
        tokio::spawn(async move { discover::serve(state).await });
    }
    {
        // 原生中继（T-032）：配置了 relay_addr / RD_RELAY 才启用。异步注册
        // （agent.toml relay_addr = "1.2.3.4:21118"）；失败仅降级——直连
        // 路径完全不受影响，中继是打洞失败场景的兜底入口
        let relay_raw = std::env::var("RD_RELAY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| state.config.relay_addr.clone());
        if let Some(raw) = relay_raw {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                match rd_transport::relay::parse_relay_addr(&raw) {
                    Ok(addr) => {
                        match rd_transport::relay::bind_via_relay(addr, &state.identity).await {
                            Ok(server) => {
                                tracing::info!(%addr, "原生中继监听就绪（跨网兜底入口）");
                                server::serve_all(state, vec![server]).await;
                            }
                            Err(err) => {
                                tracing::warn!(%addr, "中继注册失败（直连不受影响）: {err:#}")
                            }
                        }
                    }
                    Err(err) => tracing::warn!("中继地址解析失败（直连不受影响）: {err:#}"),
                }
            });
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
