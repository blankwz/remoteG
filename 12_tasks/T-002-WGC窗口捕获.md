# T-002 WGC 窗口捕获
- 所属 Epic/Story：E1 捕获 / US-02、US-03
- 负责 Agent：A2 捕获与编码
- 优先级：P0
- 预估：2d
- 前置依赖：T-001
- 可并行组：第 2 批（T-002 ∥ T-003 ∥ T-004 ∥ T-024）

## 目标
`rd-capture-win` 能按 HWND 持续输出**客户区** BGRA 帧（带时间戳、尺寸变化通知、可选脏区）。

## 上下文（只读）
- 01_discovery/可行性初判.md §1
- 03_architecture/架构方案.md §3.2

## 修改范围（可写）
- 修改：`crates/rd-capture-win/Cargo.toml`、`src/lib.rs`、`src/wgc.rs`、`src/crop.rs`
- 新建：`crates/rd-capture-win/examples/capture_dump.rs`、`crates/rd-capture-win/tests/crop.rs`

## 实现步骤
1. D3D11 设备（`D3D11CreateDevice`, `BGRA_SUPPORT`）→ `CreateDirect3D11DeviceFromDXGIDevice` → `IDirect3DDevice`。
2. `IGraphicsCaptureItemInterop::CreateForWindow(hwnd)` 得到 `GraphicsCaptureItem`；`Direct3D11CaptureFramePool::CreateFreeThreaded`（BGRA8，2 缓冲）；`CreateCaptureSession`；`SetIsCursorCaptureEnabled(false)`（光标由控制端本地绘制）；Win11 上 `SetIsBorderRequired(false)`（调用失败忽略）。
3. `FrameArrived` 回调：取纹理 → 拷贝到 staging 纹理 → `Map` → 按 `crop.rs` 计算的客户区矩形裁剪 → 发送 `CapturedFrame` 到 `crossbeam_channel::bounded(2)`（满则丢旧帧）。
4. `crop.rs`：`DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` + `GetClientRect` + `ClientToScreen` 计算客户区在捕获帧内的偏移；纯计算部分抽成 `fn client_rect_in_frame(frame_bounds, client_origin, client_size) -> Rect` 便于单测。
5. 帧尺寸变化（`ContentSize` 改变）时 `Recreate` 帧池，并在 `CapturedFrame.resized = true`。
6. 脏区：若当前 windows crate/系统提供 WGC dirty region API 则填 `dirty: Some(..)`，否则 `None`（T-010 负责块比对兜底）。实现时以 windows crate 文档为准，不确定则直接 `None`。
7. `examples/capture_dump.rs <窗口标题子串>`：`FindWindow` 遍历匹配，捕获 1 帧存 PNG（`image` crate，dev-dependency）。

## 接口
```rust
pub struct Rect { pub x: i32, pub y: i32, pub w: u32, pub h: u32 }
pub struct CapturedFrame {
    pub width: u32, pub height: u32, pub stride: u32,
    pub data: Vec<u8>,            // BGRA, 客户区
    pub dirty: Option<Vec<Rect>>, // 相对客户区
    pub resized: bool,
    pub captured_at: std::time::Instant,
}
pub struct CaptureOptions { pub max_fps: u32 }
pub struct WindowCapture { /* session, pool, thread */ }
impl WindowCapture {
    pub fn start(hwnd: isize, opts: CaptureOptions) -> anyhow::Result<(Self, crossbeam_channel::Receiver<CapturedFrame>)>;
    pub fn stop(self);
}
pub fn is_supported() -> bool; // GraphicsCaptureSession::IsSupported
```

## 测试要求
- 新增：`tests/crop.rs`（DPI 100%/150%、有/无标题栏的矩形计算）。
- 手测：`cargo run -p rd-capture-win --example capture_dump -- 记事本` 输出 PNG 无标题栏、无黄框（Win11）。
- 运行：`cargo test -p rd-capture-win`

## 验收标准 DoD
- [ ] 记事本、Chrome、VS Code 能输出正确客户区画面
- [ ] 被其他窗口遮挡时画面仍正确；最小化时 `start` 返回错误或停止出帧且不崩溃
- [ ] 1080p 窗口连续捕获 60 fps 时单帧读回 < 8 ms（日志打印）
- [ ] 窗口缩放时不崩溃，`resized` 正确置位
- [ ] 单测通过、clippy 无警告

## 风险与回滚
- GPU→CPU 读回开销：M1 可接受；零拷贝送硬编列为后续优化（记入 RUN_LOG 遗留）。
- 回滚：crate 内独立，revert 本任务提交。

## 禁止事项
- 不实现编码、网络；不修改其他 crate。
