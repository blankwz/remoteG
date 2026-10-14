//! rd-agent — Windows 被控端（M0 Spike 管线见 `spike` 子命令；正式功能自 T-015 起）。

mod config;
mod server;
mod session;
mod term_handler;

#[cfg(windows)]
mod input_handler;
#[cfg(windows)]
mod spike;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod window_session;

/// Windows 下提供 clap 子命令分发。`spike` 依赖 WGC 与 Win32 枚举，仅 Windows 可用。
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use clap::{Parser, Subcommand};

    #[derive(Parser, Debug)]
    #[command(name = "rd-agent", about = "窗口级远程控制被控端")]
    struct Cli {
        #[command(subcommand)]
        command: Commands,
    }

    #[derive(Subcommand, Debug)]
    enum Commands {
        /// M0 Spike：捕获指定窗口 → H.264 编码 → QUIC 视频流发给首个连接的客户端
        Spike(spike::SpikeArgs),
    }

    let cli = Cli::parse();
    match cli.command {
        Commands::Spike(args) => spike::run(args),
    }
}

/// 非 Windows 平台仅保留脚手架占位（spike 依赖 WGC / Windows 专属 API，保持跨平台可 check）。
#[cfg(not(windows))]
fn main() {
    println!("rd-agent scaffold (T-001)：spike 管线仅支持 Windows");
}
