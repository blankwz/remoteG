# CHANGELOG

## 性能优化增量（2026-11-05，criterion 实测）

冷热路径一次性测量定位（1080p 每帧）：OpenH264 软编 9.87ms、色彩转换 BGRA→I420 1.16ms、脏区全扫描 1.59ms、QOI 文本瓦片 109µs——软编路径总 ~12.6ms/frame，60fps 达标余量 25%，非瓶颈。

### 修复：静止窗口不再空转编码（rd-agent）
- **风控**：编码器 `process` 原无条件对每帧跑色彩转换 + 编码 + 发整帧。窗口静止时（脏块=0 / WGC 空脏区）改为跳过视频编码——静止区已由无损瓦片覆盖，控制器保持上一帧，`force_key` 置位使恢复变化首帧为干净 IDR。省下 ~11ms/帧 CPU 与整帧带宽。
- **正确性护栏**：带缓冲延迟的硬件编码器（h264_mf 等）首 `encode` 返回 `Ok(None)`，需持续喂帧才吐出首包；新增 `need_feed` 门控——编码器未出首包 / reconfigure 后禁止跳过，关键帧必达。
- 验证：新增 gating 单测（refine 开/关两路径、静止跳过、恢复 IDR）；**e2e 全套 10/10 通过**（含 t02 首帧 IDR、t04 resize、t09 FEC）。

### 码控 QoS 收敛契约锁定（rd-encode/rate.rs）
- 新增端到端仿真测试：**拥塞恢复**——重度拥塞跌底 500 后，warmup×1.3 应在 2.4s（12 tick）内爬回起始码率，防误删 warmup 退化；**启动契约**——首样本即达起始码率（warmup 是恢复坡非启动坡）。
- 厘清机制：warmup ×1.3 只在 `kbps < start_kbps` 时触发，是去拥塞恢复坡；好链路 ×1.15 持续爬升至 50Mbps 上限是 LAN 换画质的既定设计。

### 硬编路径验证（rd-encode/tests/oh264_sweep.rs）
- 用 `create_best` 生产接口测本机真实选路：**硬编 h264_mf 7.2ms/帧、静止内容 P 帧仅 ~18B**（IDR→首P→全跳过，无码率超发）；**软编 openh264 ~23-31ms/帧、对静止内容仍发 3-17KB/帧**。硬编兑现代价是 4 倍于软编的低延迟 + 更省字节，生产硬编优先路径健康；软编静止浪费更印证静止帧 gating（R3）的价值。字节样本区分出此前 42KB 为 IDR 采样伪影，非超发。

### OpenH264 编码参数扫参（负结论，防无谓改参）
- 新增 `#[ignore]` 手动工具 `tests/oh264_sweep.rs`：固定码率+固定内容扫（线程数 1~16 × 复杂度 Low/Medium）的 1080p 逐帧编码时延。
- 结论（release 实测）：**线程数几乎无关**——threads 1→16 时延 22→18ms、输出字节完全不变（2061）；openh264 逐帧 `encode_at` 不吃多线程，当前 `num_threads(4)` 无改参价值。**复杂度有效**：Low ~18.9ms < Medium ~23.5ms（快 ~20%），当前 `Complexity::Low` 正确。
- 暴露真实特征：**纯平文档内容软编 ~19ms/帧，贴近 20ms DoD 预算**；缓解靠硬编优先（已有）+ 静止帧 gating（R3）。

### 回归护栏扩展（rd-encode/benches/tiles.rs）
- 基准覆盖从脏区/QOI 扩展到**色彩转换（1080p/4K）+ QOI 解码 + OpenH264 软编（720p/1080p）**，全部热路径纳入 criterion 防回归。

实测基线（criterion）：dirty/update_1080p_full **1.49~1.59ms**、conv/bgra_to_i420_1080p **1.16ms**、encode/openh264_1080p **9.87ms**、tiles/qoi_encode_256x256 **109µs**、qoi_decode **24µs**。

> 注：`cargo fmt --all --check` 在 `apps/rd-agent/src/netinfo.rs:144` 存在前序提交（c7210ea）遗留的格式漂移，与本增量无关，未一并改动。

## v0.1.0（M1，2026-10-15）

首个可用版本：窗口级远程控制 MVP（M0 验证 + M1 全部 27 张任务卡）。

### 被控端 rd-agent（Windows 便携 zip）
- 窗口列表/事件推送（可捕获窗口过滤、图标、窗口组：对话框/菜单事件）
- WGC per-HWND 捕获 → H.264（硬编优先 nvenc/qsv/amf/mf，OpenH264 兜底）
- 静止 300ms 无损瓦片精修（QOI/zstd，64×64 块脏区跟踪）——文字逐像素清晰
- 窗口组：弹出对话框独立视频流按相对偏移合成；菜单事件+可交互（画面见已知限制）
- 码率/帧率 AIMD 自适应（RTT/积压/丢包）；4:4:4 协商（需硬件与驱动支持，当前诚实回退 420）
- 1:1 Fit：按控制端视口物理像素调整远端窗口客户区，会话结束恢复
- 键鼠注入（扫描码+UNICODE/IME、置前、防卡键）、ConPTY 终端（断线 10 分钟 resume、1MB 回放）
- SPAKE2 6 位配对码（一次性/5 分钟/5 次锁定）+ 证书指纹信任列表 + DPAPI 身份持久化
- 托盘（配对码/设备列表/暂停共享/日志目录）；日志按天滚动保留 7 天（无按键/终端内容）

### 控制端 rd-controller（Tauri 2，Windows/macOS）
- 设备管理（配对/信任/指纹钉扎/在线状态）；窗口列表页（事件驱动实时刷新）
- 会话页：多标签窗口组合成（视频+瓦片叠层）、Fit/Scale/Native × zoom、HUD、Ctrl+Shift+S 清晰度导出
- 输入映射（坐标/扫描码/IME/滚轮/防卡键）、终端标签（xterm.js、resume、字号调节）

### 质量
- workspace 35 组测试全绿（约 150 单测/集成）+ e2e 8 用例（14~18s，瓦片逐像素断言）
- 已知限制见 `09_test/兼容性.md`
