# PROJECT_STATE

- 更新：2026-11-19（**snug 改造计划全项落地 + 操作入口审计**：改造项 A 缺块增量自更新（含自动检查）+ B 国内渠道发布 + C 手势翻译 crate + D 虚拟屏预研 + E 窗口还原/静默/恢复/探针四小项，T-044~T-048；全量门禁复绿，提交 5c4182a/a06b577/a76b45a/05052a4）

- 更新：2026-11-19（**首次 tag 发布实测收官**：v0.3.1 三轮 CI 四 job 全绿，Release 六资产齐全+minisign 验签 VALID，暴露并修复 4 个 CI 缺陷，提交 6612087/ae65ec5/d29ac23）

- 更新：2026-11-19（**手测清单自动化：§5 五项做成 e2e（t11~t14）**——t11a 纯库直测 + t11b/t11c 更新全链（自动触发+确认框直通钩子）+ t12 隐私屏恢复 + t13 挂死探针 + t14 账本崩溃还原；顺带修 4 个产品缺陷（Range 差一/chunked 缺本机块/.old 竞速/账本目录接线），提交 7488a70/ab5eea2）

## 当前状态
- **首次 tag 发布实测（2026-11-19）**：仓库获得远端后首跑 release.yml 全链验证。三轮 CI：首跑暴露 3 缺陷（FFmpeg 下载 URL 404；controller-windows 缺 FFMPEG_DIR/LIBCLANG_PATH 且 ffmpeg-bin/ 被 gitignore；controller-macos 因融合版 controller 链接 Windows-only 的 rd-agent 从未可编译→摘除），二轮暴露第 4 缺陷（bundle.resources 引用 studio/dist 构建产物被 gitignore），三轮 44466532480 **四 job 全绿**。Release v0.3.1：zip/pack/update.json(447 chunks)/setup.exe/.sig/latest.json 六资产齐全；pack_sha256 与 zip 逐字节一致实算验证；**minisign 验签两路 VALID**（minisign-verify crate 同 tauri 消费端实现，T-038 密钥对配对性首次实证）；TAURI_SIGNING_PRIVATE_KEY secret 已配（rd-updater.key 直传）。遗留归 T-039：RD_UPDATE_SIGN_KEY/COS secrets、agent 更新链签名、darwin 恢复条件。详见 RUN_LOG 2026-11-19 行。
- **手测清单自动化（2026-11-19）**：`09_test/手测清单.md` §5 的 5.1~5.3/5.5~5.7 六节固化为 e2e 自动回归（`cargo test -p rd-agent --test e2e`，tests/e2e/ 目录化 main.rs + cases_update.rs + cases_recovery.rs）：**t11a** rd_update_pack/updater 纯库直测（check 防回滚/坏签名、apply full+chunked、坏块注入、暂存残留、icacls 只读回退）、**t11b/t11c** 更新 e2e 全链（本地 Range 服务器 + 假发布三件套 + `RD_UPDATE_TEST_CONFIRM=1` 确认框直通 + `RD_UPDATE_TEST_AUTO=1` main.rs 自动触发 update_flow，断言对调重启新进程第二条 RD_LISTEN/无 .old 残留/HelloAck；chunked 实测只拉 0.6% 字节）、**t12** 隐私屏 ON 崩溃重启恢复 + OFF 对照（`RD_TEST_SILENT_NOTIFY=1` 直通知情框）、**t13** 挂死窗口探针（NtSuspendProcess + fit_window 1.006s 返回）、**t14** 账本崩溃还原 + 正常销账不动对照。**首跑即修出 4 个产品缺陷**：HTTP Range 半开/闭区间差一（chunked 对标准 Range 服务器必挂）、chunked 拼装缺本机共享块、.old 清理与旧进程映像锁竞速、账本目录接线断（记账写默认目录/恢复读 --config-dir）。人测仅剩：托盘菜单入口手感、5.4 COS（T-039）、5.8/5.9。e2e 全套 16 用例全绿（含 t05/06/07 注入与 t12 黑窗，用户空闲时段）。详见 RUN_LOG 2026-11-19 行。
- **snug 改造（2026-11-19）**：依 `13_handoff/snug-借鉴与改造计划.md` 五项全落地。**A（P0）**：新 crate `rd-update-pack`（FastCDC 切块 + pack 格式**有意不去重** + ed25519 验签 + 防回滚）+ Agent `updater.rs`（check fail-closed→apply 缺块 Range 下载→拼装→.old 对调→重启计划）+ `RD_UPDATE_MODE=full|chunked` 灰度 + 托盘「检查更新」+ release.yml 打包接线；**B（P0）**：COS 镜像（mirror.rs 官方固定向量锚定签名 + 内容寻址可中断重跑 + latest 指针防回滚）+ `scripts/rd-mirror.ps1` 补传 + CI mirror-cos job，密钥只经 secret/@文件；**C（P1）**：零依赖 crate `rd-touch-translate`（四相手势状态机 + 滚动惯性，13 测试）+ ContinuousScroll tag 51 **全链路接线**（2026-11-19 用户指令提前：控制端 touch.rs 翻译编排+60fps 惯性驱动、前端 pointerType 路由、被控端 scroll_pixel 1px=1delta；触屏轻点=点击/拖动=惯性滚动/长按=右键，手测 §5.9 待触屏真机）；**D（P1）**：虚拟屏预研报告 + T-044 卡（virtual-display-rs 首选，改造面 11 项两期）；**E（P2）**：窗口记账还原（ledger.rs 崩溃后放回窗口）+ 自扰静默（quiet.rs 黑窗事件风暴过滤）+ 隐私屏状态跨重启恢复 + 挂死窗口 1s 探针（会话不被拖死）。手测口径：`09_test/手测清单.md` §5（本地 HTTP 模拟发布端全链路等八节）。遗留：验签公钥占位/COS 真渠道验证归 T-039；e2e 前台注入类待桌面空闲。
- **T-043（2026-11-07）**：**EasyTier 组网接入**——第四种跨网路径（mesh 内置打洞/中继/多协议回退）。批次1 编排器：新 crate `switchboard/sdk/rust/switchboard-mod-easytier`（sb.easytier.mesh，process 绑定不引 easytier 依赖：TOML 配置生成 + easytier-core 进程托管含 Windows Job Object 孤儿防护 + cli route 发现 camel/snake 双兼容）+ SourceKind::Mesh + registry/spec07/原理卡/USAGE 注册；批次2 agent：agent.toml `[easytier]` 节（no_tun 默认 true 免管理员）+ 托管降级 + 虚拟 IP 进直连候选（relay 竞速零改动）；批次3 controller：融合版自动继承托管 + 转发候选接线（DeviceConn::connect extra_addrs，默认路径不变）+ 设置页 EasyTier 节。验证：rd workspace 297+52+67 测试、switchboard 60 测试、前端 198 测试、双 workspace clippy 零 error；e2e t04/t05/t09 失败为前台注入类测试环境性失败（memory 警告），待空闲复跑。待外部：easytier 二进制真机双机实测（09_test/手测清单.md 1.16）。详见 RUN_LOG 2026-11-07 行。
- **T-042/T-041（2026-11-07）**：**官网下载页**（website/ 单页静态站+隐私政策+Pages 工作流，OWNER 占位待仓库远端）与**发码台账**（license-ledger.ps1 半自动发码+发码SOP.md 含撤销泄露预案；正式种子实发一单验收通过）。批次 1 至此代码全部落地，详见 RUN_LOG 2026-11-07 行。

## 当前状态
- **T-040（2026-11-07）**：**崩溃上报**——新 crate rd-crash（panic hook→JSON dump 只记元数据无内存转储、list/clear/prune、预填 Issue URL）；agent 托盘「上报崩溃…」+ controller 设置页「崩溃报告」节 + crash_dumps/clear/report_url 命令；两端 RD_TEST_CRASH 验收开关；上报地址 OWNER 占位待 T-039。详见 RUN_LOG 2026-11-07 行。
- **T-038（2026-11-07）**：**自动更新接入**——tauri-plugin-updater 全链路（conf 公钥/端点占位/passive 安装 + restart_app 命令 + 前端状态机 hook/顶栏横幅/设置页手动检查 + 24h 静默节流）；CI 签名接线（secrets 待配）并顺带修复 release.yml 产物路径遗留 bug（F-2 后应在根 target/）；更新私钥在 C:\Users\blank\.rd-secrets\rd-updater.key（待转密码管理器）。剩余：端点 OWNER 占位替换 + 全链路升级模拟归 T-039（需远端仓库）。详见 RUN_LOG 2026-11-07 行。
- **T-036（2026-11-06）**：**正式授权密钥轮换 + 版本基线 0.3.0**——`LICENSE_PUBKEY_HEX` 换随机正式种子派生公钥（种子存 `C:\Users\blank\.rd-secrets\license-seed.txt`，**待转入密码管理器后删文件**，绝不进仓库）；验签规则：正式公钥唯一锚点 + debug 构建 dev 种子兜底（e2e 零改动）、release 编译期剔除（发布版拒开发码，双档位测试锚定）；新增 show_pubkey 派生工具；版本 0.1.0→0.3.0。旧 0.1.0 产物的开发码在新版失效，重发需正式种子重签。详见 RUN_LOG 2026-11-06 行。
- **V0.2 商业化底座（2026-10-27 收官）**：T-029（画质四档/审计/授权码）+ T-028 收官（F4 窗口侧边栏 / F9 隐私屏 / F10 录屏）+ T-030（e2e t09 FEC、安全自动化、配对协商位分裂修复）。此前路线图 F4/F9/F10 标 ⬜ 系文档滞后，已于 2026-11-06 核实 RUN_LOG 2026-10-27 行后修正。下一阶段为 V0.3「分发与留存」，任务卡见 `12_tasks/V0.3-分发与留存任务卡.md`（T-036 密钥轮换为全局卡点）。
- **T-035（2026-11-02）**：**直连候选发布 + 多路径竞速**——agent 采集直连候选（本机物理网卡 v4/v6 单播地址 + UPnP/IGD 外部映射，新依赖 igd）随 REG 上报，DIAL ACK 下发候选列表；控制端竞速升级为「反射打洞 + 逐候选直接 QUIC + 中继」全员同场，谁先握手完成用谁。打洞方案自此可插拔（IPv6/UPnP/未来方案=新候选来源，控制端零改动）。开关：RD_PUNCH 总闸 / RD_PUNCH_UPNP。详见 RUN_LOG 2026-11-02 行。
- **T-034（2026-11-02）**：**打洞（去 Tailscale 依赖主线第一步）**——rd-relay 协议扩展（DIAL ACK 带被控端地址尾巴 + PUNCH_NOTIFY 下发）+ rd-transport 双端探测竞速（`connect_via_relay_punch`：中继路径与打洞直连谁先握手完成用谁）+ agent 探测响应（pump 从探测包源地址学控制端真实映射，直连 socket 回探测）+ 控制端中继回退自动带打洞。**兼容性**：旧中继/旧端自动退化纯中继，Tailscale 路径不动；**定制**：`RD_PUNCH=0` 关打洞。全链路 e2e（打洞直连命中 + 无响应回退中继）连跑 5 轮全稳，workspace 0 失败。遗留：真机跨网成功率待实测；STUN/UPnP/IPv6 为后续插件（pump 事件通道已预留）；详见 RUN_LOG 2026-11-02 行。
- **T-032/T-033（2026-11-01）**：rd 原生中继 rd-relay 全链路（UDP 打标转发 crate + rd-transport 虚拟链路 + agent 一行配置 + 控制端自动回退与设置页「远程访问」）；WS 版（Cloudflare Workers 免费版 / Node+Docker 自动 HTTPS，参考 alma-relay）与三条路线部署套件（见 switchboard/deploy/rd-relay/）。经中继 e2e 标 ignore（握手挂起待查），协议基础由 rd-relay 单测锁定。总结见 13_handoff/UU对标与中继落地总结.md。
- **T-031（2026-10-31）**：文件传输（proto tag 45~50 + agent file_handler + 控制端 files 命令 + 会话内 FilesPanel 浮层）与 WOL 远程开机（HelloAck.mac_addrs + wake_device + 设备卡片菜单）全链路落地；e2e t10（推送+拉取逐字节比对）+ 双端单测全绿。详见 RUN_LOG 2026-10-31 行与 05_api §7。已知限制：推送分片在分发循环落盘（大文件传输期输入延迟上升，v0.3 拆流）；WOL 仅同网段。code-review 修复轮闭环（2H+6M+5L，backlog 仅剩唤醒去抖——已顺手实现 3s 去抖）。手测项：09_test/手测清单.md 1.14（文件传输）/1.15（WOL）。
- **M0**：完成（D-010 关卡结论：Tauri 2 + WebCodecs 维持；场景 D iPhone probe 已测，场景 A/B/C 延迟数据待用户补测）。
- **M1**：27 张任务卡全部执行完毕（含 FIX-1）。自动化验收全绿（2026-10-23 复验）：
  - `cargo test --workspace`：36 组全 ok（约 180 用例）
  - `cargo clippy --workspace --all-targets -- -D warnings`：零警告
  - `pnpm test`（控制端）：114 用例全过 + tsc 干净
  - `cargo test -p rd-agent --test e2e -- --test-threads=1`：16/16（旧 10 用例 + t11a/b/c、t12/t13/t14 手测自动化新用例）
- **D-013 五专家评审 + 全量四批改进（2026-10-22~23，提交 586b2cc/3482e5b/2f8609a）**：
  - 包1 小白可用：双击即用、弹窗自报地址、LAN 直连默认开、UDP 发现、开机自启、指纹收 tooltip、一键重配对
  - 包2 可靠性：会话保活、陈旧帧丢弃、持续退避重连、UIPI 反馈、销毁收场、save_placement 修复
  - 包3 安全：暂停覆盖终端、连接确认框、撤销信任入口、配对超时/并发上限、device_name 消毒、zip SHA256
  - 包4 协议 v0.1：ClipboardData/SessionState/view_only、码控丢包阈值化、砍 fmt=1（05_api §6）
- **T-026 验收**：待用户按更新后 `09_test/兼容性.md` 手测（**新增验证项**：扫描局域网→配对、连接确认框、暂停→终端终止、锁屏横幅、剪贴板互传、只看不控）+ 场景 A/B/C 延迟 + 1 小时稳定性，然后确认 M1 完成。

## 交付物
- Agent：`dist/rd-agent-0.1.0-win64.zip`（便携，exe+LGPL DLL+LICENSE，打包脚本 `scripts/package-agent.ps1`）
- 控制端：`pnpm -C apps/rd-controller tauri build`（NSIS/dmg 未签名）；CI release.yml 在 tag `v*` 时自动产出并挂 GitHub Release
- 回滚：git 按 tag 回退；Agent 便携 zip 直接替换

## 未决事项（M1 收尾/移交 M2）
0. **待用户手测**（09_test/兼容性.md 已重写为含 D-013 新功能的完整版；release 产物已预构建、C:\rd 联接与 C:\rd-test bat 已恢复）：兼容性矩阵（**扩充项**：扫描局域网→配对、连接确认框、暂停→终端终止、锁屏横幅、剪贴板互传、只看不控、撤销信任）、场景 A/B/C 延迟、1 小时稳定性。
1. 用户执行兼容性手测矩阵 + 场景 A/B/C 延迟（RUN_LOG 回填后关闭 P95 指标）。
2. 协议 v0.2 候选（05_api §6.4）：TerminalExit/TermClose、Agent 剪贴板变化推送、WindowEvent 状态事件、FitApplied、BBR 拥塞 A/B。**新增：视频切 fec-shards**（Switchboard `sb.moonlight.fec-rs` + quinn datagram 已在 switchboard 侧端到端验证，rd 侧收发管线切换待做）。
2a. **M2 已落地（2026-10-24）**：① `crates/rd-switch`——Plan 执行器选路 + rd 原生 QUIC（`plan_dial`，`RD_PLAN` 环境变量切入并已接入 `DeviceConn::connect`，默认行为不变）；② **码控 v0.2**：`rd-encode/src/rate.rs` 换 baseline-adaptive（RustDesk 1.5.0 QoS 规则：基线渐进适应/3 连坏才降帧/先码率后帧率/2 连好快恢复/低码率爬坡封顶），API 不变 window_session 零改动；③ **BBR A/B 开关**：`RD_BBR=0` 临时回退 Cubic（BBR 仍为默认，D-013 包5 结论）；④ `switchboard/` v0.1.4（规范+SDK 7 crate 36 测试+SB-Bench T0/T3/T4+**Studio 三页含运行态可视化与基准对比**+daemon+deploy/DERP 套件）；⑤ **T-027 任务卡**（视频切 fec-shards）已就绪待排期。
2b. **待外部资源**：自建 DERP 部署（`switchboard/deploy/derper/` 套件就绪，需港区 VPS）；实网 T3/T4 跑分出 L-C 分（需两台真机）。
3. NVIDIA 驱动 ≥610.00 后重跑 444 探针验证 nvenc/444 端到端（`cargo test -p rd-encode --test encode create_best_yuv444_local_probe -- --ignored --nocapture`；创建接口与前端解码配置已就绪。2026-10-22 实测驱动 572.70，仍阻塞）。
4. 零拷贝送硬编优化（当前 staging 读回 ~1.3ms/帧已达标）。
5. e2e 间歇性**已定位并修复（2026-10-24）**：套件内编码器冷启动（MFT 实例反复创建）使首帧超出 5s 等待预算——视频类等待放宽至 15s（首帧性能仍由 2s WARN 追踪，任务卡口径不变）；串行 e2e 连续两次 8/8。
6. macOS：cargo check 待 CI 首跑确认；keyring 身份存储待真机验证。
7. CI 推送变绿验证：仓库尚无远端（用户提供 GitHub 私有仓库后 push）。
8. 服务化（SCM+崩溃重启）为 M2 决策点（E5 建议，E1/E2 认为现阶段 HKCU Run 自启已覆盖帮家人场景）。

## 关键决策索引
DECISIONS.md D-001~D-013（自研、Tailscale、Tauri 2、文字清晰不依赖 4:4:4、终端只接管自启会话、免费 Apple ID、React 前端、仓库分离、CloseWindow 补全、M0 结论、FIX-1 插入、FIX-2 产出入 OUT_DIR、五专家评审与全量四批改进）。
