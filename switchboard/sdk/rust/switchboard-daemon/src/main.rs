//! Switchboard Daemon bin 壳（实现已 lib 化，rd-controller 内嵌同一实现）。
//!
//! 用法：`switchboard-daemon [--port 7500] [--bench-dir <dir>] [--static-dir <dir>]`
//! 端点见 lib 文档。rd-controller 启动时会内嵌同一服务（7500 端口单实例）。

fn main() -> anyhow::Result<()> {
    let mut port: u16 = 7500;
    let mut bench_dir: Option<String> = None;
    let mut static_dir: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--bench-dir" => bench_dir = args.next(),
            "--static-dir" => static_dir = args.next(),
            _ => {}
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(switchboard_daemon::serve(port, bench_dir, static_dir))
}
