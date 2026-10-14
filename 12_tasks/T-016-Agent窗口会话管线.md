# T-016 Agent 窗口会话：列表、事件、单窗口管线与 Fit
- 所属 Epic/Story：E5 Agent / US-01、US-02、US-03
- 负责 Agent：A4 Agent 集成
- 优先级：P0
- 预估：2d
- 前置依赖：T-015、T-008、T-009、T-010
- 可并行组：第 7 批（T-016 ∥ T-017 ∥ T-018 ∥ T-020）

## 目标
实现 `window_session.rs`：窗口列表与事件推送；打开窗口后建立 捕获→H.264 视频流 + 无损瓦片流；处理视口上报执行 Fit；关闭/断开时停止并恢复窗口。

## 上下文（只读）
- 05_api/协议契约.md §2（窗口相关消息）、§3、§4
- 03_architecture/架构方案.md §3.1、§3.2
- `apps/rd-agent/src/session.rs`（共享结构与桩签名）
- `apps/rd-agent/src/spike.rs`（可复用管线代码）

## 修改范围（可写）
- 修改：`apps/rd-agent/src/window_session.rs`

## 实现步骤
1. `ListWindows` → `list_windows()` 过滤 `is_capturable` → `WindowList`（`id = hwnd as u64`，图标 PNG 缓存按 hwnd）。
2. 首次 `ListWindows` 后订阅全局事件 watcher（模块内 `OnceLock` 单例 + `broadcast`），把 Created/Destroyed/Moved/Title 转为 `WindowEvent` 推送给该会话。
3. `OpenWindow`：校验窗口存在且可捕获（否则 Error 10/11）→ `save_placement` → 开启管线 → 回 `OpenWindowAck{level: L0_PIXEL, codec: H264_420, w, h, video_stream_id, tile_stream_id}`。
4. 管线（每窗口）：捕获线程 → 编码线程（`create_best`，`config.start_kbps`，`max_fps` = min(请求, 配置)）→ `DataTx(Video)`；同一帧送 `TileRefiner::on_frame`（失效）并每 50 ms `tick`（预算 = 当前码率的 50% × 0.05 s）→ `DataTx(Tile)`。`config.refine_enabled=false` 时跳过瓦片。帧尺寸变化 → `reconfigure` + `TileRefiner` 重建。首帧 IDR。
5. 无变化时不编码（捕获无新帧即不发），节省带宽。
6. `ViewportInfo`：若该窗口 `fit == FIT` → `target_client_size` → `fit_window`；结果 Partial/NotResizable 时发 `Error{code:12}`（非致命，控制端转为本地缩放）。同一窗口视口变化 150 ms 去抖。
7. `CloseWindow` / `on_disconnect`：停止管线（取消令牌，join 线程），`restore_placement`。
8. `state.paused == true` 时暂停出帧（保留连接）。
9. `WindowSessions::is_open` 返回真实状态。

## 测试要求
- 自动化由 T-025 覆盖；本任务手测：控制端未完成时用 T-025 的测试客户端或在本文件内加 `#[cfg(test)]` 的管线单元测试（mock 帧源 → 检查发出的首帧为 IDR、静止 300 ms 后有瓦片）。
- 运行：`cargo test -p rd-agent`

## 验收标准 DoD
- [ ] 打开窗口 1 s 内收到首个视频 keyframe
- [ ] 静止 300 ms 后收到瓦片，变化时收到失效帧
- [ ] Fit 后远端客户区尺寸 = 视口物理像素；关闭后窗口复原
- [ ] 同一会话可同时打开 ≥ 3 个窗口，CPU < 30%（硬编，1080p）
- [ ] 断开后无残留线程（日志确认 join）

## 风险与回滚
- 管线线程泄漏：所有线程持有取消令牌，`on_disconnect` 统一 join。回滚 revert（桩恢复）。

## 禁止事项
- 不处理窗口组与码率自适应（T-027）；不修改 session.rs 等其他文件，接口问题上报 Orchestrator。
