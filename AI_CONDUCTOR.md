# AI_CONDUCTOR：窗口级远程控制（REQ-20261013-001）

你是本需求的执行 Orchestrator。设计已经完成，你的任务是按任务卡组织实现，不重新做关键设计。

## 1. 目标
- 一句话目标：个人用类 RustDesk 远控工具——Windows 被控端按窗口捕获与操控，控制端 1:1 自适应保证文字清晰，终端在控制端自绘。
- 成功指标：静止 300 ms 后文字区与源像素逐像素一致；端到端延迟 LAN P95 < 50 ms、tailnet 直连 P95 < 120 ms；终端持续输出 < 50 Kbps。
- 非目标（不要做）：文件传输、音频、整屏模式、UAC/登录界面、无人值守服务、多用户、自建中继、窗口缩略图、移动端触控（M2）、Web DOM 镜像（M3）、UIA 文字层（M4）。

## 2. 先读这些（按顺序，不要多读）
1. 本需求 `INDEX.md`
2. `01_discovery/需求分析.md` §2 用户故事
3. `03_architecture/架构方案.md` 摘要 + 与任务相关的小节
4. `12_tasks/00_任务总览.md`
5. 当前任务卡 + `13_handoff/上下文包.md` 中该任务一行

代码仓库根目录 `<RD_ROOT>`（[假设] `C:\code\rd`，D-008）；任务卡中的代码路径都相对它。规划文档路径相对本需求文件夹。

## 3. 技能与工具
| 技能/工具 | 用途 | 是否已确认可用 | 不可用时的替代做法 |
|---|---|---|---|
| Claude Code `Agent` 工具（子 Agent） | 按 §4 编制派发任务 | 已确认 | 单会话串行执行任务卡 |
| `/code-review` 技能 | 每张卡完成后审查 diff | 已确认 | Orchestrator 人工对照 DoD 审查 |
| `/security-review` 技能 | T-013、T-015、T-017 完成后安全审查 | 已确认 | 对照 11_security/安全方案.md 逐条检查 |
| Rust stable + `x86_64-pc-windows-msvc`、VS Build Tools | 构建 | 待确认（`rustc -V`、`rustup target list --installed`） | 按 rustup 官方安装 |
| Node.js ≥ 20 + pnpm | 控制端前端 | 待确认（`node -v`、`pnpm -v`） | `npm i -g pnpm` |
| Tauri 2 CLI | 控制端构建 | 待确认（`pnpm tauri -V`） | 作为 devDependency `@tauri-apps/cli` 安装 |
| ffmpeg LGPL shared 构建 | 硬编 | 待确认（`FFMPEG_DIR` 是否设置） | 仅用 OpenH264（T-003 已设计回退） |
| Tailscale（被控 PC + 控制端） | 网络 | 待确认（`tailscale status`） | `--listen` / `allow_lan` 局域网直连 |
| Mac + Xcode（免费 Apple ID） | M0 iPad Safari 验证、M2 iOS | 用户确认有 Mac，无付费账号 | Safari 测试页无需签名（D-006） |
| clumsy | 弱网模拟（T-027、T-026） | 待确认 | 跳过弱网用例，记录 RUN_LOG |

## 4. Agent 编制
| Agent | 负责的任务卡 | 拥有的文件/目录 |
|---|---|---|
| A0 Orchestrator | T-007、T-026；每卡验收 | 规划仓库 RUN_LOG/DECISIONS/INDEX；`tools/webcodecs-probe/`、`.github/workflows/release.yml`、`scripts/` |
| A1 基础设施与协议 | T-001、T-004、T-013 | 根配置、`proto/`、`crates/rd-proto`、`crates/rd-transport`、`crates/rd-security`、`ci.yml`（T-025 例外） |
| A2 捕获与编码 | T-002、T-003、T-010、T-011 | `crates/rd-capture-win`、`crates/rd-encode` |
| A3 Windows 系统层 | T-008、T-009、T-012、T-014 | `crates/rd-window-win`、`crates/rd-input-win`、`crates/rd-term` |
| A4 Agent 集成 | T-006、T-015、T-016、T-017、T-018、T-027 | `apps/rd-agent/`（按任务卡细分文件） |
| A5 控制端 | T-005、T-019～T-023 | `apps/rd-controller/`（按 07_frontend/控制端结构.md 细分） |
| A6 测试 | T-024、T-025 | `tools/clarity-bench/`、`apps/rd-agent/tests/e2e.rs`、`tests/common/` |

## 5. 执行顺序
见 `12_tasks/00_任务总览.md`。摘要：
- 第 1 批：T-001
- 第 2 批（并行）：T-002、T-003、T-004、T-024
- 第 3 批（并行）：T-005、T-006
- 第 4 批：**T-007 关卡——停下，向用户汇报 M0 数据，等待确认**
- 第 5 批（并行）：T-008、T-010、T-012、T-013、T-014
- 第 6 批（并行）：T-009、T-011、T-015、T-019
- 第 7 批（并行）：T-016、T-017、T-018、T-020
- 第 8 批（并行）：T-027、T-021
- 第 9 批（并行）：T-022、T-023、T-025
- 第 10 批：**T-026 验收——用户确认**

## 6. 协调规则
- 任务卡是唯一执行单位；只改任务卡列出的文件。
- 契约（`05_api/协议契约.md`、`proto/rd.proto`、`07_frontend/控制端结构.md`）有疑问时停止并上报，不要自行修改契约。
- 公共结构/桩由前置任务一次建好（T-001 模块空壳、T-005 命令桩、T-015 Agent 共享结构）；后续任务发现需要改这些文件 → 上报 Orchestrator，由 Orchestrator 决定是否插入小任务并记 DECISIONS。
- 每完成一个任务，在 `RUN_LOG.md` 追加：任务号、变更摘要、测试结果（命令 + 结果）、遗留问题。
- 每张卡完成后运行 `/code-review`；安全相关卡加 `/security-review`。
- 冲突交给 Orchestrator 处理，并记录到 `DECISIONS.md`。
- 提交规范：一卡一个分支/PR，提交信息 `T-0XX: <标题>`，便于按卡回滚。

## 7. 完成标准
- 所有任务卡的 DoD 都满足，`cargo test --workspace`、`pnpm -C apps/rd-controller test`、e2e 通过。
- US-01～06 验收标准逐条核对通过（T-026）。
- Release 产物可安装；可按 tag 回滚。

## 8. 禁止
- 重复读取未变化的文件、扫描整个仓库、读取 `target/`、`node_modules/`、锁文件、无关需求。
- 越界修改、擅自扩展需求（扩展记为“建议”交用户）。
- 链接 GPL 组件；日志记录按键、终端内容、配对码、私钥。
