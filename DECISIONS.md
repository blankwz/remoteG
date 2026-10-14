# 决策记录

## D-001 自研，不 fork RustDesk（已确认 2026-10-13）
- 原因：RustDesk 为 AGPL-3.0；窗口级 + 语义渲染架构与其整屏架构差异大，fork 收益有限。
- 影响：仅参考其设计（虚拟显示器、输入处理思路），不复制代码。

## D-002 网络层：Tailscale 优先，自建中继延后（已确认 2026-10-13）
- 方案 A：MVP 只用 Tailscale（寻址/穿透/DERP 中继），应用层 QUIC + 证书钉扎。成本 0、运维 0；依赖第三方账号。
- 方案 B：MVP 同时自建 rendezvous + relay。多 2～3 周工作量 + 一台公网服务器。
- 决定 A；协议保留“直连地址可配”，将来接自建中继不改协议。

## D-003 控制端用 Tauri 2（已确认 2026-10-13，M0 Spike 复核）
- 方案 A：Tauri 2（WebView）。终端（xterm.js）和 M3 DOM 回放天然契合，一套代码覆盖 iOS。风险：WebCodecs 性能/4:4:4 支持、IPC 大帧开销。
- 方案 B：原生 Rust（wgpu + egui）。视频路径最优；终端需自绘、DOM 回放仍要嵌 WebView、iOS 支持弱。
- 决定 A；T-007 若 Spike 不达标，视频层改为 Tauri 原生子窗口（wgpu）渲染，其余保留 WebView。

## D-004 文字清晰度不依赖 4:4:4 编码（已确认 2026-10-13）
- 1:1 适配消除缩放 + 静止区无损瓦片精修为主，4:4:4 作为能力协商后的增强。

## D-005 终端模式只接管 Agent 自己启动的会话（已确认 2026-10-13）
- 已存在的终端窗口在 M1 按普通窗口（L0）处理；读取现有 Windows Terminal 缓冲区（UIA）放 M4。

## D-006 iOS 签名：先用免费 Apple ID（已确认 2026-10-13）
- 现状：有 Mac，无付费开发者账号。
- 免费 Apple ID 可经 Xcode 安装到自己的 iPad/iPhone，签名约 7 天过期需重装，无 TestFlight。
- M0 的 iPad 验证用 Safari 打开测试页（与 WKWebView 同引擎），无需签名。
- M2 若每周重装不可接受，再购买 Apple Developer Program（99 美元/年）—— 交用户决定。

## D-007 控制端前端：React + TypeScript + Vite（默认采用，T-005 开始前可改）
- 理由：AI 与社区资料最多，xterm.js/rrweb 均有现成集成。
- 替代：Svelte（更轻，资料少）、Solid。

## D-008 代码仓库与规划仓库分离（默认采用）
- 代码仓库根目录记为 `<RD_ROOT>`，[假设] 为 `C:\code\rd`；任务卡中所有路径相对 `<RD_ROOT>`。
- 控制端 `apps/rd-controller/src-tauri` 为独立 Cargo workspace（通过 path 依赖引用 crates），根 workspace `exclude` 它，避免并行任务同时改根 `Cargo.toml`。

## D-009 补全 `CloseWindow` 消息定义（T-001 执行中，待用户确认）
- 现象：05_api/协议契约.md §2 的 `Envelope` oneof 引用 `CloseWindow close_window = 15`，但全文未定义 `message CloseWindow`，proto 语法上无法编译（非语义歧义，属契约笔误）。
- 决定：按与 `OpenWindow` 对称的最小语义补全 `message CloseWindow { uint64 window_id = 1; }`（proto/rd.proto 已含），已同步标注本条记录；**05_api 文档本身未改动**。
- 待确认：若用户希望 CloseWindow 携带其他字段（如关闭原因），在 T-015 冻结协议前修改即可。
