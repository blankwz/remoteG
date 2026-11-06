//! 从签名种子派生公钥（只在开发者机器上运行；**种子本身绝不打印/不进仓库**）。
//!
//! 用途：密钥轮换 SOP 的第 2 步——`openssl rand -hex 32` 出种子后，
//! 用本工具派生公钥 hex，替换 `license.rs` 的 [`LICENSE_PUBKEY_HEX`]。
//!
//! 用法：
//! ```text
//! export RD_LICENSE_SEED=$(openssl rand -hex 32)
//! cargo run -p rd-security --example show_pubkey
//! # 或 --seed <hex 64>（用完记得清 shell 历史）
//! ```

use clap::Parser;

#[derive(Parser, Debug)]
#[command(about = "rd 授权签名公钥派生（种子→公钥，单向）")]
struct Args {
    /// 签名种子（hex 64）；缺省读 RD_LICENSE_SEED。
    #[arg(long)]
    seed: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let seed = match (std::env::var("RD_LICENSE_SEED").ok(), Args::parse().seed) {
        (_, Some(s)) => s,
        (Some(s), None) => s,
        (None, None) => anyhow::bail!("缺少种子：设 RD_LICENSE_SEED 或传 --seed <hex 64>"),
    };
    // 只派生公钥（VerifyingKey），seed 用完即弃，不落日志。
    let vk = rd_security::license::pubkey_hex_from_seed(&seed)?;
    println!("{vk}");
    Ok(())
}
