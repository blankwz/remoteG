//! 构建脚本:把 `rd-agent.manifest` 嵌入 Windows 可执行文件。
//!
//! Per-Monitor-V2 DPI 感知是进程级设置;`rd-input-win` 的坐标换算
//! (`ClientToScreen`/`GetClientRect`)依赖它返回物理像素(11_security、T-017)。
//!
//! 实现:生成标准 RC wrapper(`1 RT_MANIFEST "rd-agent.manifest"`)→
//! `rc.exe` 编译为 COFF 资源 → `rustc-link-lib` 链入可执行文件。
//! 借用 embed-resource 的 Windows SDK 探测(`find_windows_sdk_tool`)。
//!
//! **注意**:rc.exe 是 ANSI 程序,无法处理含非 ASCII(如中文)的路径;
//! 因此 rc 编译放在系统临时目录下以绝对路径引用,避免仓库路径含中文时构建失败。
//! 但最终 `.lib` 产物会复制进 `OUT_DIR` 并从那里链接:`OUT_DIR` 在 `target/` 内、
//! 归 cargo 指纹管理,不会像系统临时目录那样被独立清理(清理后指纹仍新鲜、
//! build 脚本不重跑,链接就会找不到输入文件)。

use std::path::Path;

fn main() {
    // 按 target 的 cfg 判断(而非 build script 自身的 host)
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }
    println!("cargo:rerun-if-changed=rd-agent.manifest");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let src = Path::new(&manifest_dir).join("rd-agent.manifest");

    // 工作目录取系统临时目录:绝大多数环境下为纯 ASCII 路径
    let work = std::env::temp_dir().join("rd-agent-manifest-build");
    std::fs::create_dir_all(&work).expect("创建临时资源目录失败");

    let manifest = work.join("rd-agent.manifest");
    std::fs::copy(&src, &manifest).expect("复制 rd-agent.manifest 失败");

    // RC 脚本字符串中反斜杠需转义;引用临时目录下的绝对路径
    let manifest_str = manifest.to_string_lossy().replace('\\', "\\\\");
    let rc = work.join("rd-agent-manifest.rc");
    std::fs::write(&rc, format!("1 24 \"{manifest_str}\"\n")).expect("写 wrapper rc 失败");

    let lib = work.join("rd-agent-manifest.lib");
    let rc_exe = embed_resource::find_windows_sdk_tool("rc.exe")
        .unwrap_or_else(|| std::path::PathBuf::from("rc.exe"));
    let status = std::process::Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&lib)
        .arg(&rc)
        .status()
        .expect("运行 rc.exe 失败(请确认已安装 Windows SDK)");
    if !status.success() {
        panic!("rc.exe 编译 rd-agent.manifest 失败({})", rc_exe.display());
    }

    // 产物入 OUT_DIR(cargo 管理);链接器从 OUT_DIR 找,不依赖临时目录存活
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::copy(&lib, Path::new(&out_dir).join("rd-agent-manifest.lib"))
        .expect("复制 manifest 资源库到 OUT_DIR 失败");

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=rd-agent-manifest");
}
