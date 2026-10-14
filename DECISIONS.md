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

## D-009 补全 `CloseWindow` 消息定义（已确认 2026-10-14）
- 现象：05_api/协议契约.md §2 的 `Envelope` oneof 引用 `CloseWindow close_window = 15`，但全文未定义 `message CloseWindow`，proto 语法上无法编译（非语义歧义，属契约笔误）。
- 决定：按与 `OpenWindow` 对称的最小语义补全 `message CloseWindow { uint64 window_id = 1; }`（proto/rd.proto 已含）；05_api 文档本身未改动。
- 状态：**用户已确认**（2026-10-14）。

## D-010 M0 结论：D-003 维持 Tauri 2 + WebCodecs（用户确认 2026-10-14）
- 数据（自动实测）：Win 本机 WebView2 支持 H.264 Baseline/High/4:4:4/AV1，184 帧 1080p 解码吞吐 769 fps；iPhone Safari 18.7（iOS 18.7）支持 Baseline/High/HEVC，吞吐 580 fps，**4:4:4 与 AV1 不支持**；编码 h264_mf 1080p 3.7-4.0ms、OpenH264 12-17ms；WGC 读回 avg 1.16ms；Spike 全链路回环通过。
- 结论：**D-003 维持方案 A**（Tauri + WebView + WebCodecs），不切换 D-003 方案 B（原生 wgpu 视频层）。D-004 路径验证成立：文字清晰不依赖 4:4:4（iPhone 无 4:4:4），静止无损瓦片为主，4:4:4 仅作能力协商增强（Win 端可启用、iPhone 端自动关闭）。
- 待回填：场景 A/B/C 端到端延迟数据（用户稍后补充，补齐前 LAN P95<50ms / tailnet P95<120ms 目标未正式关闭）。
- 已知风险移交 M1：WebView2 中途 flush 死锁（流式场景预计不触发）；h264_mf 起步延迟大（T-016 管线优化）；CapturedFrame.resized 信号失效（T-016 前修复 rd-capture-win）。
