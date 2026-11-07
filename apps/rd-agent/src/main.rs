// release 构建隐藏控制台窗口(小白双击即用,无黑框不闪退);日志已双写文件,
// stdout 层在无控制台时静默。debug/测试构建保留控制台(RD_LISTEN 解析依赖 stdout)。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! rd-agent — Windows 被控端（无头 bin 入口）。
//!
//! 核心逻辑已 lib 化（`rd_agent`，融合模式 rd-app 内嵌；D-013 F-1）；
//! 本 bin 保留无头被控模式：无参数（双击）等价 `run`，命令行子命令供测试
//! 与调试（`run --print-port`、`spike`）。托盘只属于 bin 模式。

mod spike;
mod tray;

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::prelude::*;

/// Windows 下提供 clap 子命令分发。`spike` 依赖 WGC 与 Win32 枚举,仅 Windows 可用;
/// `run` 的核心逻辑跨平台(托盘为 Windows 项)。
fn main() -> anyhow::Result<()> {
    // Per-Monitor-V2 DPI 感知(原 manifest,lib 化后改运行时声明;F-1):
    // 必须在任何 HWND 创建前调用;失败时沿用系统默认(混合缩放下坐标可能漂移)。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
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
    /// 覆盖监听地址(如 `127.0.0.1:0`);缺省按配置自动选择。
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

/// `run` 子命令入口:初始化日志 → lib `start` → 托盘/信号驻留。
fn run_app(args: RunArgs) -> anyhow::Result<()> {
    let config_dir = args
        .config_dir
        .clone()
        .unwrap_or_else(rd_agent::default_config_dir);
    // guard 必须存活到进程结束,否则非阻塞日志线程停摆
    let _log_guard = init_logging(&config_dir.join("logs"));
    // 崩溃上报（T-040）：panic hook → logs/crash/*.json（元数据，无内存转储）
    rd_crash::install_panic_hook(
        "rd-agent",
        env!("CARGO_PKG_VERSION"),
        rd_crash::crash_dir(&config_dir.join("logs")),
    );
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "rd-agent 启动");

    // 人为崩溃测试开关（T-040 验收）：RD_TEST_CRASH=1 时在 hook 安装后立即 panic，
    // 供验证 dump 落盘与托盘上报入口（生产环境无人设置，等价死代码路径）
    if std::env::var("RD_TEST_CRASH").as_deref() == Ok("1") {
        panic!("RD_TEST_CRASH 触发的测试崩溃（验证 dump 落盘）");
    }

    // tokio 跑在后台线程,主线程留给托盘/信号等待;
    // quinn Endpoint 的创建依赖 runtime 上下文,绑定须在 runtime 就绪后进行
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("创建 tokio runtime 失败")?;

    #[cfg(debug_assertions)]
    let test_pair_code = args.test_pair_code.clone();
    #[cfg(not(debug_assertions))]
    let test_pair_code = None::<String>;

    let runtime =
        rt.block_on(async { rd_agent::start(&config_dir, args.listen, test_pair_code) })?;
    let bound = runtime.bound.clone();

    if args.print_port {
        // 机器可读前缀:与彩色日志行区分,解析方按行前缀过滤
        for addr in &bound {
            println!("RD_LISTEN={addr}");
        }
        let _ = std::io::stdout().flush();
    }

    park_main_thread(&runtime, args.no_tray)?;
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

/// 主线程驻留:Windows 默认跑托盘(永不返回);`--no-tray` 或非 Windows 等待 Ctrl+C。
#[cfg(windows)]
fn park_main_thread(runtime: &rd_agent::AgentRuntime, no_tray: bool) -> anyhow::Result<()> {
    if no_tray {
        wait_ctrl_c()
    } else {
        tray::run(runtime)
    }
}

/// 非 Windows 平台无托盘:一律等待 Ctrl+C(保持可 check/可运行)。
#[cfg(not(windows))]
fn park_main_thread(runtime: &rd_agent::AgentRuntime, no_tray: bool) -> anyhow::Result<()> {
    let _ = (runtime, no_tray);
    wait_ctrl_c()
}

#[cfg_attr(windows, allow(dead_code))]
fn wait_ctrl_c() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(tokio::signal::ctrl_c())
        .context("等待 Ctrl+C 失败")?;
    tracing::info!("收到 Ctrl+C,退出");
    Ok(())
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
