//! rd-pack-cli — 更新包发布端工具（改造项 A 发布端接线 + 改造项 B 镜像上传）。
//!
//! 子命令：
//! - `pack`：把 Agent 便携 zip 切块打 pack，产出 `<pack>.pack` + `update.json`
//!   （可选 `--sign-key` 对 update.json 签名，产出 `.sig`）。release.yml 在打
//!   完 zip 后调用。
//! - `keygen`：生成 ed25519 签名密钥对（seed hex 存文件，公钥打印）。
//!   密钥纪律（对齐 T-036）：只进凭据管理/CI secret，绝不进仓库。
//! - `mirror`：从 GitHub Releases 拉产物校验后补传国内对象存储（改造项 B），
//!   内容寻址：同 sha256 跳过、不同拒绝（防覆盖不同内容）。

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "rd-pack-cli", about = "更新包发布工具（pack/签名/镜像）")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 把 zip 切块打 pack（+ update.json，可选签名）
    Pack(PackArgs),
    /// 生成 ed25519 签名密钥对
    Keygen {
        /// seed 写入的文件（0600 语义由调用方保证；公钥打印到 stdout）
        #[arg(long)]
        out: PathBuf,
    },
    /// 国内渠道补传：从 GitHub Releases 拉产物 → 校验 → 上传 COS（改造项 B）
    Mirror(MirrorArgs),
}

#[derive(Args, Debug)]
struct PackArgs {
    /// 要切块的 zip（通常为 dist/rd-agent-<版本>-win64.zip）
    #[arg(long)]
    zip: PathBuf,
    /// 版本号（写入清单）
    #[arg(long)]
    version: String,
    /// 产出目录
    #[arg(long, default_value = "dist")]
    out_dir: PathBuf,
    /// 包文件名（默认 <zip 文件名去 .zip>.pack）
    #[arg(long)]
    pack_file: Option<String>,
    /// ed25519 seed hex（或含 seed hex 的文件路径）——签名 update.json
    #[arg(long)]
    sign_key: Option<String>,
}

#[derive(Args, Debug)]
struct MirrorArgs {
    /// GitHub Release tag（如 v0.3.0）
    #[arg(long)]
    tag: String,
    /// GitHub Releases 下载基址（默认本仓库 releases/download/<tag>）
    #[arg(long)]
    github_base: Option<String>,
    /// 要镜像的文件名清单（默认 rd-agent zip/pack/update.json 全套）
    #[arg(long, value_delimiter = ',')]
    files: Option<Vec<String>>,
    /// COS 配置（bucket、region、前缀），JSON 或「@文件」引用
    #[arg(long)]
    cos: String,
    /// COS secretId/secretKey，JSON 或「@文件」引用（绝不写进仓库/日志）
    #[arg(long)]
    cos_keys: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Pack(args) => cmd_pack(args),
        Commands::Keygen { out } => cmd_keygen(&out),
        Commands::Mirror(args) => rd_update_pack::mirror::cmd_mirror(
            &args.tag,
            args.github_base.as_deref(),
            args.files.as_deref(),
            &args.cos,
            &args.cos_keys,
        ),
    }
}

fn cmd_pack(args: PackArgs) -> anyhow::Result<()> {
    let zip_bytes =
        std::fs::read(&args.zip).with_context(|| format!("读 zip 失败: {}", args.zip.display()))?;
    let zip_name = args
        .zip
        .file_name()
        .and_then(|n| n.to_str())
        .context("zip 文件名非法")?;
    let pack_file = args.pack_file.unwrap_or_else(|| {
        zip_name
            .strip_suffix(".zip")
            .unwrap_or(zip_name)
            .to_string()
            + ".pack"
    });

    let chunker = rd_update_pack::Chunker::with_defaults();
    let (manifest, pack) =
        rd_update_pack::Manifest::make(&args.version, &pack_file, &zip_bytes, &chunker);

    std::fs::create_dir_all(&args.out_dir)?;
    let pack_path = args.out_dir.join(&pack_file);
    let json_path = args.out_dir.join("update.json");
    std::fs::write(&pack_path, &pack)?;
    let json = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(&json_path, &json)?;

    let mut signed = false;
    if let Some(key_spec) = &args.sign_key {
        let seed = load_key(key_spec)?;
        let sig = rd_update_pack::sign_bytes(&seed, &json)?;
        std::fs::write(args.out_dir.join("update.json.sig"), sig)?;
        signed = true;
    }

    println!("pack     : {}", pack_path.display());
    println!("update.json: {}", json_path.display());
    println!("version  : {}", manifest.version);
    println!("zip sha256: {}", manifest.pack_sha256);
    println!(
        "chunks   : {}（缺块更新对比基准；zip {} bytes → pack {} bytes）",
        manifest.chunks.len(),
        manifest.pack_size,
        pack.len()
    );
    println!("signed   : {signed}");
    Ok(())
}

fn cmd_keygen(out: &Path) -> anyhow::Result<()> {
    let (seed_hex, pub_hex) = rd_update_pack::generate_keypair();
    std::fs::write(out, seed_hex.as_bytes())?;
    println!("seed hex 已写入 {}", out.display());
    println!("pubkey hex（锚定到 Agent 端 RD_UPDATE_SIGN_PUBKEY 默认值）: {pub_hex}");
    println!("纪律：seed 只进凭据管理/CI secret，绝不进仓库。");
    Ok(())
}

/// 密钥参数：直接 hex，或 `@文件路径`。
fn load_key(spec: &str) -> anyhow::Result<String> {
    let raw = if let Some(path) = spec.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("读密钥文件失败: {path}"))?
    } else {
        spec.to_string()
    };
    let hex = raw.trim().to_string();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("签名密钥必须是 64 位 hex（ed25519 seed）");
    }
    Ok(hex)
}
