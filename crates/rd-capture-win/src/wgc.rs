//! WGC 会话：按 HWND 建立捕获流，输出客户区 BGRA 帧。
//!
//! 管线：`D3D11CreateDevice`（BGRA_SUPPORT）→ `CreateDirect3D11DeviceFromDXGIDevice`
//! → `IGraphicsCaptureItemInterop::CreateForWindow` → `Direct3D11CaptureFramePool::
//! CreateFreeThreaded`（BGRA8、2 缓冲）→ `FrameArrived` 回调中取纹理 → staging
//! 拷贝 → `Map` 读回 → 按 `crop` 裁客户区 → `crossbeam_channel::bounded(2)`
//! 满则丢旧帧。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureDirtyRegionMode,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, IsIconic, IsWindow};

use crate::crop::{client_rect_in_frame, Rect};

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
const BUFFER_COUNT: i32 = 2;

/// `IDirect3DDevice` 包装。WGC 的 `Direct3D11CaptureFramePool`（free-threaded）设计上
/// 允许跨线程 Recreate，其包装的 D3D 设备对象为 agile 对象；windows-rs 0.62 未为该
/// WinRT 接口生成 Send/Sync，这里显式声明以跨回调线程共享。
#[derive(Clone)]
struct AgileDevice(IDirect3DDevice);
unsafe impl Send for AgileDevice {}
unsafe impl Sync for AgileDevice {}

/// 捕获配置。
pub struct CaptureOptions {
    /// 帧率上限（每秒最多投递多少帧；WGC 只在窗口有更新时推帧，静止窗口无帧）。
    pub max_fps: u32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self { max_fps: 60 }
    }
}

/// 一帧客户区像素。`data` 为紧凑行排布（行距 = `stride`），BGRA8。
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// BGRA，客户区尺寸。
    pub data: Vec<u8>,
    /// 相对客户区的脏区；系统不支持 WGC 脏区（Win11 24H2 前）时为 `None`（由 T-010 块比对兜底）。
    pub dirty: Option<Vec<Rect>>,
    /// 与上一投递帧相比窗口内容尺寸（WGC `ContentSize`，含非客户区边框）发生了
    /// 变化（首帧为 false）。信号为粘性：通道满丢旧帧时，未送达的 `resized`
    /// 会转移到后续帧，消费端不会因背压而错过。
    pub resized: bool,
    pub captured_at: Instant,
    /// GPU→CPU 读回耗时（CopyResource + Map + 行拷贝），供 DoD「60fps 单帧读回 <8ms」验证。
    pub readback: Duration,
}

/// 窗口捕获会话。`stop(self)` 或 Drop 时关闭会话并停止推帧。
pub struct WindowCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    frame_arrived_token: i64,
    // 持有 Shared 强引用：回调闭包只持 Weak，Drop（先移除回调再 Close）后回调不会再复活它。
    _shared: Arc<Shared>,
}

/// WGC 是否可用（Win10 1903+ 且未被策略禁用）。
pub fn is_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

impl WindowCapture {
    /// 开始捕获 `hwnd` 的客户区。返回会话句柄与帧接收端。
    ///
    /// 最小化窗口不视为错误：会话正常建立，但窗口还原前不产出帧。
    pub fn start(hwnd: isize, opts: CaptureOptions) -> Result<(Self, Receiver<CapturedFrame>)> {
        if !is_supported() {
            bail!("Windows.Graphics.Capture 不受支持（需 Windows 10 1903+）");
        }

        // 调用线程未初始化 COM 时兜底为 MTA；已初始化（STA/MTA）则保持原样，失败忽略。
        // 进程生命周期内不再 CoUninitialize（rd-agent 场景可接受）。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                bail!("无效 HWND: {:#x}", hwnd.0 as usize);
            }
        }

        let (device, context) = create_d3d11_device()?;
        let winrt_device = create_winrt_device(&device)?;
        let item = create_item_for_window(hwnd)?;

        let item_size = item.Size().context("获取 GraphicsCaptureItem.Size 失败")?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            FORMAT,
            BUFFER_COUNT,
            item_size,
        )
        .context("创建 Direct3D11CaptureFramePool 失败")?;
        let session = pool
            .CreateCaptureSession(&item)
            .context("创建 CaptureSession 失败")?;

        // 光标由控制端本地绘制；Win11 去黄框；脏区报告（Win11 24H2+）。
        // 三者在不支持的系统上失败均忽略。
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly);

        let (tx, rx) = crossbeam_channel::bounded(2);
        let shared = Arc::new(Shared {
            d3d_device: device,
            context,
            winrt_device: AgileDevice(winrt_device),
            tx,
            // crossbeam 为 MPMC：回调在通道满时用它丢最旧一帧（用户端 recv 与之竞争，正常只有一个消费端）。
            rx: rx.clone(),
            hwnd: hwnd.0 as isize,
            min_interval: Duration::from_secs_f64(1.0 / opts.max_fps.max(1) as f64),
            state: Mutex::new(State {
                staging: None,
                staging_size: (0, 0),
                pool_size: item_size,
                delivery: DeliveryState::default(),
                last_sent: None,
                dirty_supported: None,
            }),
        });

        let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new({
            let shared = Arc::downgrade(&shared);
            move |sender, _args| {
                let Some(shared) = shared.upgrade() else {
                    return Ok(());
                };
                if let Ok(pool) = sender.ok() {
                    on_frame_arrived(&shared, pool);
                }
                Ok(())
            }
        });
        let frame_arrived_token = pool
            .FrameArrived(&handler)
            .context("注册 FrameArrived 失败")?;

        session.StartCapture().context("StartCapture 失败")?;

        Ok((
            Self {
                session,
                pool,
                frame_arrived_token,
                _shared: shared,
            },
            rx,
        ))
    }

    /// 停止捕获。实际清理在 Drop 中完成（先移除回调，再关闭帧池与会话）。
    pub fn stop(self) {}
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_arrived_token);
        let _ = self.pool.Close();
        let _ = self.session.Close();
    }
}

/// 跨回调共享的不可变资源与可变状态。COM 接口均为自由线程对象（Send + Sync）。
struct Shared {
    d3d_device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: AgileDevice,
    tx: Sender<CapturedFrame>,
    rx: Receiver<CapturedFrame>,
    hwnd: isize,
    min_interval: Duration,
    state: Mutex<State>,
}

struct State {
    staging: Option<ID3D11Texture2D>,
    staging_size: (u32, u32),
    pool_size: SizeInt32,
    delivery: DeliveryState,
    last_sent: Option<Instant>,
    /// `None` = 尚未探测；`Some(true/false)` = 系统是否支持 frame.DirtyRegions（Win11 24H2+）。
    dirty_supported: Option<bool>,
}

/// `resized` 信号的判定与背压状态。
///
/// - `prev_content`：上一**投递成功**帧的 `ContentSize`（基线）。帧池纹理尺寸
///   滞后 `ContentSize` 一帧（Recreate 后才追平）且受系统对齐影响，不能作为
///   基线——基线必须用内容尺寸，resize 瞬间即置位、无过渡误报。
/// - `pending_resize`：已有尺寸变化信号进入通道但尚未确定被消费端取走。通道
///   满丢旧帧时被丢帧的信号转移到顶替帧，避免 T-006 实证过的「信号被挤掉」。
#[derive(Default)]
struct DeliveryState {
    prev_content: Option<(u32, u32)>,
    pending_resize: bool,
}

fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    // 回调线程 panic 不应毒化捕获状态。
    state.lock().unwrap_or_else(|p| p.into_inner())
}

fn on_frame_arrived(shared: &Shared, pool: &Direct3D11CaptureFramePool) {
    // 关键：无论是否节流都必须排空帧池——WGC 池（2 缓冲）满后不再触发
    // FrameArrived，一旦某次回调不取帧，事件流就会永久停止。
    // 只保留最后一帧做处理，其余（过时）帧直接丢弃。
    let mut latest = None;
    while let Ok(f) = pool.TryGetNextFrame() {
        latest = Some(f);
    }
    let Some(frame) = latest else { return };

    let now = Instant::now();
    {
        let st = lock_state(&shared.state);
        if let Some(t) = st.last_sent {
            if now.duration_since(t) < shared.min_interval {
                tracing::debug!("frame arrived but throttled");
                return; // 超出 max_fps，丢弃本次更新
            }
        }
    }

    tracing::debug!("frame arrived, processing");
    if process_frame(shared, pool, &frame, now) {
        lock_state(&shared.state).last_sent = Some(now);
    }
}

/// 处理一帧：必要时 Recreate 池 → staging 读回 → 裁客户区 → 投递。
/// 返回是否成功投递了一帧。
fn process_frame(
    shared: &Shared,
    pool: &Direct3D11CaptureFramePool,
    frame: &Direct3D11CaptureFrame,
    now: Instant,
) -> bool {
    let content = match frame.ContentSize() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("ContentSize 失败: {e}");
            return false;
        }
    };
    if content.Width <= 0 || content.Height <= 0 {
        tracing::warn!("ContentSize 为空: {content:?}");
        return false;
    }

    // 帧尺寸变化：按新尺寸 Recreate 帧池。
    {
        let mut st = lock_state(&shared.state);
        if content.Width != st.pool_size.Width || content.Height != st.pool_size.Height {
            if let Err(e) = pool.Recreate(&shared.winrt_device.0, FORMAT, BUFFER_COUNT, content) {
                tracing::warn!("帧池 Recreate 失败: {e}");
            }
            st.pool_size = content;
        }
    }

    let surface: IDirect3DSurface = match frame.Surface() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Surface 失败: {e}");
            return false;
        }
    };
    let access: IDirect3DDxgiInterfaceAccess = match surface.cast() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("cast IDirect3DDxgiInterfaceAccess 失败: {e}");
            return false;
        }
    };
    let src: ID3D11Texture2D = match unsafe { access.GetInterface() } {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("GetInterface(ID3D11Texture2D) 失败: {e}");
            return false;
        }
    };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { src.GetDesc(&mut desc) };
    if desc.Width == 0 || desc.Height == 0 {
        return false;
    }

    // staging 纹理：随源尺寸变化重建。
    let staging = {
        let mut st = lock_state(&shared.state);
        if st.staging_size != (desc.Width, desc.Height) {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            if unsafe {
                shared
                    .d3d_device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            }
            .is_err()
            {
                tracing::warn!("创建 staging 纹理失败 ({}x{})", desc.Width, desc.Height);
                return false;
            }
            let Some(staging) = staging else { return false };
            st.staging = Some(staging.clone());
            st.staging_size = (desc.Width, desc.Height);
            staging
        } else {
            match st.staging.clone() {
                Some(t) => t,
                None => return false,
            }
        }
    };

    let t0 = Instant::now();
    unsafe { shared.context.CopyResource(&staging, &src) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if unsafe {
        shared
            .context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
    }
    .is_err()
    {
        tracing::warn!("Map staging 失败");
        return false;
    }

    let client = client_rect_physical(
        HWND(shared.hwnd as *mut core::ffi::c_void),
        desc.Width,
        desc.Height,
    );
    let Some(rect) = client else {
        unsafe { shared.context.Unmap(&staging, 0) };
        tracing::debug!("窗口最小化/无效，跳帧");
        return false; // 最小化 / 窗口销毁：不出帧
    };

    let pitch = mapped.RowPitch as usize;
    let bytes = pitch * desc.Height as usize;
    let buf = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), bytes) };
    let bw = rect.w as usize * 4;
    let mut data = Vec::with_capacity(bw * rect.h as usize);
    for row in 0..rect.h as usize {
        let off = (rect.y as usize + row) * pitch + rect.x as usize * 4;
        data.extend_from_slice(&buf[off..off + bw]);
    }
    unsafe { shared.context.Unmap(&staging, 0) };
    let readback = t0.elapsed();

    let dirty = extract_dirty(shared, frame, rect);

    let mut st = lock_state(&shared.state);
    deliver(
        &mut st.delivery,
        &shared.tx,
        &shared.rx,
        (content.Width.max(0) as u32, content.Height.max(0) as u32),
        CapturedFrame {
            width: rect.w,
            height: rect.h,
            stride: rect.w * 4,
            data,
            dirty,
            resized: false,
            captured_at: now,
            readback,
        },
    )
}

/// 投递一帧并维护 `resized` 信号基线（纯逻辑，`wgc.rs` 内单测覆盖）。
///
/// 判定：本帧内容尺寸 != 基线（上一投递帧），或存在挂起信号。投递成功后更新
/// 基线并清除挂起；通道满丢最旧一帧时，其未送达的 `resized` 转移到本帧。
/// 返回是否成功投递（决定是否刷新 `last_sent` 节流时钟）。
fn deliver(
    st: &mut DeliveryState,
    tx: &Sender<CapturedFrame>,
    rx: &Receiver<CapturedFrame>,
    content: (u32, u32),
    mut frame: CapturedFrame,
) -> bool {
    let size_changed = st.prev_content.map(|p| p != content).unwrap_or(false);
    frame.resized = size_changed || st.pending_resize;
    if frame.resized {
        tracing::debug!(
            content = ?content,
            prev = ?st.prev_content,
            size_changed,
            pending = st.pending_resize,
            "resized 置位"
        );
    }

    match tx.try_send(frame) {
        Ok(()) => {
            st.prev_content = Some(content);
            st.pending_resize = false;
            true
        }
        Err(TrySendError::Full(mut frame)) => {
            // 通道满：丢最旧一帧；它尚未被消费端取走，其 resized 信号转移到本帧。
            tracing::debug!("channel 满，丢旧帧（信号转移）");
            if let Ok(dropped) = rx.try_recv() {
                frame.resized |= dropped.resized;
            }
            match tx.try_send(frame) {
                Ok(()) => {
                    st.prev_content = Some(content);
                    st.pending_resize = false;
                    true
                }
                // 理论不可达（刚腾出一格）；防御性挂起信号，等通道恢复后随帧送达。
                Err(TrySendError::Full(_)) => {
                    st.pending_resize = true;
                    false
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// 提取脏区并换算为客户区相对坐标；系统不支持（首次 QI IDirect3D11CaptureFrame2
/// 失败）则缓存结果、之后不再尝试，恒返回 `None`。
fn extract_dirty(
    shared: &Shared,
    frame: &Direct3D11CaptureFrame,
    client: Rect,
) -> Option<Vec<Rect>> {
    {
        let st = lock_state(&shared.state);
        if st.dirty_supported == Some(false) {
            return None;
        }
    }
    let regions = match frame.DirtyRegions() {
        Ok(r) => r,
        Err(_) => {
            lock_state(&shared.state).dirty_supported = Some(false);
            return None;
        }
    };
    lock_state(&shared.state).dirty_supported = Some(true);

    let count = regions.Size().unwrap_or(0);
    if count == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let Ok(r) = regions.GetAt(i) else { continue };
        // 帧坐标 → 客户区坐标，并与客户区求交裁剪。
        let left = r.X.max(client.x);
        let top = r.Y.max(client.y);
        let right = (r.X + r.Width).min(client.x + client.w as i32);
        let bottom = (r.Y + r.Height).min(client.y + client.h as i32);
        if right <= left || bottom <= top {
            continue;
        }
        out.push(Rect {
            x: left - client.x,
            y: top - client.y,
            w: (right - left) as u32,
            h: (bottom - top) as u32,
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 查询客户区在捕获帧内的矩形（物理像素）。最小化 / 窗口无效返回 None。
fn client_rect_physical(hwnd: HWND, frame_w: u32, frame_h: u32) -> Option<Rect> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
    }
    // 临时切换为 per-monitor v2，保证 GetClientRect/ClientToScreen 返回物理像素；
    // DWMWA_EXTENDED_FRAME_BOUNDS 本身总是物理像素。
    let old = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let result = query_client_geometry(hwnd).and_then(|g| {
        client_rect_in_frame(
            (frame_w, frame_h),
            g.bounds_origin,
            g.client_origin,
            g.client_size,
        )
    });
    unsafe { SetThreadDpiAwarenessContext(old) };
    result
}

/// 窗口几何（均物理像素）。
struct WindowGeometry {
    /// 窗口视觉边界（`DWMWA_EXTENDED_FRAME_BOUNDS`）左上角屏幕坐标。
    bounds_origin: (i32, i32),
    /// 客户区左上角屏幕坐标。
    client_origin: (i32, i32),
    /// 客户区尺寸。
    client_size: (u32, u32),
}

fn query_client_geometry(hwnd: HWND) -> Option<WindowGeometry> {
    unsafe {
        let mut crect = RECT::default();
        if GetClientRect(hwnd, &mut crect).is_err() {
            return None;
        }
        let mut origin = POINT { x: 0, y: 0 };
        if !ClientToScreen(hwnd, &mut origin).as_bool() {
            return None;
        }
        let bounds_origin = frame_bounds_origin(hwnd).unwrap_or_else(|| {
            // DWM 失败时回退 GetWindowRect（有细小偏差但可用）。
            let mut wrect = RECT::default();
            if GetWindowRect(hwnd, &mut wrect).is_ok() {
                (wrect.left, wrect.top)
            } else {
                (origin.x, origin.y)
            }
        });
        Some(WindowGeometry {
            bounds_origin,
            client_origin: (origin.x, origin.y),
            client_size: (
                (crect.right - crect.left).max(0) as u32,
                (crect.bottom - crect.top).max(0) as u32,
            ),
        })
    }
}

fn frame_bounds_origin(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()
    .map(|_| (rect.left, rect.top))
}

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice 失败")?;
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice 返回空设备"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice 返回空上下文"))?;
    Ok((device, context))
}

fn create_winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast().context("QI IDXGIDevice 失败")?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .context("CreateDirect3D11DeviceFromDXGIDevice 失败")?;
    inspectable.cast().context("QI IDirect3DDevice 失败")
}

fn create_item_for_window(hwnd: HWND) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("获取 IGraphicsCaptureItemInterop 工厂失败")?;
    unsafe { interop.CreateForWindow(hwnd) }.context("CreateForWindow 失败")
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    fn frame(resized: bool) -> CapturedFrame {
        CapturedFrame {
            width: 10,
            height: 10,
            stride: 40,
            data: Vec::new(),
            dirty: None,
            resized,
            captured_at: Instant::now(),
            readback: Duration::ZERO,
        }
    }

    /// 正常速率：尺寸变化帧恰好置位一次，恢复旧尺寸再次置位。
    #[test]
    fn resize_sets_flag_once_each_direction() {
        let (tx, rx) = crossbeam_channel::bounded(2);
        let mut st = DeliveryState::default();

        assert!(deliver(&mut st, &tx, &rx, (640, 480), frame(false)));
        assert!(!rx.try_recv().unwrap().resized, "首帧为 false");

        assert!(deliver(&mut st, &tx, &rx, (884, 561), frame(false)));
        assert!(rx.try_recv().unwrap().resized, "放大置位");

        assert!(deliver(&mut st, &tx, &rx, (884, 561), frame(false)));
        assert!(!rx.try_recv().unwrap().resized, "同尺寸不重复置位");

        assert!(deliver(&mut st, &tx, &rx, (640, 480), frame(false)));
        assert!(rx.try_recv().unwrap().resized, "缩回再次置位");
    }

    /// T-006 背压场景：通道恒满、消费端慢取——resized 信号必须转移到存活帧，
    /// 不被「丢最旧」策略挤掉。
    #[test]
    fn flag_survives_backpressure_drop() {
        let (tx, rx) = crossbeam_channel::bounded(2);
        let mut st = DeliveryState::default();
        let rx = rx.clone();

        // 通道塞满两帧同尺寸（模拟 resize 前的稳定流，消费端未取）。
        assert!(deliver(&mut st, &tx, &rx, (640, 480), frame(false)));
        assert!(deliver(&mut st, &tx, &rx, (640, 480), frame(false)));

        // resize 后连续投递 4 帧，期间消费端不取（编码慢）。
        for _ in 0..4 {
            assert!(deliver(&mut st, &tx, &rx, (624, 441), frame(false)));
        }

        // drain：必须存在 resized=true 的帧，且只投出信号不重复堆积。
        let flags: Vec<bool> = rx.try_iter().map(|f| f.resized).collect();
        assert!(
            flags.iter().any(|&r| r),
            "背压下 resized 信号被丢帧策略挤掉: {flags:?}"
        );
        assert_eq!(
            flags.iter().filter(|&&r| r).count(),
            1,
            "信号恰好一份: {flags:?}"
        );
    }
}
