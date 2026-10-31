# PROJECT_STATE

- 更新：2026-10-31（T-031 文件传输 + WOL 远程开机落地，UU 远控对标两项代码缺口补齐）

## 当前状态
- **T-031（2026-10-31）**：文件传输（proto tag 45~50 + agent file_handler + 控制端 files 命令 + 会话内 FilesPanel 浮层）与 WOL 远程开机（HelloAck.mac_addrs + wake_device + 设备卡片菜单）全链路落地；e2e t10（推送+拉取逐字节比对）+ 双端单测全绿。详见 RUN_LOG 2026-10-31 行与 05_api §7。已知限制：推送分片在分发循环落盘（大文件传输期输入延迟上升，v0.3 拆流）；WOL 仅同网段。
- **M0**：完成（D-010 关卡结论：Tauri 2 + WebCodecs 维持；场景 D iPhone probe 已测，场景 A/B/C 延迟数据待用户补测）。
- **M1**：27 张任务卡全部执行完毕（含 FIX-1）。自动化验收全绿（2026-10-23 复验）：
  - `cargo test --workspace`：36 组全 ok（约 180 用例）
  - `cargo clippy --workspace --all-targets -- -D warnings`：零警告
  - `pnpm test`（控制端）：114 用例全过 + tsc 干净
  - `cargo test -p rd-agent --test e2e -- --test-threads=1`：8/8（14~18s，含瓦片逐像素断言）
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
