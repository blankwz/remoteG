//! 授权码签发工具（只在开发者机器上运行；**私钥绝不进仓库/产品**）。
//!
//! 用法：
//! ```text
//! # 开发/演示（内置开发种子；debug 构建可验签，**发布版会拒收**）：
//! cargo run -p rd-security --example issue_license -- --name "张三" --plan pro --devices 99
//!
//! # 正式签发（正式种子，2026-11-06 轮换后的唯一发布版可用种子）：
//! export RD_LICENSE_SEED=<正式种子 hex 64>   # 存密码管理器，绝不进仓库
//! cargo run -p rd-security --example issue_license -- --name "客户A" --plan pro --expires-days 365
//! ```
//!
//! 自验规则：debug 构建下开发种子码可通过自验（dev 验签兜底）；release 构建
//! 下自验失败即说明种子与内嵌正式公钥不配对——**发正式码务必用正式种子**。
//!
//! 输出授权码文本 → 存为 `<被控机 config_dir>/license.key`（默认
//! `%LOCALAPPDATA%\rd\license.key`），重启 Agent 生效。

use anyhow::Context as _;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(about = "rd 授权码签发")]
struct Args {
    /// 授权给谁（展示用）。
    #[arg(long, default_value = "")]
    name: String,
    /// 授权档位：pro / free。
    #[arg(long, default_value = "pro")]
    plan: String,
    /// 受信任设备数上限。
    #[arg(long, default_value_t = 99)]
    devices: u32,
    /// 有效期（天）；省略 = 永久。
    #[arg(long)]
    expires_days: Option<u64>,
    /// 覆盖签名种子（hex 64）；缺省读 RD_LICENSE_SEED，再缺省用开发种子。
    #[arg(long)]
    seed: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let seed = match (args.seed.clone(), std::env::var("RD_LICENSE_SEED").ok()) {
        (Some(s), _) => s,
        (None, Some(s)) => s,
        (None, None) => rd_security::license::dev_seed_hex(),
    };
    let plan = match args.plan.as_str() {
        "pro" => rd_security::license::Plan::Pro,
        "free" => rd_security::license::Plan::Free,
        other => anyhow::bail!("未知档位 {other}（pro/free）"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let license = rd_security::license::License {
        plan,
        name: args.name.clone(),
        devices_max: args.devices,
        expires: args.expires_days.map(|d| now + d * 86_400),
        issued: now,
    };
    let payload = serde_json::to_string(&license).context("序列化授权载荷")?;
    let code = rd_security::license::sign_payload(&payload, &seed)?;
    println!("{code}");
    // 自验一遍（签发即验，杜绝发出去才发现公钥不配对）
    let echo = rd_security::license::verify_license(&code, now)
        .context("签发后自验失败：种子与 LICENSE_PUBKEY_HEX 不配对？")?;
    eprintln!(
        "✓ plan={:?} devices={} name={} expires={:?}",
        echo.plan, echo.devices_max, echo.name, echo.expires
    );
    Ok(())
}
