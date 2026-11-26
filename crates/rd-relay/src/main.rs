//! rd-relay 服务端入口：`rd-relay [bind_addr]` 或 `rd-relay --listen <bind_addr>`（默认 0.0.0.0:21118）。
//!
//! 部署即拷这一个 exe（Linux/Windows 单文件、无依赖），systemd/schtasks
//! 拉起即可；不需要域名/证书/Docker（流量只是密文转发）。

use std::net::SocketAddr;

fn parse_bind_arg<I: IntoIterator<Item = String>>(args: I) -> anyhow::Result<SocketAddr> {
    let mut iter = args.into_iter().skip(1);
    let raw = match iter.next() {
        None => format!("0.0.0.0:{}", rd_relay::DEFAULT_PORT),
        Some(flag) if flag == "--listen" || flag == "-l" => iter
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} 缺少监听地址参数（示例 0.0.0.0:21118）"))?,
        Some(flag) if flag.starts_with("--listen=") => flag["--listen=".len()..].to_string(),
        Some(pos) => pos,
    };
    raw.parse()
        .map_err(|e| anyhow::anyhow!("监听地址格式错误（示例 0.0.0.0:21118）: {e}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let bind = parse_bind_arg(std::env::args())?;
    rd_relay::spawn(bind).await?;
    // spawn 已把收包循环放后台；主任务挂起即可（进程退出=服务退出）
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn parse_bind_supports_positional_and_listen_flag() {
        assert_eq!(
            parse_bind_arg(["rd-relay".into()]).unwrap(),
            "0.0.0.0:21118".parse().unwrap()
        );
        assert_eq!(
            parse_bind_arg(["rd-relay".into(), "0.0.0.0:21119".into()]).unwrap(),
            "0.0.0.0:21119".parse().unwrap()
        );
        assert_eq!(
            parse_bind_arg(["rd-relay".into(), "--listen".into(), "0.0.0.0:21120".into()]).unwrap(),
            "0.0.0.0:21120".parse().unwrap()
        );
        assert_eq!(
            parse_bind_arg(["rd-relay".into(), "--listen=127.0.0.1:21121".into()]).unwrap(),
            "127.0.0.1:21121".parse().unwrap()
        );
    }
}
