# rd — 窗口级远程控制 （半成品，未完成）

个人用类 RustDesk 远控工具：Windows 被控端按窗口捕获与操控，控制端 1:1 自适应保证文字清晰，终端在控制端自绘。

核心能力：QUIC 传输（quinn + BBR）、多路径竞速（反射打洞 / 直连候选 / rd-relay 中继 / EasyTier mesh / Tailscale）、文件传输、WOL 远程开机、剪贴板互传、隐私屏、录屏、画质四档、授权码商业化（Ed25519 离线验签）、崩溃上报、自动更新（控制端 Tauri updater；被控端缺块差分 + 自动检查，FastCDC 切块省流量）、手机比例虚拟屏（远控连入自动建屏/断开撤屏，触屏手势直控——轻点/惯性滚动/长按右键）。

## 构建与测试

```bash
cargo build --workspace          # 构建全部 crate
cargo test --workspace           # 全部测试
cargo clippy --workspace -- -D warnings
cargo fmt --all --check
```

要求：Rust stable（`x86_64-pc-windows-msvc`）+ VS Build Tools。共享 crate（协议/传输/编码/安全/中继/选路等）保持跨平台，CI 在 macOS 上对其逐一 `cargo check`；被控端与控制端是 Windows 专属产品，不参与 macOS 检查。

控制端 `apps/rd-controller/` 为独立 Cargo workspace（Tauri 2 + Node/pnpm），由根 workspace 排除（见 DECISIONS.md D-008）。

## 目录

| 路径 | 说明 |
|---|---|
| `proto/rd.proto` | 协议契约 v0（与规划文档 05_api §2 逐字段一致） |
| `crates/rd-proto` | prost 生成的协议消息 |
| `crates/rd-transport` | quinn 封装、证书、帧分流、多路径竞速 |
| `crates/rd-capture-win` | WGC per-HWND 窗口捕获 |
| `crates/rd-encode` | 编码器抽象 + H.264 + 无损瓦片 + 码控 |
| `crates/rd-input-win` | 输入注入（SendInput） |
| `crates/rd-window-win` | 窗口枚举/事件/分组/resize |
| `crates/rd-term` | ConPTY 终端会话 |
| `crates/rd-relay` | 原生中继（UDP 打标转发） |
| `crates/rd-switch` | Plan 执行器选路 |
| `crates/rd-security` | 配对、信任存储、授权码（Ed25519 离线验签） |
| `crates/rd-crash` | 崩溃上报（panic hook → JSON dump） |
| `crates/rd-update-pack` | 被控端自更新：FastCDC 切块 / pack 格式 / 验签 / COS 镜像（mirror feature） |
| `crates/rd-touch-translate` | 触控手势翻译（四相状态机 + 滚动惯性，零依赖纯逻辑） |
| `crates/rd-vdisplay` | 虚拟屏：手机比例屏建撤/换档（virtual-display-rs master 管道客户端） |
| `apps/rd-agent` | 被控端可执行程序 |
| `apps/rd-controller` | 控制端（Tauri 2） |
| `switchboard/` | 跨网方案编排器规范 + SDK（EasyTier/DERP/中继等可插拔模块）+ Studio |
| `website/` | 官网单页静态站（GitHub Pages） |
| `tools/clarity-bench` | 文字清晰度/延迟基准工具 |

架构与任务规划见需求文档：`03_architecture/架构方案.md` §2、`12_tasks/`。
