# Switchboard 使用指南（服务启动 + 相关重点）

> 面向操作的手册：怎么启动、每个参数什么意思、哪些行为是预期的、哪些还没验证。
> 规范与设计见 [README.md](README.md) 与 [spec/](spec/00-总览与设计原则.md)。

## 0. 组件速查

| 组件 | 位置 | 端口/产物 | 用途 |
|---|---|---|---|
| Studio（Web 可视化） | `switchboard/studio/dist/`（由 daemon 托管） | http://127.0.0.1:7500 | 方案测试（预设场景一键出分）、实时监控、跑分对比、高级编排 |
| Daemon（守护进程） | `switchboard/sdk/rust/switchboard-daemon/` | http://127.0.0.1:7500 | 合成会话引擎 + WS 遥测流 + bench 文件 API（Studio 的后端） |
| SB-Bench（CLI） | `switchboard/sdk/rust/switchboard-bench/` | bench.json | 离线跑分（T0/T3/T4 剖面 × 模块选择） |
| rd 开关 | rd 主程序 | 环境变量 | `RD_PLAN`（Plan 选路）、`RD_BBR=0`（Cubic 对比） |
| DERP 部署套件 | `switchboard/deploy/derper/` | VPS 上 | 自建中继（跨网 1.2s 根治） |

## 1. 启动方式

### 1.1 日常使用（一条命令）

**daemon 现在直接托管 Studio 界面**——启动后打开 http://127.0.0.1:7500 即是完整三页（方案测试/实时监控/跑分对比），无需 vite：

```bash
# 先构建一次界面产物（界面改动后才需重跑）
pnpm -C switchboard/studio build

# 一条命令启动（界面 + 数据同源）
cargo run --manifest-path switchboard/sdk/rust/Cargo.toml -p switchboard-daemon -- --port 7500 --bench-dir switchboard/registry/bench
```

### 1.2 前端开发模式（改界面代码时）

**顺序：先 daemon，后 Studio**（Studio 的 `/api` 由 vite 代理到 7500，含 WebSocket；热重载只对 5173 生效）。

```bash
# ① daemon（在仓库根目录执行）
cargo run --manifest-path switchboard/sdk/rust/Cargo.toml -p switchboard-daemon -- --port 7500 --bench-dir switchboard/registry/bench

# ② Studio（另开终端）
pnpm -C switchboard/studio dev
# 浏览器打开 http://localhost:5173
```

`.claude/launch.json` 已配好这两个服务（Claude Desktop 里可一键启动）。

**参数说明**：
- `--port 7500`：daemon 监听端口（与 vite 代理目标一致，改端口需同步改 `studio/vite.config.ts`）；
- `--bench-dir <目录>`：基准报告目录（默认产物在 `switchboard/registry/bench/`），不设则基准页只能本地上传。

### 1.2 生产/离线查看

```bash
pnpm -C switchboard/studio build   # 产物 switchboard/studio/dist/
```

`dist/` 是纯静态文件，但 **运行页需要 daemon 在 7500 端口**才有数据；只看编排/基准（本地上传 json）可双击打开。

## 2. Studio 三页用法与重点

**页面用语对照（界面已全部大白话）**：

| 界面词 | 技术含义 |
|---|---|
| 搭方案 / 实时监控 / 跑分对比 | 编排（组合模块）/ 运行（实时遥测）/ 基准（跑分对比） |
| 找到对方 | 发现阶段（LAN 广播/会合服务器找候选地址） |
| 穿墙直连 / 备用中转 | UDP 打洞 / 中继兜底 |
| 数据通道 / 画质保障 / 画面压缩 | 传输 / 媒体适配（FEC+码控）/ 编解码 |
| 抗丢包方案（RS / XOR） | FEC 纠错码实现：RS=Moonlight 同款（抗丢包强），XOR=极简参考（省带宽） |
| 自动修复丢包 / 丢掉的视频帧 | fec_recovered（分片重建成功）/ frame_lost（救不回来整帧丢弃） |
| 总分 / W1 文字清不清楚 … W7 打开多快 | SB-Bench 综合分与五个 workload 分项 |


### 2.1 编排页（默认）
- 左侧调色板拖六类阶段（发现/打洞/中继/传输/媒体适配/编解码）到画布；
- 节点下拉**即换实现**（`[L-B]` 可用 / `[L-A]` 登记桩，桩执行时诚实报不支持）；
- 连线组成候选路径；**红色连线 = caps 不兼容**（如 fec-shards 媒体接到了无 lossy datagram 的传输）；
- 「导出 Plan TOML」产物可直接喂给 `RD_PLAN` / bench。

### 2.2 方案测试页（默认页，推荐从这里开始）

**三步流程**：① 选一张方案卡片（场景用大白话写明）→ ② 点「测试这个方案」（约 10 秒）→ ③ 自动出分进排行榜。
「▶ 依次测试全部」会自动把 4 套方案逐个跑完并排名（约 1 分钟）。

预设四套方案（2026-10-24 实测分，本机演示网络）：

| 方案 | 场景 | 实测分 |
|---|---|---|
| A 家里局域网直连 | 同一 WiFi/路由器的两台电脑 | 82 |
| B 跨网帮家人（异地） | 两地宽带，直连优先+中转兜底 | 79 |
| C 弱网救急 | 酒店 WiFi/信号差、经常丢包 | 56 |
| D 极限弱网 | UDP 被限速的恶劣网络 | 27 |

卡片标注「开箱即用」的可直接测；真实环境的额外部署（中转服务器/穿墙会合/组网客户端）在页面底部「真实环境部署指引」和节点详情里有分步说明。

### 2.3 运行页（实时遥测，已并入方案测试页）
- 表单参数即弱网仿真：`RTT`/`丢包(0-1)`/`带宽(Mbps)`/`突发(包)`/`时长(s)`/`FEC 选择`；
- ▶ 开始后看到：路径/媒体摘要条 + **FPS/接收带宽/RTT/丢失率** 四条实时曲线 + 事件流；
- **预期行为**：丢包↑ → `frame_lost`/`frame_drop` 事件滚动、FPS 曲线下探；RS FEC 的 `fec_recovered` 表示分片被重建（帧没丢）；
- FEC 选择差异（实测 T3）：fec-xor 赢带宽受限场景（hover 84.6 vs 45.0），fec-rs 赢突发恢复（93.2 vs 80.9）——**开销换恢复**；
- 「停止」或时长到期后会话结束，WS 收到 `__end__`。

### 2.4 高级编排页（原画布，给懂行的人）

保留完整的六阶段拖拽画布与 Plan 导出；下拉框已改人话（如「QUIC 传输（推荐）」「自建 DERP 中转（需 VPS）」），选中需要部署的模块时，节点详情里直接显示分步部署指引。

### 2.5 基准页
- 下拉从 `--bench-dir` 加载，或本地上传 bench.json（可多选）；
- 上：综合分排名；下：W1 文字 / W2 hover / W3 视频 / W6 弱网恢复 / W7 启动 五分项分组柱状；
- 兼容两种报告格式（新 `passes[].runs[]` 取均值；旧 `workloads.{}`）；
- **口径提醒**：T0 是回归剖面（×0.9），**注册表评分以 T3/T4 实网为准**——对比同一剖面下的不同模块才有意义。

## 3. SB-Bench CLI

```bash
cd switchboard/sdk/rust

# T0 回归（快）
cargo run -p switchboard-bench -- --profile T0 --fast --out ../../registry/bench/t0.json

# T3 弱网双口径（均匀 + 突发各一轮，均值×0.9）——对 FEC 选型最有参考价值
cargo run -p switchboard-bench -- --profile T3 --fast --media sb.moonlight.fec-rs --out ../../registry/bench/rs-t3.json

# T4 强制中继（path.kind 如实标注 relay）
cargo run -p switchboard-bench -- --profile T4 --fast --out ../../registry/bench/t4.json
```

参数：`--rtt-ms 80 --loss 0.02 --mbps 10 --mtu 1200 --burst 5 --runs 3 --fast --media <id> --out <file>`。
注意：`--fast` 只跑 1 run 并缩短窗口（CI/演示用）；正式评分去掉 `--fast` 并 `--runs 3`。

## 4. rd 侧开关（主程序行为默认不变）

| 环境变量 | 效果 |
|---|---|
| `RD_PLAN=<plan.toml 路径>` | DeviceConn 走 Switchboard 执行器选路（LAN 发现/回退），失败自动回退默认直连 |
| `RD_BBR=0` | 拥塞控制从 BBR（默认，D-013 包5）临时切回 Cubic 做 A/B |

模板 Plan：`switchboard/registry/plans/family-help-v1.toml`（引用了 L-A 桩的路径会诚实失败并回退）。
**T-027（视频切 fec-shards）尚未合入**——任务卡见 `12_tasks/T-027-视频切fec-shards.md`，勿在 M1 手测窗口合入。

## 5. 自建 DERP（跨网根治，需 VPS）

见 [deploy/derper/README.md](deploy/derper/README.md)：compose 起 headscale+derper+caddy → 两台机器 `tailscale up --login-server` → `tailscale netcheck` 验证。预期中继 RTT 1.2s → 50~150ms。
登录后可跑 `python deploy/tailscale-bridge.py` 输出 StageStats（direct/relay 如实标注）。

## 6. 测试命令汇总

```bash
# rd 主仓库（仓库根目录）
cargo test --workspace --exclude rd-agent        # 快速全绿
cargo test -p rd-agent --test e2e -- --test-threads=1   # e2e 必须串行（8/8）

# switchboard（switchboard/sdk/rust 目录）
cargo test --workspace                            # 6 crate 36 测试

# switchboard studio
pnpm -C switchboard/studio build                  # tsc + vite
```

**已知注意**：
- rd-agent e2e 套件内编码器冷启动会使首帧变慢——视频等待已放宽 15s（首帧性能仍由 2s WARN 追踪，任务卡口径）；
- 多个 cargo 任务并行跑会争锁/互踩，等一个结束再跑下一个；
- 本机 tailscale 未登录时遥测桥无 peer 输出（代码路径已验证）。

## 7. 端口与目录速查

| 项 | 值 |
|---|---|
| Studio dev | 5173（vite 代理 `/api` → 7500） |
| Daemon | 7500（`--port` 可改） |
| rd agent QUIC | 21118（默认）；LAN 发现 21119；Switchboard LAN 发现 21121 |
| 基准产物 | `switchboard/registry/bench/*.json` |
| 模块注册表 | `switchboard/registry/index.json`（Studio/执行器共用） |
| 原理卡片 | `switchboard/docs/cards/*.md` |

## 8. 诚实边界（已测/未测）

- ✅ 单机回环 + 真实协议栈：全部 36+52 测试与 7 份 bench.json 覆盖（QUIC/KCP/STUN/FEC 均为真实协议互通，非 mock）；
- ❌ 跨机真实网络（LAN 真机广播、WAN 真打洞、DERP 中继、实网 T3/T4 跑分）：需第二台机器与 VPS，代码与套件就绪；
- ⚠️ quinn 指纹为 SHA-256（密码学合规）；`sb.stun.punch-v2` 需要 Rendezvous 通道注入（宿主接口已留）。
