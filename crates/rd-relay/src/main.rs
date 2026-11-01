//! rd-relay 服务端入口：`rd-relay [bind_addr]`（默认 0.0.0.0:21118）。
//!
//! 部署即拷这一个 exe（Linux/Windows 单文件、无依赖），systemd/schtasks
//! 拉起即可；不需要域名/证书/Docker（流量只是密文转发）。

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let bind: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("0.0.0.0:{}", rd_relay::DEFAULT_PORT))
        .parse()
        .map_err(|e| anyhow::anyhow!("监听地址格式错误（示例 0.0.0.0:21118）: {e}"))?;
    rd_relay::spawn(bind).await?;
    // spawn 已把收包循环放后台；主任务挂起即可（进程退出=服务退出）
    std::future::pending::<()>().await;
    Ok(())
}
