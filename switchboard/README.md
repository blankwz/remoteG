# Switchboard（接线板）— 通用远控/低延迟流媒体模块规范

> **一句话**：把 RustDesk、Moonlight、Tailscale、Steam SDR、WebRTC、SRT……各家成熟的「打洞 / 中继 / 传输 / 编码」方案，抽象成**可插拔、可替换、可评测**的标准模块；用户像搭积木一样挑选组合，规范保证它们互相兼容、结果可比。

- **状态**：v0.1（草案，2026-10-24）
- **独立性**：本目录整体自洽，不依赖 rd 项目代码，可随时抽离为独立仓库。
- **语言**：规范性核心 = Rust trait SDK；跨语言绑定 = WIT（WebAssembly Component Model）；巨型现有栈 = process 绑定（外部进程适配）。

## 为什么需要它

同类项目各造一遍轮子，且**无法互换**：想用 RustDesk 的打洞就得接受它的整个客户端；想要 Moonlight 的 FEC 就得移植它的整个协议。根因是每家把「连接管理 + 传输语义 + 媒体适配 + 自适应控制」耦合在一个私有栈里。Switchboard 把这四件事拆成带能力协商（caps）的独立阶段，各家实现只需适配阶段接口，不必重写内部逻辑。

## 组成

| 部分 | 路径 | 说明 |
|---|---|---|
| 规范 v0.1 | [`spec/`](spec/00-总览与设计原则.md) | 7 篇：总览原则 / 阶段与端口契约 / 连接计划 DSL / 遥测契约 / 基准评分 / 接口绑定 / 合规等级 / 模块映射 |
| 原理卡片 | [`docs/cards/`](docs/cards/) | 从各家真实源码提炼的底层原理（Moonlight、RustDesk 等） |
| SDK | [`sdk/rust/`](sdk/rust/) | `switchboard-core`：trait + caps 协商 + 计划执行器 + 参考模块，带测试 |
| 基准 | `sdk/rust/switchboard-bench` | SB-Bench 命令行：跑负载 → 出指标 → 按 04 篇标准打分 → `bench.json` |
| 编排器 | [`studio/`](studio/) | Switchboard Studio：流程图拖拽换模块、caps 兼容校验、导出计划 |
| 注册表 | [`registry/`](registry/) | 模块清单（caps + 评分 + 出处 + 原理卡片链接） |

## 10 分钟上手

**完整操作手册（服务启动/参数含义/预期行为/已知边界）见 [USAGE.md](USAGE.md)。**

```bash
# 1) daemon（先启动——Studio 运行页的数据后端；仓库根目录执行）
cargo run --manifest-path switchboard/sdk/rust/Cargo.toml -p switchboard-daemon -- --port 7500 --bench-dir switchboard/registry/bench

# 2) Studio（编排 / 运行 / 基准 三页）
pnpm -C studio dev    # 浏览器 http://localhost:5173

# 3) 离线基准
cd sdk/rust && cargo run -p switchboard-bench -- --profile T3 --fast --out ../../registry/bench/t3.json
```

## 设计红线（为什么规范长这样）

1. **语义对齐，不是 API 对齐**：各家底层差异是本质的（Moonlight 的 FEC 绑定分片、QUIC 的流语义），规范用 caps 协商承认差异，而不是假装统一。
2. **遥测即公民**：不吐统一遥测的模块没有可比性——遥测是最低合规等级。
3. **计划与实现分离**：编辑器产出的是声明式连接计划（TOML），执行器把它映射到任何合规模块。换实现不换计划。
4. **先立标杆再铺开**：规范由「我们的 rd 栈 + 合成参考模块」先完整实例化验证，再逐家适配（每家一张原理卡片 + 一个 adapter）。

## 版本

- v0.1（2026-10-24）：阶段模型、Channel 契约、Plan DSL、遥测 v1、SB-Bench 评分标准、合规等级、参考模块注册。
- v0.1.1（2026-10-24 双专家评审修订）：caps 谓词语义重写；打洞铁律增 MTU 联动条款；Plan 增 role/retry/probe/预算切分；遥测增 loss 三分与 frame_rendered；评分锚点剖面条件化 + 回声标钟 + 突发丢包双口径；合规增负向探针；fec 参考实现修 4 缺陷（u16 分片/丢帧不吞新片/WouldBlock 弃帧/乱序窗）。
- v0.1.2（2026-10-24 真实适配器落地）：**sb.quinn.quic 升 L-B**（真实 QUIC datagram+流+TOFU 钉扎）；新增 **sb.moonlight.fec-rs**（Reed-Solomon + 动态比例 + 单片帧 200% 规则）；**W1 文字可读性**入基准（M5 T0 直读口径）；综合分对齐 04 篇全权重；**deploy/** 自建 DERP 套件（headscale+derper+caddy）+ tailscale 遥测桥；双 FEC A/B 基准产物入 registry/bench——XOR 赢 hover 带宽场景（84.7 vs 45.0）、RS 赢突发恢复（85.6 vs 67.6），权衡清晰可见。
- v0.1.3（2026-10-24 目标收尾）：**sb.kcp.kcp 升 L-B**（kcp 0.6 turbo）；新增 **sb.stun.punch-v2 L-B**（五条铁律参考实现 + RFC5389 子集 + 迷你 STUN 测试服务器）；quinn 指纹换 SHA-256（密码学合规）；W1 升级为字模渲染+最近邻识别（合成 OCR）；**T3 双口径 / T4 强制中继剖面**入 bench（--profile），T4 以 path.kind=relay 如实标注。
- v0.1.4（2026-10-24 运行态可视化）：新增 **switchboard-daemon**（合成会话引擎 + WS 遥测流 + HTTP API，axum）；Studio 三标签页——**编排**（原有）/ **运行**（参数表单 → 实时 FPS/带宽/RTT/丢失率曲线 + 事件流 + 路径健康摘要）/ **基准**（多 bench.json 加载，综合分+五分项柱状对比，兼容新旧两种报告格式）；vite 代理已配，daemon 启动：`cargo run --manifest-path switchboard/sdk/rust/Cargo.toml -p switchboard-daemon -- --port 7500 --bench-dir switchboard/registry/bench`。
