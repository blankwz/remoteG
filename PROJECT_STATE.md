# PROJECT_STATE

- 更新：2026-10-15（M1 代码完成，T-026 验收关卡）

## 当前状态
- **M0**：完成（D-010 关卡结论：Tauri 2 + WebCodecs 维持；场景 D iPhone probe 已测，场景 A/B/C 延迟数据待用户补测）。
- **M1**：27 张任务卡全部执行完毕（含插入的 FIX-1 修复）。自动化验收全绿：
  - `cargo test --workspace`：35 组全 ok（约 150 用例）
  - `cargo clippy --workspace --all-targets -- -D warnings`：零警告
  - `pnpm test`（控制端）：106 用例全过 + tsc 干净
  - `cargo test -p rd-agent --test e2e -- --test-threads=1`：8/8（14~18s，含瓦片逐像素断言）
- **T-026 验收**：待用户执行 `09_test/兼容性.md` 手测矩阵 + 补测场景 A/B/C 延迟 + 1 小时稳定性，然后确认 M1 完成。

## 交付物
- Agent：`dist/rd-agent-0.1.0-win64.zip`（便携，exe+LGPL DLL+LICENSE，打包脚本 `scripts/package-agent.ps1`）
- 控制端：`pnpm -C apps/rd-controller tauri build`（NSIS/dmg 未签名）；CI release.yml 在 tag `v*` 时自动产出并挂 GitHub Release
- 回滚：git 按 tag 回退；Agent 便携 zip 直接替换

## 未决事项（M1 收尾/移交 M2）
1. 用户执行兼容性手测矩阵 + 场景 A/B/C 延迟（RUN_LOG 回填后关闭 P95 指标）。
2. 协议 v0.1 建议：TerminalExit/TermClose、CloseWindow/SetViewport 回包语义、TerminalExit code。
3. rd-encode 444 创建接口（T-027 遗留，一行透传）；NVIDIA 驱动 ≥610.00 后重跑 probe 验证 nvenc/444。
4. T-002 `CapturedFrame.resized` 信号失效（管线已用尺寸差量绕过）；零拷贝送硬编优化。
5. e2e 套件间歇性（10 轮 3 次单例失败：t06 前台门控/t07 清理裕量）——加固小任务待排。
6. macOS：cargo check 待 CI 首跑确认；keyring 身份存储待真机验证。
7. CI 推送变绿验证：仓库尚无远端（用户提供 GitHub 私有仓库后 push）。
8. 剪贴板同步（M1 末尾可选项）——用户未决定，默认不做。

## 关键决策索引
DECISIONS.md D-001~D-011（自研、Tailscale、Tauri 2、文字清晰不依赖 4:4:4、终端只接管自启会话、免费 Apple ID、React 前端、仓库分离、CloseWindow 补全、M0 结论、FIX-1 插入）。
