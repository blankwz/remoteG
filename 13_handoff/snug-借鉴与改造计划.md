# snug 借鉴细节与改造计划（交接文档）

> 生成：2026-11-18。来源：`/learn-project gh zephyr4123/snug` 学习产出。
> 用途：**交给其他 AI 执行改造**。每项细节附 snug 源码证据（file:line）与 remoteG 落点。
> snug 源码已归档：`C:\Users\blank\claude-learn-done\zephyr4123-snug`（下文证据路径均以此相对路径给出，缩写 `<snug>/`）。
> 学习档案（11 件 + 图谱）：`C:\Users\blank\.claude\learned-projects\zephyr4123-snug\`；5 张功能卡在 `...\learned-projects\cards\`（content-chunk-update / gesture-translation / virtual-display-management / window-move-restore / connection-state-tracking）。

## 0. snug 是什么（30 秒）

macOS 菜单栏应用（Swift 6，MIT），为网易 UU远程 补一块「虚拟手机屏」：用 `CGVirtualDisplay` 私有接口创建竖向虚拟显示器（比例按用户手机机型计算），Mac 窗口移入铺满整屏，手机上获得原生级远程操作体验。核心四件套：**虚拟屏 + 连接判定 + 手势翻译 + 崩溃可还原**，外加 **FastCDC 增量更新**。质量极高：所有魔法数字都有实测依据，全部纯逻辑可单测（`<snug>/Sources/SnugCore` 不碰系统 API）。

三层架构（`<snug>/Package.swift:19-40`）：SnugCore（纯逻辑）→ SnugSystem（系统接口，实现 SnugCore 协议）→ Snug（应用层）。**这个「Core 纯逻辑 + 协议切面」的组织方式本身就是可借鉴项**——remoteG 的 crates 划分已经同构，可对照校验边界。

---

## 1. 借鉴细节总表（按 remoteG 优先级）

| # | 细节 | snug 证据 | remoteG 落点 | 优先级 |
|---|---|---|---|---|
| A1 | FastCDC 内容切块（gear 表固定种子，切点只看内容） | `<snug>/Sources/SnugCore/Update/Chunker.swift:11-13`（min 32Ki/avg 128Ki/max 512Ki）、`:14`（gear=SHA256("snug-gear-i") 固定种子派生）、`:38-49`（cutLength 掩码逻辑） | 新 crate 发布端+Agent 端共用 | **P0** |
| A2 | 更新包格式：块清单（每块 sha256+offset+length、每文件由哪几块组成），任意旧版缺块直升 | `<snug>/Sources/SnugCore/Update/UpdatePackage.swift:5`（注释：不挑从哪个版本升）、`:15-24`（Chunk 结构）、`:43`（file→chunks 映射） | `rd-proto`/发布产物格式 | **P0** |
| A3 | 更新编排：启动查一次不轮询、失败保守（已装版本原样保留+提示手动）、签名校验、原子对调 | `<snug>/Sources/Snug/Update/Updater.swift:89`（check）、`:127-165`（install 全序）、`:178-186`（错误分类→手动下载兜底）；`<snug>/Sources/Snug/Update/AppBundle.swift`（swap+签名核对） | T-038 tauri-updater 的增量补充 | **P0** |
| B1 | 双渠道发布：GitHub + 国内 COS，同 sha256 校验，对象级内容寻址跳过重复上传 | `<snug>/docs/development.md`「国内源」节（缓存策略表：版本件 1 年 / latest 5 分钟 / 只向新版本切换） | website/ + release.yml 后置 | **P0** |
| B2 | 发布可中断重跑：同 commit 产物复用、已上传且一致跳过 | `<snug>/docs/development.md`「发布流程」、`<snug>/scripts/snug`（ship/mirror 子命令，621 行 shell） | CI release.yml 与 scripts/ | **P0** |
| C1 | 手势翻译状态机（四相：idle→pending→scrolling/selecting） | `<snug>/Sources/SnugCore/Touch/TouchTranslator.swift:101-107`（Phase）、`:91-99`（阈值：slop=10pt、holdDuration=0.5s、flingWindow=0.1s、minFlingSpan=0.04s、maxFlingSpeed=4000） | 移动端控制端（未来） | **P1** |
| C2 | 滚动物理：小数余量累计、惯性每帧×0.95 衰减、启动门槛 200pt/s、停止线 40pt/s、估速样本 ≥0.04s 防网络扎堆 | `<snug>/Sources/SnugCore/Touch/Scrolling.swift:30-43`（ScrollWheel）、`:46-76`（Momentum） | 同上 | **P1** |
| C3 | 状态机与事件线程分离、补发事件带防环标记、阈值乘缩放倍数 | `<snug>/Sources/SnugCore/Touch/TouchTranslator.swift:1-12`（头注释设计说明）、`<snug>/Sources/SnugSystem/TouchTap.swift`（宿主半） | 移动端输入管线 | **P1** |
| D1 | 多档同比例分辨率模式：换档不重推流、点击坐标保持准确；按窗口实测选档（先铺再读实际尺寸→选最窄够用档） | `<snug>/Sources/SnugCore/PhoneScreen/ScreenFit.swift:13-29`（全部常数）、`:72-78`（width(toFit:) 先乘后除防浮点误进档）；`<snug>/docs/how-it-works.md`「屏幕适配」节 | Windows IDD 预研 + rd-encode/rd-proto | **P1（预研）** |
| D2 | App 资源库 = 窗口级远程的入口：运行中/全部/搜索三列表，窗口枚举不分屏幕空间 | `<snug>/docs/how-it-works.md`「App 资源库」节、`<snug>/Sources/Snug/PhoneScreen/AppCatalog.swift`、`<snug>/Sources/SnugCore/PhoneScreen/Apps.swift` | rd-window-win + 控制端窗口列表（衔接 proto v0.2 候选 WindowEvent/FitApplied） | **P1（预研）** |
| E1 | 窗口记账还原：改前先写账、还原成功才销账、写不进不动手、崩溃后 recover() | `<snug>/Sources/SnugCore/PhoneScreen/PhoneDesk.swift:1-13`（头注释）、`<snug>/Sources/SnugCore/Persistence/JSONFile.swift:35`（原子写 `.atomic`）、`<snug>/docs/how-it-works.md`「还原记录」节 | rd-window-win / F9 隐私屏 / F4 侧边栏 | **P2** |
| E2 | 留下账（LeftBehind）：视野外被销毁的资源转账保留（两周），带着被改状态回来再还原 | `<snug>/Sources/SnugCore/PhoneScreen/LeftBehind.swift`、`<snug>/docs/how-it-works.md`「在 Snug 之外关闭的窗口」段 | 同上（窗口被第三方关闭场景） | **P2** |
| E3 | 自扰静默期：自己引起拓扑扰动前先静默，防信号扰动被误判 | `<snug>/Sources/SnugCore/Session/ConnectionTracker.swift:18-19`（1.5s 去抖/5s 静默）、`:31-35`（noteSelfInducedChange 取 max） | rd-agent 会话保活/心跳判定 | **P2** |
| E4 | 中途启动回看日志恢复状态（回看窗口实测 4 小时≈2 秒；回看完成前状态报「未知」而非猜） | `<snug>/Sources/Snug/Sensing/CaptureLogWatcher.swift:2-5`（头注释）、`:15`（replayWindow）、`:44`（未知态显式建模） | rd-agent 服务重启后的会话恢复 | **P2** |
| E5 | 辅助功能/外部 API 调用全局 1s 超时（系统默认 6s，个别 App 卡住不拖慢整体）；存在性查询与内容操作分离 | `<snug>/Sources/SnugSystem/AX.swift:2`、`:13`、`:22`（AXUIElementSetMessagingTimeout）；`<snug>/Sources/SnugCore/PhoneScreen/PhoneServices.swift:53-86`（WindowControl 协议：exists/isShown 问窗口服务器，move/close 走 AX） | rd-window-win（Win32/UIA 对应物：存在性用 IsWindow/EnumWindows，操作用 SetWindowPos） | **P2** |
| E6 | 窗口移动核对重试：size→pos→size→核对，不符重来（App 跨屏夹紧尺寸） | `<snug>/Sources/SnugSystem/WindowService.swift:12-13`（retryDelay 0.25s）、`:249-257` | rd-window-win | **P2** |
| E7 | 机型/枚举类数据单一 JSON 数据源 + 测试逐条校验（竖握书写/无重复/字段齐全） | `<snug>/Sources/SnugCore/PhoneScreen/PhoneModels.swift:2`（头注释）、`<snug>/Assets/phone-models.json`（800 机型）、`<snug>/AppController.swift:65-67`（打包缺失兜底） | 设备配置/码表类数据组织 | **P2** |
| E8 | 多控制端「记集合不记第一个」（表格顺序是连接顺序不代表谁在用） | `<snug>/Sources/SnugCore/Session/ConnectionTracker.swift:7-9`（头注释 issue #3） | 未来多查看端场景 | **P2** |

---

## 2. 详细改造计划

> 每个改造项独立成任务卡（建议放 `12_tasks/`）。标注【决策点】处需要人拍板或改造 AI 给出推荐后确认。

### 改造项 A（P0）：FastCDC 缺块增量更新

**目标**：Agent 便携 zip 与控制端安装包支持「任意旧版本 → 最新版，只下载缺块」。补齐 T-038（tauri-plugin-updater 整包下载）的带宽短板，国内用户收益最大。

**现状差距**：T-038 已接 tauri-plugin-updater（conf 公钥/端点占位/passive 安装），但下载粒度是整包；`release.yml` 产物为整包 zip/nsis。

**改造步骤**：
1. 新 crate `rd-update-pack`（rd workspace）：
   - FastCDC 切块器：Rust 生态有现成 `fastcdc` crate 可直接用；若自实现，照 `<snug>/Chunker.swift:14` 的「固定种子派生 gear 表」做法——**关键不变量：发布端与客户端必须同一份切块代码**（snug 用 SnugCore 共享，remoteG 用 rd-update-pack 同时被发布工具与 Agent 依赖，同构）。
   - pack 格式：`{version, chunks:[{sha256, offset, length}], files:[{path, chunks:[i…]}]}`（对照 A2 证据），逐块 zstd 压缩后首尾相接；`update.json` 随产物发布。
   - 单测：改一处只影响附近块 / 内容挪位块可识别 / 空文件 / 大文件边界。
2. Agent 端自更新执行器（`apps/rd-agent` 内新模块）：
   - 盘点：对当前安装 zip 同法切块，按 sha256 对比清单得缺块集。
   - 下载：HTTP Range 逐块拉取（复用 rd-transport 的 HTTP 客户端能力或 reqwest），逐块 sha256 校验，失败重试 N 次后整体放弃。
   - 拼装+校验：拼出新 zip 全量校验 →（Windows）校验 Authenticode 签名或复用 T-036 的 LICENSE_PUBKEY_HEX 锚定思路校验发布清单签名。
   - 原子对调：临时目录拼装 → 校验通过 → rename 对调 → 重启 Agent。**失败路径全保守：任何一步失败，已装版本原样保留 + 通知用户手动下载**（对照 A3 证据 Updater.swift:178-186 的错误分类）。
3. 发布端接线：`release.yml` 在现有产物后追加 pack+update.json 生成与上传；[决策点] 更新端点与 T-038 的 updater 端点合一还是分立（推荐合一，双通道并存灰度：环境变量 `RD_UPDATE_MODE=chunked|full`，验证若干版本后 chunked 转默认）。
4. 手测清单补项（09_test/）：断点续传、坏块注入、旧版本矩阵（至少 v0.1 zip → 最新）、磁盘只读路径失败回退。

**验收标准**：单测覆盖切块/清单/拼装；本地双版本模拟升级只走缺块请求；失败注入后旧版可正常启动；fmt/clippy/全量测试绿；e2e 不受影响。

**参考代码清单**（改造 AI 先读）：
- `<snug>/Sources/SnugCore/Update/Chunker.swift`（全文 55 行，逻辑.md 机制 5 有语言无关五要素）
- `<snug>/Sources/SnugCore/Update/UpdatePackage.swift`（清单结构与拼装）
- `<snug>/Sources/SnugCore/Update/UpdateFeed.swift`（update.json 模型与更新源）
- `<snug>/Sources/Snug/Update/Updater.swift:89-186`（编排+保守失败）
- `<snug>/Sources/Snug/Update/AppBundle.swift`（对调与签名核对——Windows 侧需换 Authenticode/清单签名）
- `<snug>/Sources/SnugRelease/main.swift`（发布端 CLI：打包与清单生成）
- `<snug>/docs/development.md`「应用内更新」节（双文件产物约定）
- 卡：`~/.claude/learned-projects/cards/content-chunk-update/`（card.md 方案对比 + patterns/swift-content-chunk-update.json 的 adaptationNotes 已列 Windows 改造点）

### 改造项 B（P0）：国内渠道发布与可中断重跑

**目标**：GitHub Releases 之外增加国内对象存储渠道（腾讯云 COS+CDN 或等价），发布/补传可中断重跑，内容寻址不重传。

**现状**：`website/`（Pages）+ `release.yml`（tag v* 挂 GitHub Release）；国内用户直连 GitHub 慢。

**改造步骤**：
1. `scripts/` 新增上传脚本（PowerShell 或 Python，参考 snug 用 Python+锁定依赖）：对象路径 `v<版本>/<产物>`、内容 sha256 相同即跳过、不同即拒绝（对照 B1 证据的 COS 约定）。
2. `latest/` 指针对象只向更新版本切换（防回滚覆盖），下载文件名带版本号。
3. `release.yml` 后置 job：上传产物 + 更新 latest；幂等（重跑安全）。
4. `scripts/rd-mirror.ps1`：从 GitHub 拉取校验后补传国内（对应 B2 的 mirror）。
5. 密钥管理走 `.machine.env` 同款思路——**只记名称不存密钥**（`<snug>/.machine.env.example`），实际密钥进凭据管理（对齐 T-036 种子管理纪律）。

**验收**：模拟中断后重跑零重复上传；latest 指针不可回退；两渠道 sha256 一致性自动核对。

**参考代码清单**：`<snug>/scripts/snug`（ship/dist/mirror/media 子命令段）、`<snug>/docs/development.md`「国内源」「发布流程」两节、`<snug>/.machine.env.example`。

### 改造项 C（P1）：手势翻译（移动端控制端前置，建议先出模块骨架）

**目标**：为未来手机控制端准备「触摸语义 → 远控事件」的翻译模块与协议约定。

**【架构决策点——与 snug 相反的落位】**：snug 必须在**被控端**翻译，因为它改不了 UU 手机端（UU 把单指拖动发成鼠标拖拽）。remoteG 两端都是自己的，**翻译应放在控制端（手机侧）**：手机端把手势识别为 scroll/right-click/drag-select，直接经 rd-proto 发对应事件，被控端（rd-input-win）零改动。改造 AI 请按此设计，不要照搬 snug 的被控端 CGEvent tap 架构；snug 的价值在**状态机本身与全部实测常数**。

**改造步骤**：
1. 新 crate `rd-touch-translate`（纯逻辑：`#[derive]` 状态机 + `now` 由调用方注入，零 I/O 依赖——对照 snug 的 Core 组织方式）：
   - 四相状态机 + 6 种输出命令 + 防环标记 + region 缩放（证据 C1/C3）。
   - 惯性帧生成器（60fps 调用方驱动；证据 C2）。先乘后除等浮点细节照抄 `ScreenFit.swift:72` 注释。
2. 协议侧（`rd-proto`）：确认现有输入事件流可表达「带增量滚动的滚动事件 + 相位标记」；若只有离散滚轮事件，加 continuous-scroll 变体（tag 分配遵照 05_api 现有分配表）。
3. 单测：全阈值边界（10pt 恰好临界、0.5s 恰好临界、假速度防护用「事件扎堆 5ms」样例）。
4. 不接 UI——移动端 UI 属未来项目，本项只交付可复用核心 + 协议预留。

**验收**：单测覆盖状态机全部迁移路径；常数集中为具名常量并注明 snug 实测出处；`cargo test -p rd-touch-translate` 绿。

**参考代码清单**：`<snug>/Sources/SnugCore/Touch/TouchTranslator.swift`（全文 236 行）、`<snug>/Sources/SnugCore/Touch/Scrolling.swift`（全文 76 行）、`<snug>/docs/how-it-works.md`「手势翻译」节、卡 `gesture-translation/patterns/swift-gesture-translation.json`。

### 改造项 D（P1）：窗口级远程 + 手机比例虚拟屏（预研立项，不动手实现）

**目标**：预研报告，回答两个问题：① Windows 被控端能否稳定创建「手机比例」虚拟显示器并登记多档同比例模式；② 单窗口远程会话（App 资源库模式）在现有架构上的改造面。

**预研内容**：
1. Windows IDD（Indirect Display Driver）方案调查：微软 IddSampleDriver 起步 + **社区项目 snug-win（`github.com/nicocart/snug-win`，C#/.NET 8/WPF，基于 Snug 2.5.0 移植）——它已经在 Windows 上复刻了虚拟手机屏，是被控端虚拟屏的最直接参考**。产出：可行性、签名/安装复杂度（IDD 需要驱动签名，对便携 zip 分发模式是重大约束）、多档同比例模式的可行性。
2. 若虚拟屏落地：rd-encode 分辨率切换不停流的约束（同比例换档不重协商——snug 实证 UU 类推流工具如此，remoteG 需验证自家 WebCodecs/rd-encode 链路）；坐标映射按当前档位缩放系数下发（rd-proto 新字段）。
3. 窗口级远程会话改造面清单：rd-window-win 窗口枚举/置顶/关闭按钮定位 → proto v0.2 候选 WindowEvent/FitApplied（PROJECT_STATE.md 已列）衔接 → 控制端窗口列表 UI。
4. 体验细节预研（snug 实测结论直接搬运）：按窗口实测选档不按 App 预设（D1 证据）；「要存储吗」对话框跟随上屏；关闭按钮先核对再拦截（TouchTranslator CloseButton.isFrontmost 模式）。

**产出**：`03_architecture/` 下预研报告 + 12_tasks 任务卡草案。**本项只读不写产品代码。**

**参考代码清单**：`<snug>/docs/how-it-works.md`「手机屏」「屏幕适配」「手机型号」三节、`<snug>/Sources/SnugCore/PhoneScreen/ScreenFit.swift`（档位数学全文）、`<snug>/Sources/SnugCore/PhoneScreen/PhoneScreenSpec.swift`（屏身份/物理尺寸/序列号策略——Windows 侧对应物是 IDD 的 monitor descriptor）、`<snug>/Sources/SnugSystem/PhoneDisplayService.swift`（建屏/换档/帮手全生命周期）、卡 `virtual-display-management/patterns/macos-virtual-display-management.json`（invariants 四条全是 Windows 侧要重新回答的问题）。

### 改造项 E（P2）：可靠性细节包（四个独立小任务）

**E1 窗口操作记账还原**：rd-window-win 新增「改动前写账（原子写 JSON）→ 复原成功销账 → 启动时 recover()」；适用点：F9 隐私屏、F4 侧边栏引起的窗口调整。参考：`<snug>/Sources/SnugCore/Persistence/JSONFile.swift:35`、`<snug>/Sources/SnugCore/PhoneScreen/PhoneDesk.swift:1-13` 头注释、卡 `window-move-restore`。
**E2 自扰静默期**：rd-agent 会话保活/重连判定中，凡本进程引起的显示拓扑/窗口扰动（分辨率切换、隐私屏、未来虚拟屏）先登记静默窗（取 max 延后），静默期内不判断线。参考：`<snug>/Sources/SnugCore/Session/ConnectionTracker.swift:18-19,31-35`。
**E3 重启回看恢复**：rd-agent 重启后先回看近期持久化状态/日志恢复「当前会话/推流」认知，恢复完成前状态显式报「未知」（不猜）。参考：`<snug>/Sources/Snug/Sensing/CaptureLogWatcher.swift:2-5,15,44`。
**E4 外部操作超时与双通道查询**：窗口/UIA 操作统一 1s 级超时（Win32 SendMessageTimeout / UIA 超时），存在性查询走不经进程的 API（IsWindow/GetWindowRect），与内容操作分离。参考：`<snug>/Sources/SnugSystem/AX.swift:2,13,22`、`<snug>/Sources/SnugCore/PhoneScreen/PhoneServices.swift:53-86`、`<snug>/Sources/SnugSystem/WindowService.swift:12-13,249-257`。

每个小任务独立提交、独立验收（单测或 e2e），不动协议。

---

## 3. 明确不照搬的部分

| 不照搬 | 原因 |
|---|---|
| CGVirtualDisplay / AX / `_silgen_name` 私有 API（`<snug>/Sources/SnugSystem/AX.swift`、`CGVirtualDisplayBridge/`） | macOS 专属私有接口；Windows 侧走 IDD/UIA/Win32 公开面 |
| 三路信号连接判定整体架构 | snug 无 API 只能观测推断；remoteG 自有协议直接知道会话状态。E3 的「回看恢复」与 E2 的「静默期」两个细节除外 |
| 被控端手势翻译（CGEvent tap 接管注入事件） | 见改造项 C 决策点：remoteG 两端自有，翻译放控制端更干净 |
| SwiftUI 界面 / App 资源库 UI 实现细节 | 前端栈不同；借鉴其信息架构（运行中/全部/搜索、防跳动的「只增删不调换」刷新策略——`<snug>/docs/how-it-works.md`「App 资源库·刷新」段） |
| FastCDC 具体常数照抄 | 32Ki/128Ki/512Ki 适合 ~10MB 级 App 包；remoteG zip 体积分布不同，按产物实测调参 |

---

## 4. 执行纪律（给改造 AI）

1. **本地先做好，再推送**（用户确立的工作流，严禁「推送→看红→再修」）：fmt、clippy（`cargo clippy --workspace --all-targets -- -D warnings`）、全量测试本地全绿后才碰远端；工具链对齐 `rust-toolchain.toml`（CI 曾因本地/CI clippy 版本偏斜翻车十轮，见 `CI-FIX-HANDOFF.md`）。
2. e2e 中前台输入注入类用例（t05/t06 等）**严禁在用户使用电脑时运行**。
3. 任务卡进 `12_tasks/`，完成后回写 `RUN_LOG.md` 与 `PROJECT_STATE.md`（对齐既有格式）；协议改动同步 `05_api`。
4. commit 格式 `<type>: <描述>`（feat/fix/refactor/docs/test/chore/perf/ci）。
5. 所有 snug 引用路径基于归档根 `C:\Users\blank\claude-learn-done\zephyr4123-snug`；源码只读参考，不改动、不提交 snug 内容到本仓库（实现须重写为 Rust/自有代码，注意 MIT 许可证归属——如大段移植算法逻辑，在文件头注明 derived from zephyr4123/snug (MIT)）。
6. 置信纪律：本文档所有 snug 证据行号已核实；remoteG 侧落点为文件/模块级指引，改造前先读对应 crate 现状再动手。
