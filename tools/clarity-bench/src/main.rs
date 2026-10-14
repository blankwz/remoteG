//! clarity-bench — 文字清晰度/延迟基准工具（T-024）。
//!
//! 子命令：
//! - `testwin`  被控端测试窗口（static / scroll / clock，PerMonitorV2 + GDI）
//! - `capture`  抓取指定标题窗口客户区为 PNG（等待 T-002 rd-capture-win，当前为桩）
//! - `compare`  按定位标记对齐并比对两张 PNG，输出一致率/PSNR/最大通道误差

mod compare;

#[cfg(windows)]
mod testwin;

use anyhow::{bail, Result};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = dispatch(&args) {
        eprintln!("clarity-bench: {e:#}");
        std::process::exit(2);
    }
}

fn dispatch(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None => {
            print_usage();
            bail!("未指定子命令");
        }
        Some("testwin") => run_testwin(&args[1..]),
        Some("capture") => run_capture(&args[1..]),
        Some("compare") => compare::run_cli(&args[1..]),
        Some("--help" | "-h" | "help") => {
            print_usage();
            Ok(())
        }
        Some(other) => bail!("未知子命令: {other}"),
    }
}

#[cfg(windows)]
fn run_testwin(args: &[String]) -> Result<()> {
    testwin::run(testwin::TestwinOptions::parse(args)?)
}

#[cfg(not(windows))]
fn run_testwin(_args: &[String]) -> Result<()> {
    bail!("testwin 仅支持 Windows")
}

/// capture 子命令：等待 T-002 的 `rd-capture-win` 提供抓帧能力后接入。
///
/// 接入方式：在本函数中把窗口标题解析为 HWND，调用 rd-capture-win 抓取
/// 客户区位图，再复用 `compare::save_png_rgb` 写出。CLI 形态保持不变。
fn run_capture(args: &[String]) -> Result<()> {
    if args.len() != 2 {
        bail!("用法: clarity-bench capture <窗口标题> <out.png>");
    }
    bail!(
        "capture 未实现：等待 rd-capture-win（任务 T-002）完成后接入（目标窗口: {}，输出: {}）",
        args[0],
        args[1]
    )
}

fn print_usage() {
    println!(
        "clarity-bench — 文字清晰度/延迟基准工具\n\
         \n\
         用法:\n\
         \x20 clarity-bench testwin [--mode static|scroll|clock] [--size WxH]\n\
         \x20 clarity-bench capture <窗口标题> <out.png>\n\
         \x20 clarity-bench compare <source.png> <received.png> [--json] [--text-region x,y,w,h]\n\
         \n\
         示例:\n\
         \x20 clarity-bench testwin --mode clock --size 1280x800\n\
         \x20 clarity-bench compare source.png received.png --json\n\
         详细测量步骤见 tools/clarity-bench/README.md"
    );
}
