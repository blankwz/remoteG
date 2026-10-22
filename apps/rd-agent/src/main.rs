// release 构建隐藏控制台窗口(小白双击即用,无黑框不闪退);日志已双写文件,
// stdout 层在无控制台时静默。debug/测试构建保留控制台(RD_LISTEN 解析依赖 stdout)。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! rd-agent — Windows 被控端。
//!
//! 无参数(双击)等价 `run`:自动发现监听地址 + 托盘;命令行子命令保留给
//! 测试与调试(`run --print-port`、`spike`)。

mod clipboard;
mod config;
mod discover;
mod input_handler;
mod power;
mod server;
mod session;
mod term_handler;
mod tray;
mod window_session;

#[cfg(windows)]
mod spike;

use std::collections::HashMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing_subscriber::prelude::*;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};

use crate::session::AgentState;

/// 终端会话空闲保留时长(rd-term 文档建议的生产值)。
const TERM_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Windows 下提供 clap 子命令分发。`spike` 依赖 WGC 与 Win32 枚举,仅 Windows 可用;
/// `run` 的核心逻辑跨平台(托盘为 Windows 项)。
fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        #[cfg(windows)]
        Some(Commands::Spike(args)) => spike::run(args),
        Some(Commands::Run(args)) => run_app(args),
        // 无参数(双击启动)= 正式服务
        None => run_app(RunArgs::default()),
    }
}

#[derive(Parser, Debug)]
#[command(name = "rd-agent", about = "窗口级远程控制被控端")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// M0 Spike:捕获指定窗口 → H.264 编码 → QUIC 视频流发给首个连接的客户端
    #[cfg(windows)]
    Spike(spike::SpikeArgs),
    /// 正式被控服务:监听、鉴权与消息分发(T-015 起)
    Run(RunArgs),
}

/// `run` 子命令参数。
#[derive(Args, Debug, Default)]
pub struct RunArgs {
    /// 配置/身份/信任列表目录(默认 %LOCALAPPDATA%\rd;日志在其下 logs/)。
    #[arg(long)]
    config_dir: Option<PathBuf>,
    /// 覆盖监听地址(如 `127.0.0.1:0`);缺省按配置自动选择(Tailscale 优先)。
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// 不启动托盘(服务/测试模式)。
    #[arg(long, default_value_t = false)]
    no_tray: bool,
    /// 把实际监听地址逐行打印到 stdout(供测试/脚本读取)。
    #[arg(long, default_value_t = false)]
    print_port: bool,
    /// (仅 debug 构建生效)固定 6 位测试配对码,供端到端测试(T-025)。
    #[cfg(debug_assertions)]
    #[arg(long)]
    test_pair_code: Option<String>,
}

/// `run` 子命令入口。
fn run_app(args: RunArgs) -> anyhow::Result<()> {
    let config_dir = args.config_dir.clone().unwrap_or_else(default_config_dir);
    // guard 必须存活到进程结束,否则非阻塞日志线程停摆
    let _log_guard = init_logging(&config_dir.join("logs"));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "rd-agent 启动");

    let config = config::Config::load(&config_dir)?;
    let identity = rd_security::load_or_create_identity(&config_dir, "rd-agent")
        .context("加载/生成本机身份失败")?;
    let local_fp = identity.fingerprint();
    tracing::info!(fingerprint = local_fp.short_hex(), "本机指纹(前 8 字节)");
    let trust = rd_security::TrustStore::open(&config_dir.join("trusted.json"))
        .context("打开信任列表失败")?;

    let state = Arc::new(AgentState {
        config: config.clone(),
        identity,
        local_fp,
        trust: Mutex::new(trust),
        pairing: Mutex::new(rd_security::AgentPairing::new()),
        terms: rd_term::TermManager::new(TERM_IDLE_TIMEOUT),
        injector: Mutex::new(input_handler::Injector::new()),
        paused: AtomicBool::new(false),
        peers: Mutex::new(HashMap::new()),
        listen_addrs: Mutex::new(Vec::new()),
    });

    #[cfg(debug_assertions)]
    if let Some(code) = &args.test_pair_code {
        force_test_pair_code(&state, code)?;
        tracing::info!("已注入测试配对码(仅 debug 构建;码值不落日志)");
    }

    let listen_addrs = server::resolve_listen_addrs(&state.config, args.listen)?;

    // tokio 跑在后台线程,主线程留给托盘/信号等待;
    // quinn Endpoint 的创建依赖 runtime 上下文,绑定须在 runtime 就绪后进行
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("创建 tokio runtime 失败")?;
    let servers = rt.block_on(async { server::bind_servers(&listen_addrs, &state.identity) })?;
    let bound: Vec<SocketAddr> = servers.iter().map(|s| s.local_addr()).collect();
    tracing::info!(
        listen = ?bound,
        fingerprint = state.local_fp.short_hex(),
        "QUIC 监听就绪"
    );
    {
        // 托盘弹窗"本机地址"消费:bind 之前已构建 AgentState,bind 后回填
        *state.listen_addrs.lock().expect("listen_addrs 锁中毒") = bound.clone();
    }
    {
        let state = Arc::clone(&state);
        rt.spawn(async move {
            server::serve_all(state, servers).await;
        });
    }

    {
        // UDP 局域网发现应答(21119):失败仅降级(不影响 QUIC 服务)
        let state = Arc::clone(&state);
        rt.spawn(async move { discover::serve(state).await });
    }

    {
        // encode_caps 预热(D-013 包2):ffmpeg 444 探测 ~1.5s,提前到启动时
        // 后台执行,消除首个 HelloAck 的同步等待(控制端 RPC 超时 5s)
        rt.spawn(async move {
            let _ = tokio::task::spawn_blocking(rd_encode::caps::probe_encode_caps).await;
        });
    }

    if args.print_port {
        // 机器可读前缀:与彩色日志行区分,解析方按行前缀过滤
        for addr in &bound {
            println!("RD_LISTEN={addr}");
        }
        let _ = std::io::stdout().flush();
    }

    park_main_thread(&state, &rt, args.no_tray)?;
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

/// 主线程驻留:Windows 默认跑托盘(永不返回);`--no-tray` 或非 Windows 等待 Ctrl+C。
#[cfg(windows)]
fn park_main_thread(
    state: &Arc<AgentState>,
    rt: &tokio::runtime::Runtime,
    no_tray: bool,
) -> anyhow::Result<()> {
    if no_tray {
        wait_ctrl_c(rt)
    } else {
        tray::run(Arc::clone(state))
    }
}

/// 非 Windows 平台无托盘:一律等待 Ctrl+C(保持可 check/可运行)。
#[cfg(not(windows))]
fn park_main_thread(
    state: &Arc<AgentState>,
    rt: &tokio::runtime::Runtime,
    _no_tray: bool,
) -> anyhow::Result<()> {
    let _ = state;
    wait_ctrl_c(rt)
}

fn wait_ctrl_c(rt: &tokio::runtime::Runtime) -> anyhow::Result<()> {
    rt.block_on(tokio::signal::ctrl_c())
        .context("等待 Ctrl+C 失败")?;
    tracing::info!("收到 Ctrl+C,退出");
    Ok(())
}

/// 默认配置目录:Windows `%LOCALAPPDATA%\rd`(directories::ProjectDirs)。
fn default_config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "rd")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("rd"))
}

/// 按天滚动日志:`<config_dir>/logs/rd-agent.YYYY-MM-DD.log`,保留 7 天。
/// 返回的 guard 必须存活至进程结束。禁止记录输入内容、终端数据与配对码。
fn init_logging(dir: &Path) -> tracing_appender::non_blocking::WorkerGuard {
    if let Err(err) = std::fs::create_dir_all(dir) {
        eprintln!("创建日志目录失败({}): {err}", dir.display());
    }
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("rd-agent")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir)
        .expect("初始化滚动日志失败");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // 双写:滚动文件(托盘"打开日志目录")+ stdout(控制台启动时可实时观察,
    // 双击 exe 启动时无控制台则该层静默)。RUST_LOG=debug 可看更多细节。
    let file_layer = tracing_subscriber::fmt::layer().with_writer(writer);
    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stdout_layer)
        .init();
    guard
}

/// (仅 debug 构建)把 AgentPairing 的配对码"摇"成指定值,供端到端测试。
///
/// rd-security 出于安全不提供注入固定码的 API(生产码必须 CSPRNG 生成);
/// 这里反复 `new_code()` 直到命中目标值——6 位码期望约 10^6 次调用(毫秒级)。
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
