//! 窗口会话管线（T-016）：窗口列表、事件推送、单窗口 capture → H.264/无损瓦片
//! → QUIC 数据流，以及 1:1 视口适配（Fit）。
//!
//! ## 消息入口
//! `handle` 由 session.rs 分发循环（async 上下文）同步调用；实际工作
//! （Win32 枚举、QUIC 流建立）转交 tokio 任务，`&SessionCtx` 经 [`ctx_arc`]
//! 恢复为 `Arc` 后跨任务持有（契约见该函数文档）。
//!
//! ## 线程模型（每窗口管线）
//! - rd-capture-win 的 WGC 回调线程 → crossbeam 通道 →
//! - `rd-enc-<hwnd>` 编码线程：帧 → H.264 AU（视频通道）；同一帧喂
//!   [`TileRefiner`]（回收失效标记），每 50 ms tick 一次，把静止 ≥300 ms 的
//!   块在 `码率 × 50% × 50 ms` 的字节预算内编码为无损瓦片（瓦片通道）→
//! - 视频 / 瓦片两个 tokio 发送任务：`DataTx(Video/Tile)`，`select` 取消令牌。
//!
//! ## 退出协议
//! 取消令牌触发（CloseWindow / 断开 / 管线自亡）→ `WindowCapture` drop
//! （捕获通道关闭）→ 编码线程退出 → 发送任务退出 → 收尾任务从注册表移除
//! 该窗口并统一 join、恢复窗口布局；编码失败经 [`PipeError`] 由发送任务
//! 转为 `Error{code:11}` 回给控制端。
//!
//! ## 事件推送
//! 全局 `SetWinEventHook` 监听线程（rd-window-win）经桥接线程转 `broadcast`；
//! 连接首次 `ListWindows` 时订阅，事件经"当前可捕获顶层窗口"复核后转为
//! `WindowEvent` 推送（子窗口 / 菜单 / 已不在列表的窗口事件不上报）。
//!
//! 注：`handle` 内的工作为异步执行，同连接连续两条窗口消息的到达顺序
//! 不保证与协议顺序严格一致（tokio 调度粒度）；控制端按协议不应在
//! 未收到 ack 前对同一窗口发后续窗口消息，此为 M1 的既定约束。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use rd_capture_win::{CaptureOptions, CapturedFrame, WindowCapture};
use rd_encode::dirty::Rect as DirtyRect;
use rd_encode::frame::RawFrame;
use rd_encode::tiles::{TileOut, TileRefiner};
use rd_encode::{create_best, EncodedAu, EncoderConfig, VideoEncoder};
use rd_proto::v0::envelope::Msg;
use rd_proto::v0::window_event::Kind;
use rd_proto::v0::{
    CloseWindow, Codec, Error, FitMode, ListWindows, OpenWindow, OpenWindowAck, RenderLevel,
    ViewportInfo, WindowEvent, WindowInfo, WindowList,
};
use rd_transport::{DataTx, StreamKind, TileHeader, VideoHeader};
use rd_window_win::events::{spawn_event_watcher, WatcherHandle, WindowEventRaw};
use rd_window_win::resize::{fit_window, restore_placement, save_placement, target_client_size};
use rd_window_win::resize::{FitResult, SavedPlacement};
use rd_window_win::{list_windows, window_icon_png, WindowAttrs};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::session::{AgentState, SessionCtx};

/// 协议错误码 10：窗口不存在（05_api §4）。
const ERR_NO_WINDOW: u32 = 10;
/// 协议错误码 11：窗口无法捕获（最小化/受保护/管线失败；05_api §4）。
const ERR_NOT_CAPTURABLE: u32 = 11;
/// 协议错误码 12：无法调整尺寸（已退化为 SCALE；05_api §4，非致命）。
const ERR_FIT_DEGRADED: u32 = 12;
/// 同一窗口视口上报的去抖间隔（任务卡步骤 6）。
const VIEWPORT_DEBOUNCE: Duration = Duration::from_millis(150);
/// 瓦片 tick 周期（任务卡步骤 4）。
const TILE_TICK: Duration = Duration::from_millis(50);
/// 瓦片块静止判定阈值（架构 §3.2：区域静止 ≥300 ms 后精修）。
const TILE_STILL_MS: u32 = 300;
/// 瓦片预算占当前码率的比例（任务卡步骤 4：50% × 0.05 s）。
const TILE_BUDGET_RATIO: f64 = 0.5;
/// 事件广播通道容量（每连接一个订阅者，缓冲慢消费者）。
const BROADCAST_CAP: usize = 256;
/// 图标缓存条目上限（hwnd 复用/长期运行导致的过期条目靠整表清空兜底）。
const ICON_CACHE_CAP: usize = 1024;
/// 编码通道容量（编码线程 → 视频发送任务；有界形成背压）。
const AU_CHANNEL_CAP: usize = 64;
/// 瓦片通道容量（失效标记可能整帧批量产生）。
const TILE_CHANNEL_CAP: usize = 256;
/// 编码线程空转等待下限（recv_timeout 的最小超时，防忙等）。
const SLEEP_MIN: Duration = Duration::from_millis(1);

// ---------------------------------------------------------------------------
// 进程级注册表（WindowSessions 为 unit struct，接口由 session.rs 固定）
// ---------------------------------------------------------------------------

/// window_id(hwnd) → 窗口管线。多连接共享；断开时仅清理本连接打开的窗口。
static PIPES: OnceLock<Mutex<HashMap<u64, PipeHandle>>> = OnceLock::new();
/// 连接标识（SessionCtx 地址）→ 事件订阅取消通道。
static EVENT_SUBS: OnceLock<Mutex<HashMap<usize, mpsc::Sender<()>>>> = OnceLock::new();
/// hwnd → 图标 PNG（首见查询，含 None 防重复查询失败窗口）。
static ICON_CACHE: OnceLock<Mutex<HashMap<u64, Option<Vec<u8>>>>> = OnceLock::new();
/// 最近一次列表枚举得到的可捕获顶层窗口集合（事件复核用）。
static TOP_WINDOWS: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();

fn pipes() -> &'static Mutex<HashMap<u64, PipeHandle>> {
    PIPES.get_or_init(Default::default)
}

fn event_subs() -> &'static Mutex<HashMap<usize, mpsc::Sender<()>>> {
    EVENT_SUBS.get_or_init(Default::default)
}

fn icon_cache() -> &'static Mutex<HashMap<u64, Option<Vec<u8>>>> {
    ICON_CACHE.get_or_init(Default::default)
}

fn top_windows() -> &'static Mutex<HashSet<u64>> {
    TOP_WINDOWS.get_or_init(Default::default)
}

/// Mutex 中毒恢复：状态各字段独立、无跨字段不变量，直接沿用内部值（guard）。
fn lock_poisoned<T>(p: std::sync::PoisonError<T>) -> T {
    p.into_inner()
}

/// 把分发循环传入的 `&SessionCtx` 恢复为 `Arc<SessionCtx>`。
///
/// # 接口契约（依赖 session.rs 的实现细节）
/// session.rs 的分发循环持有 `Arc<SessionCtx>` 并以 `Arc` deref 出来的
/// `&ctx` 调用本模块；此处按同一地址临时递增强引用计数，将其提升为
/// 拥有型 `Arc`（调用返回后原 `Arc` 仍存活，计数回到平衡）。
/// 若上游改为传栈上临时对象的引用，此函数即 UB——session.rs 接口变更
/// 需同步调整本模块并上报 Orchestrator。
fn ctx_arc(ctx: &SessionCtx) -> Arc<SessionCtx> {
    let ptr: *const SessionCtx = ctx;
    // SAFETY: 见函数文档;指针来自存活中的 Arc<SessionCtx>。
    unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
}

/// `&SessionCtx` 在进程级注册表中的键（Arc 堆地址，连接生命周期内稳定唯一）。
fn ctx_key(ctx: &SessionCtx) -> usize {
    std::ptr::from_ref::<SessionCtx>(ctx) as usize
}

// ---------------------------------------------------------------------------
// 消息入口（session.rs 固定的公开签名）
// ---------------------------------------------------------------------------

/// 窗口类控制流消息（session.rs 分发时按类型归组）。
#[derive(Debug)]
pub enum WindowMsg {
    List(ListWindows),
    Open(OpenWindow),
    Close(CloseWindow),
    Viewport(ViewportInfo),
}

/// 窗口子会话集合：window_id(hwnd) → 捕获管线（实体状态见模块级 [`PIPES`]）。
///
/// `handle`/`on_disconnect` 的签名不含 self，故以进程级单例承载；
/// 多连接共享一张表，`on_disconnect` 只清理本连接（owner）打开的窗口。
#[derive(Debug, Default)]
pub struct WindowSessions;

impl WindowSessions {
    /// 指定窗口是否已打开捕获会话。
    #[allow(dead_code)]
    pub fn is_open(&self, id: u64) -> bool {
        pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .contains_key(&id)
    }
}

/// 处理一条窗口消息。业务错误回 `Error{code:10/11/12}`（异步）；
/// 返回 Err 仅用于协议级致命错误（本模块当前无此类错误）。
pub fn handle(ctx: &SessionCtx, req_id: u32, msg: WindowMsg) -> anyhow::Result<()> {
    match msg {
        WindowMsg::List(_) => handle_list(ctx, req_id),
        WindowMsg::Open(m) => handle_open(ctx, req_id, m),
        WindowMsg::Close(m) => handle_close(m),
        WindowMsg::Viewport(m) => handle_viewport(ctx, m),
    }
}

/// 会话断开：停止本连接打开的全部窗口管线，并撤销事件订阅。
pub fn on_disconnect(ctx: &SessionCtx) {
    let key = ctx_key(ctx);
    event_subs()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .remove(&key); // drop 发送端 → 对应事件推送任务退出

    let mine: Vec<PipeHandle> = {
        let mut map = pipes().lock().unwrap_or_else(lock_poisoned);
        let owned: Vec<u64> = map
            .iter()
            .filter(|(_, h)| h.owner == key)
            .map(|(id, _)| *id)
            .collect();
        owned.into_iter().filter_map(|id| map.remove(&id)).collect()
    };
    for h in mine {
        h.shutdown(true);
    }
}

// ---------------------------------------------------------------------------
// List：窗口列表 + 首次事件订阅
// ---------------------------------------------------------------------------

fn handle_list(ctx: &SessionCtx, req_id: u32) -> anyhow::Result<()> {
    let ctx = ctx_arc(ctx);
    // 首次 ListWindows 启动全局 watcher 并为本连接订阅事件推送（幂等）。
    subscribe_events(&ctx);
    tokio::spawn(async move {
        // 枚举与图标查询可能被 hung 窗口拖慢（每窗最多 200 ms），放 blocking 池。
        let listed = tokio::task::spawn_blocking(list_infos).await;
        match listed {
            Ok(Ok(infos)) => {
                if let Err(err) = ctx
                    .send(Msg::WindowList(WindowList { windows: infos }), req_id)
                    .await
                {
                    tracing::warn!("回 WindowList 失败: {err:#}");
                }
            }
            Ok(Err(err)) => tracing::warn!("枚举窗口失败: {err:#}"),
            Err(err) => tracing::warn!("窗口枚举任务失败: {err:#}"),
        }
    });
    Ok(())
}

/// 枚举可捕获顶层窗口并构造协议 `WindowInfo`（含图标缓存），同步刷新顶层集合。
fn list_infos() -> anyhow::Result<Vec<WindowInfo>> {
    let wins = list_windows()?;
    {
        let mut top = top_windows().lock().unwrap_or_else(lock_poisoned);
        *top = wins.iter().map(|w| w.hwnd as u64).collect();
    }
    Ok(wins
        .iter()
        .map(|a| to_window_info(a, icon_png_cached(a.hwnd as u64)))
        .collect())
}

fn to_window_info(a: &WindowAttrs, icon: Option<Vec<u8>>) -> WindowInfo {
    WindowInfo {
        id: a.hwnd as u64,
        title: a.title.clone(),
        process: a.process.clone(),
        pid: a.pid,
        icon_png: icon.unwrap_or_default(),
        x: a.rect.x,
        y: a.rect.y,
        w: a.rect.w,
        h: a.rect.h,
        dpi: a.dpi,
        resizable: a.resizable,
        minimized: a.minimized,
    }
}

/// hwnd → 图标 PNG（缓存；上限整表清空兜底 hwnd 复用导致的陈旧条目）。
fn icon_png_cached(hwnd: u64) -> Option<Vec<u8>> {
    let mut cache = icon_cache().lock().unwrap_or_else(lock_poisoned);
    if cache.len() > ICON_CACHE_CAP {
        cache.clear();
    }
    cache
        .entry(hwnd)
        .or_insert_with(|| window_icon_png(hwnd as isize))
        .clone()
}

// ---------------------------------------------------------------------------
// 事件推送：全局 watcher（OnceLock 单例）→ broadcast → 每连接订阅任务
// ---------------------------------------------------------------------------

/// 全局窗口事件监听（进程生命周期持有）。
struct EventWatcher {
    tx: broadcast::Sender<WindowEventRaw>,
    /// 持有监听线程（Drop 停线程并反装 hook）；单例永不释放。
    _handle: WatcherHandle,
}

static WATCHER: OnceLock<EventWatcher> = OnceLock::new();
/// watcher 初始化锁（避免并发首次 ListWindows 重复启动监听线程）。
static WATCHER_INIT: Mutex<()> = Mutex::new(());

fn ensure_watcher() -> Option<&'static EventWatcher> {
    if let Some(w) = WATCHER.get() {
        return Some(w);
    }
    let _guard = WATCHER_INIT.lock().unwrap_or_else(lock_poisoned);
    if let Some(w) = WATCHER.get() {
        return Some(w);
    }
    let (handle, rx) = match spawn_event_watcher() {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!("启动窗口事件监听失败（事件推送不可用，列表不受影响）: {err:#}");
            return None;
        }
    };
    let (tx, _) = broadcast::channel(BROADCAST_CAP);
    // 监听线程(crossbeam) → 各连接订阅任务(broadcast) 的桥接线程；
    // JoinHandle 直接 detach（单例生命周期 = 进程生命周期）。
    let bridge_tx = tx.clone();
    let bridged = std::thread::Builder::new()
        .name("rd-winevent-bridge".into())
        .spawn(move || {
            while let Ok(ev) = rx.recv() {
                // 无订阅者时 send 返回 Err，事件直接丢弃（列表仍可拉取）
                let _ = bridge_tx.send(ev);
            }
        });
    if bridged.is_err() {
        tracing::warn!("启动窗口事件桥接线程失败（handle drop 会回收监听线程）");
        return None; // handle drop → 监听线程退出，可下次重试
    }
    let _ = WATCHER.set(EventWatcher {
        tx,
        _handle: handle,
    });
    WATCHER.get()
}

/// 为本连接订阅窗口事件推送（幂等：同一连接只起一个推送任务）。
fn subscribe_events(ctx: &SessionCtx) {
    let key = ctx_key(ctx);
    let mut subs = event_subs().lock().unwrap_or_else(lock_poisoned);
    if subs.contains_key(&key) {
        return;
    }
    let Some(watcher) = ensure_watcher() else {
        return;
    };
    let mut ev_rx = watcher.tx.subscribe();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<()>(1);
    subs.insert(key, cmd_tx);
    drop(subs);

    let ctx = ctx_arc(ctx);
    tokio::spawn(async move {
        loop {
            // cmd_tx 被 on_disconnect 移除（drop）后 recv 返回 None → 退出
            let ev = tokio::select! {
                None = cmd_rx.recv() => break,
                ev = ev_rx.recv() => match ev {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "窗口事件推送积压，丢弃 {n} 条");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            };
            let Some(msg) = window_event_msg(&ev) else {
                continue;
            };
            if ctx.send(msg, 0).await.is_err() {
                break; // 连接已断
            }
        }
        event_subs()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .remove(&key);
        tracing::debug!("窗口事件推送任务已退出");
    });
}

/// 原始事件 → 协议 `WindowEvent`（子窗口/菜单事件过滤；顶层复核见函数体）。
fn window_event_msg(ev: &WindowEventRaw) -> Option<Msg> {
    let (kind, hwnd) = match ev {
        WindowEventRaw::Created(h) => (Kind::Created, *h),
        WindowEventRaw::Destroyed(h) => (Kind::Destroyed, *h),
        WindowEventRaw::Moved(h) => (Kind::Moved, *h),
        WindowEventRaw::Title(h) => (Kind::Title, *h),
        // 菜单弹出事件属窗口组（T-027），M1 不推送
        WindowEventRaw::MenuStart(_) | WindowEventRaw::MenuEnd(_) => return None,
    };
    let wid = hwnd as u64;

    if kind == Kind::Destroyed {
        // 窗口已销毁、无法复核：只推此前确认在列表中的顶层窗口
        let was_top = top_windows()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .remove(&wid);
        if !was_top {
            return None;
        }
        return Some(Msg::WindowEvent(WindowEvent {
            kind: Kind::Destroyed as i32,
            info: Some(WindowInfo {
                id: wid,
                ..WindowInfo::default()
            }),
            root_id: wid,
        }));
    }

    // Created/Moved/Title：复核当前仍为可捕获顶层窗口（顺带全量刷新缓存）
    let wins = match list_windows() {
        Ok(w) => w,
        Err(err) => {
            tracing::debug!("窗口事件复核枚举失败: {err:#}");
            return None;
        }
    };
    {
        let mut top = top_windows().lock().unwrap_or_else(lock_poisoned);
        *top = wins.iter().map(|w| w.hwnd as u64).collect();
    }
    let attrs = wins.iter().find(|a| a.hwnd as u64 == wid)?;
    Some(Msg::WindowEvent(WindowEvent {
        kind: kind as i32,
        info: Some(to_window_info(attrs, icon_png_cached(wid))),
        root_id: wid,
    }))
}

// ---------------------------------------------------------------------------
// Open：校验 → 保存布局 → 捕获 → 数据流 → 管线线程/任务 → ack
// ---------------------------------------------------------------------------

fn handle_open(ctx: &SessionCtx, req_id: u32, open: OpenWindow) -> anyhow::Result<()> {
    let ctx = ctx_arc(ctx);
    tokio::spawn(async move {
        if let Err(err) = open_window(&ctx, req_id, open).await {
            tracing::warn!("OpenWindow 处理失败: {err:#}");
        }
    });
    Ok(())
}

async fn open_window(ctx: &Arc<SessionCtx>, req_id: u32, open: OpenWindow) -> anyhow::Result<()> {
    let wid = open.window_id;
    // 同窗口重复打开：先按关闭语义停掉旧管线（恢复布局，避免覆盖原始 placement）
    if let Some(old) = pipes().lock().unwrap_or_else(lock_poisoned).remove(&wid) {
        tracing::info!(window = format!("{wid:#x}"), "窗口已打开，先关闭旧会话");
        old.shutdown(true);
    }

    // 校验：不存在/不可捕获 → 10；最小化 → 11（WGC 会话可建但无帧，显式拒绝）
    let attrs = {
        let wins = list_windows().context("枚举窗口失败")?;
        wins.into_iter().find(|a| a.hwnd as u64 == wid)
    };
    let Some(attrs) = attrs else {
        return send_error(
            ctx,
            req_id,
            ERR_NO_WINDOW,
            format!("窗口 {wid:#x} 不存在或不可捕获"),
        )
        .await;
    };
    if attrs.minimized {
        return send_error(ctx, req_id, ERR_NOT_CAPTURABLE, "窗口处于最小化，无法捕获").await;
    }

    let cfg = &ctx.state.config;
    // max_fps = min(请求, 配置)；请求 0 视为未指定，用配置默认
    let fps = if open.max_fps == 0 {
        cfg.max_fps
    } else {
        open.max_fps.min(cfg.max_fps)
    }
    .max(1);
    let kbps = cfg.start_kbps.max(1);
    let refine = cfg.refine_enabled;

    let saved = match save_placement(attrs.hwnd) {
        Ok(p) => Some(p),
        Err(err) => {
            tracing::warn!(window = format!("{wid:#x}"), "保存窗口布局失败: {err:#}");
            None
        }
    };

    let (capture, frame_rx) =
        match WindowCapture::start(attrs.hwnd, CaptureOptions { max_fps: fps }) {
            Ok(v) => v,
            Err(err) => {
                restore_saved(attrs.hwnd, &saved);
                return send_error(
                    ctx,
                    req_id,
                    ERR_NOT_CAPTURABLE,
                    format!("启动捕获失败: {err:#}"),
                )
                .await;
            }
        };

    let mut video_tx = match ctx.conn.open_data(StreamKind::Video).await {
        Ok(t) => t,
        Err(err) => {
            drop(capture);
            restore_saved(attrs.hwnd, &saved);
            return send_error(
                ctx,
                req_id,
                ERR_NOT_CAPTURABLE,
                format!("打开视频流失败: {err:#}"),
            )
            .await;
        }
    };
    let mut tile_tx = match ctx.conn.open_data(StreamKind::Tile).await {
        Ok(t) => t,
        Err(err) => {
            drop(capture);
            restore_saved(attrs.hwnd, &saved);
            return send_error(
                ctx,
                req_id,
                ERR_NOT_CAPTURABLE,
                format!("打开瓦片流失败: {err:#}"),
            )
            .await;
        }
    };
    let video_stream_id = u32::try_from(video_tx.id()).unwrap_or(u32::MAX);
    let tile_stream_id = u32::try_from(tile_tx.id()).unwrap_or(u32::MAX);

    let cancel = CancellationToken::new();
    let errors = Arc::new(PipeError::default());
    let (au_tx, au_rx) = mpsc::channel(AU_CHANNEL_CAP);
    let (tile_out_tx, tile_rx) = mpsc::channel(TILE_CHANNEL_CAP);

    let enc_thread = {
        let run = PipeRun {
            frame_rx,
            au_tx,
            tile_tx: tile_out_tx,
            state: Arc::clone(&ctx.state),
            fps,
            kbps,
            refine,
            errors: Arc::clone(&errors),
            cancel: cancel.clone(),
        };
        match std::thread::Builder::new()
            .name(format!("rd-enc-{wid:#x}"))
            .spawn(move || encode_loop(run))
        {
            Ok(t) => t,
            Err(err) => {
                drop(capture);
                restore_saved(attrs.hwnd, &saved);
                let _ = video_tx.finish();
                let _ = tile_tx.finish();
                return send_error(
                    ctx,
                    req_id,
                    ERR_NOT_CAPTURABLE,
                    format!("启动编码线程失败: {err}"),
                )
                .await;
            }
        }
    };

    tokio::spawn(video_send_task(
        Arc::clone(ctx),
        cancel.clone(),
        video_tx,
        au_rx,
        wid,
        Arc::clone(&errors),
    ));
    tokio::spawn(tile_send_task(
        Arc::clone(ctx),
        cancel.clone(),
        tile_tx,
        tile_rx,
        wid,
        errors,
    ));

    pipes().lock().unwrap_or_else(lock_poisoned).insert(
        wid,
        PipeHandle {
            cancel: cancel.clone(),
            capture: Some(capture),
            enc_thread: Some(enc_thread),
            saved,
            hwnd: wid,
            fit: open.fit,
            deb: Arc::new(ViewportDeb::default()),
            owner: ctx_key(ctx),
        },
    );

    ctx.send(
        Msg::OpenWindowAck(OpenWindowAck {
            window_id: wid,
            level: RenderLevel::L0Pixel as i32,
            video_stream_id,
            tile_stream_id,
            codec: Codec::H264420 as i32,
            w: attrs.rect.w,
            h: attrs.rect.h,
        }),
        req_id,
    )
    .await?;

    tracing::info!(
        window = format!("{wid:#x}"),
        fps,
        kbps,
        refine,
        video_stream_id,
        tile_stream_id,
        "窗口管线已建立"
    );
    Ok(())
}

fn restore_saved(hwnd: isize, saved: &Option<SavedPlacement>) {
    if let Some(p) = saved {
        if let Err(err) = restore_placement(hwnd, p) {
            tracing::warn!(hwnd = format!("{hwnd:#x}"), "恢复窗口布局失败: {err:#}");
        }
    }
}

async fn send_error(
    ctx: &SessionCtx,
    req_id: u32,
    code: u32,
    message: impl Into<String>,
) -> anyhow::Result<()> {
    let message = message.into();
    tracing::info!(code, %message, "回错误消息");
    ctx.send(Msg::Error(Error { code, message }), req_id).await
}

// ---------------------------------------------------------------------------
// Close / Viewport
// ---------------------------------------------------------------------------

fn handle_close(close: CloseWindow) -> anyhow::Result<()> {
    let handle = pipes()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .remove(&close.window_id);
    match handle {
        Some(h) => {
            h.shutdown(true);
            tracing::info!(
                window = format!("{:#x}", close.window_id),
                "窗口会话已关闭并恢复布局"
            );
        }
        None => {
            tracing::debug!(
                window = format!("{:#x}", close.window_id),
                "CloseWindow：该窗口未打开，忽略"
            );
        }
    }
    Ok(())
}

/// 视口去抖状态：pending 存最新上报，abort 取缔上一轮未到期的定时任务。
#[derive(Default)]
struct ViewportDeb {
    pending: Mutex<Option<ViewportInfo>>,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
}

fn handle_viewport(ctx: &SessionCtx, vp: ViewportInfo) -> anyhow::Result<()> {
    let (cancel, deb, hwnd, fit) = {
        let map = pipes().lock().unwrap_or_else(lock_poisoned);
        match map.get(&vp.window_id) {
            Some(h) => (h.cancel.clone(), Arc::clone(&h.deb), vp.window_id, h.fit),
            None => {
                tracing::debug!(
                    window = format!("{:#x}", vp.window_id),
                    "Viewport：该窗口未打开，忽略"
                );
                return Ok(());
            }
        }
    };
    *deb.pending.lock().unwrap_or_else(lock_poisoned) = Some(vp);
    if let Some(prev) = deb.abort.lock().unwrap_or_else(lock_poisoned).take() {
        prev.abort(); // 150 ms 内的连续上报只保留最后一次
    }

    let ctx = ctx_arc(ctx);
    let task_deb = Arc::clone(&deb);
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(VIEWPORT_DEBOUNCE) => {}
        }
        let latest = task_deb.pending.lock().unwrap_or_else(lock_poisoned).take();
        if let Some(vp) = latest {
            apply_viewport(&ctx, hwnd, fit, &vp).await;
        }
    });
    *deb.abort.lock().unwrap_or_else(lock_poisoned) = Some(task.abort_handle());
    Ok(())
}

/// 执行 1:1 适配（架构 §3.1）：fit != FIT 时不动远端（控制端本地缩放）；
/// Partial/NotResizable 回 `Error{code:12}`（非致命），由控制端转本地缩放。
async fn apply_viewport(ctx: &SessionCtx, window_id: u64, fit: i32, vp: &ViewportInfo) {
    if fit != FitMode::Fit as i32 {
        return;
    }
    let (target_w, target_h) = target_client_size(vp.css_w, vp.css_h, vp.dpr);
    match fit_window(window_id as isize, target_w, target_h) {
        Ok(FitResult::Exact) => {
            tracing::info!(
                window = format!("{window_id:#x}"),
                w = target_w,
                h = target_h,
                "窗口客户区已适配视口"
            );
        }
        Ok(FitResult::Partial { actual_w, actual_h }) => {
            tracing::warn!(
                window = format!("{window_id:#x}"),
                target = format!("{target_w}x{target_h}"),
                actual = format!("{actual_w}x{actual_h}"),
                "窗口尺寸调整受程序限制，退化本地缩放"
            );
            let _ = send_error(
                ctx,
                0,
                ERR_FIT_DEGRADED,
                format!("窗口无法调整到 {target_w}×{target_h}（实际 {actual_w}×{actual_h}），已退化为本地缩放"),
            )
            .await;
        }
        Ok(FitResult::NotResizable) => {
            tracing::warn!(window = format!("{window_id:#x}"), "窗口不可调整尺寸");
            let _ = send_error(
                ctx,
                0,
                ERR_FIT_DEGRADED,
                "窗口不可调整尺寸，已退化为本地缩放",
            )
            .await;
        }
        Err(err) => {
            tracing::warn!(
                window = format!("{window_id:#x}"),
                "fit_window 失败: {err:#}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 管线注册表条目与统一关闭
// ---------------------------------------------------------------------------

/// 一个窗口管线的句柄：shutdown 统一取消、join 并按需恢复布局。
struct PipeHandle {
    cancel: CancellationToken,
    capture: Option<WindowCapture>,
    enc_thread: Option<JoinHandle<()>>,
    saved: Option<SavedPlacement>,
    hwnd: u64,
    /// 打开时的适配模式（proto 枚举原始 i32）。
    fit: i32,
    deb: Arc<ViewportDeb>,
    /// 打开该窗口的连接标识（SessionCtx 地址）；断开清理只动自己的窗口。
    owner: usize,
}

impl PipeHandle {
    /// 停止管线：取消令牌 → 捕获 drop（通道关闭）→ join 编码线程。
    /// `restore` 为 true 时恢复会话前的窗口布局。
    fn shutdown(mut self, restore: bool) {
        self.cancel.cancel();
        if let Some(a) = self.deb.abort.lock().unwrap_or_else(lock_poisoned).take() {
            a.abort();
        }
        drop(self.capture.take()); // WindowCapture::Drop 关闭帧池与会话
        if let Some(t) = self.enc_thread.take() {
            match t.join() {
                Ok(()) => tracing::debug!(hwnd = format!("{:#x}", self.hwnd), "编码线程已 join"),
                Err(_) => tracing::warn!(hwnd = format!("{:#x}", self.hwnd), "编码线程 panic"),
            }
        }
        if restore {
            restore_saved(self.hwnd as isize, &self.saved);
        }
    }
}

/// 单窗口管线的一次性运行参数（编码线程打包入参）。
struct PipeRun {
    frame_rx: crossbeam_channel::Receiver<CapturedFrame>,
    au_tx: mpsc::Sender<(u32, EncodedAu)>,
    tile_tx: mpsc::Sender<(u32, TileOut)>,
    state: Arc<AgentState>,
    fps: u32,
    kbps: u32,
    refine: bool,
    errors: Arc<PipeError>,
    cancel: CancellationToken,
}

/// 管线故障槽：编码线程写入，发送任务收尾时读取并转 `Error` 回控制端。
#[derive(Default)]
struct PipeError(Mutex<Option<(u32, String)>>);

impl PipeError {
    fn set(&self, code: u32, message: String) {
        *self.0.lock().unwrap_or_else(lock_poisoned) = Some((code, message));
    }

    fn take(&self) -> Option<(u32, String)> {
        self.0.lock().unwrap_or_else(lock_poisoned).take()
    }
}

// ---------------------------------------------------------------------------
// 编码线程：捕获帧 → H.264 AU + 无损瓦片/失效标记
// ---------------------------------------------------------------------------

/// 编码线程主循环（参照 spike.rs 管线）。
///
/// 退出条件：取消令牌、捕获通道关闭（capture drop）、发送端关闭、编码出错
/// （写入 [`PipeError`] 后退出，由发送任务收尾上报）。
fn encode_loop(run: PipeRun) {
    let PipeRun {
        frame_rx,
        au_tx,
        tile_tx,
        state,
        fps,
        kbps,
        refine,
        errors,
        cancel,
    } = run;
    let mut pipe = FramePipeline::new(fps, kbps, refine, true);
    let budget = tile_budget_bytes(kbps);
    let mut frame_seq: u32 = 0;
    // 最近一帧（瓦片 tick 需要；refine 关闭时不保留，省一帧内存）
    let mut last: Option<(u32, RawFrame)> = None;
    let mut next_tick = Instant::now() + TILE_TICK;

    loop {
        if cancel.is_cancelled() {
            return;
        }
        let wait = next_tick
            .saturating_duration_since(Instant::now())
            .max(SLEEP_MIN);
        match frame_rx.recv_timeout(wait) {
            Ok(frame) => {
                frame_seq = frame_seq.wrapping_add(1);
                if state.paused.load(Ordering::Relaxed) {
                    tracing::trace!("会话暂停，丢弃捕获帧");
                    last = None; // 暂停期间不出帧（瓦片一并停）
                    continue;
                }
                let raw = RawFrame {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.stride as i32,
                    pts_us: unix_us(),
                    data: frame.data,
                };
                let dirty = dirty_rects(frame.dirty);
                match pipe.process(&raw, dirty.as_deref(), Instant::now()) {
                    Ok(step) => {
                        if let Some(au) = step.au {
                            if au_tx.blocking_send((frame_seq, au)).is_err() {
                                return; // 视频发送任务已退出
                            }
                        }
                        for t in step.invalidates {
                            if tile_tx.blocking_send((frame_seq, t)).is_err() {
                                return; // 瓦片发送任务已退出
                            }
                        }
                        if refine {
                            last = Some((frame_seq, raw));
                        }
                    }
                    Err(err) => {
                        tracing::error!(window_seq = frame_seq, "编码失败: {err:#}");
                        errors.set(ERR_NOT_CAPTURABLE, format!("视频管线异常退出: {err:#}"));
                        return;
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if now < next_tick {
                    continue; // 被关闭信号提前唤醒，等下一个 tick 点
                }
                next_tick = now + TILE_TICK;
                let Some((seq, raw)) = last.as_ref() else {
                    continue;
                };
                for t in pipe.tick(raw, now, budget) {
                    if tile_tx.blocking_send((*seq, t)).is_err() {
                        return;
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return; // 捕获会话已关闭（正常关闭路径）
            }
        }
    }
}

/// 瓦片 tick 字节预算 = 当前码率 × [`TILE_BUDGET_RATIO`] × tick 周期(0.05 s)。
fn tile_budget_bytes(kbps: u32) -> usize {
    (f64::from(kbps) * 1000.0 / 8.0 * TILE_BUDGET_RATIO * TILE_TICK.as_secs_f64()) as usize
}

/// 捕获脏区（屏幕/帧坐标，i32 原点）→ 编码脏区（客户区坐标，u32）。
fn dirty_rects(rects: Option<Vec<rd_capture_win::Rect>>) -> Option<Vec<DirtyRect>> {
    rects.map(|rs| {
        rs.into_iter()
            .map(|r| DirtyRect {
                x: r.x.max(0) as u32,
                y: r.y.max(0) as u32,
                w: r.w,
                h: r.h,
            })
            .collect()
    })
}

/// 一帧处理结果：视频 AU 与失效标记瓦片。
struct FrameStep {
    au: Option<EncodedAu>,
    invalidates: Vec<TileOut>,
}

/// 捕获帧 → H.264 AU + 无损瓦片的可测核心（不含捕获/传输 IO）。
///
/// 尺寸变化检测与 spike.rs 同款（`CapturedFrame.resized` 实证不可靠，T-006，
/// 以帧尺寸差量为准）；变化时 `reconfigure` + 重建 [`TileRefiner`]，下一帧
/// 强制 IDR。`refine = false` 时跳过全部瓦片路径。
struct FramePipeline {
    fps: u32,
    kbps: u32,
    refine: bool,
    prefer_hw: bool,
    encoder: Option<Box<dyn VideoEncoder>>,
    refiner: Option<TileRefiner>,
    last_size: Option<(u32, u32)>,
    force_key: bool,
}

impl FramePipeline {
    fn new(fps: u32, kbps: u32, refine: bool, prefer_hw: bool) -> Self {
        Self {
            fps,
            kbps,
            refine,
            prefer_hw,
            encoder: None,
            refiner: None,
            last_size: None,
            force_key: true, // 首帧 IDR
        }
    }

    /// 当前编码尺寸（首帧后为 Some；供单测断言尺寸重建）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn size(&self) -> Option<(u32, u32)> {
        self.last_size
    }

    /// 处理一帧：脏块比对（失效回收）→ 尺寸变化处理 → 编码。
    fn process(
        &mut self,
        raw: &RawFrame,
        dirty: Option<&[DirtyRect]>,
        now: Instant,
    ) -> anyhow::Result<FrameStep> {
        let size_changed = self.last_size != Some((raw.width, raw.height));
        if size_changed {
            match self.encoder.as_mut() {
                Some(enc) => enc
                    .reconfigure(raw.width, raw.height)
                    .context("编码器 reconfigure 失败")?,
                None => {
                    let cfg = EncoderConfig {
                        width: raw.width,
                        height: raw.height,
                        fps: self.fps,
                        bitrate_kbps: self.kbps,
                        prefer_hw: self.prefer_hw,
                    };
                    let enc = create_best(&cfg).context("创建视频编码器失败")?;
                    self.encoder = Some(enc);
                }
            }
            self.force_key = true;
            self.last_size = Some((raw.width, raw.height));
            self.refiner = self
                .refine
                .then(|| TileRefiner::new(raw.width, raw.height, TILE_STILL_MS));
            tracing::info!(
                width = raw.width,
                height = raw.height,
                "窗口尺寸变化：编码器已 reconfigure（下一帧 IDR），瓦片精修器已重建"
            );
        }

        let invalidates = match self.refiner.as_mut() {
            Some(r) => r.on_frame(raw, dirty, now),
            None => Vec::new(),
        };
        let enc = self
            .encoder
            .as_mut()
            .expect("encoder created on first frame");
        let au = enc.encode(raw, std::mem::take(&mut self.force_key))?;
        Ok(FrameStep { au, invalidates })
    }

    /// 瓦片 tick：静止成熟的块在预算内编码下发（refine 关闭时恒空）。
    fn tick(&mut self, raw: &RawFrame, now: Instant, budget_bytes: usize) -> Vec<TileOut> {
        match self.refiner.as_mut() {
            Some(r) => r.tick(raw, now, budget_bytes, None),
            None => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 发送任务与收尾
// ---------------------------------------------------------------------------

async fn video_send_task(
    ctx: Arc<SessionCtx>,
    cancel: CancellationToken,
    mut tx: DataTx,
    mut rx: mpsc::Receiver<(u32, EncodedAu)>,
    window_id: u64,
    errors: Arc<PipeError>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                None => break, // 编码线程退出（通道关闭）
                Some((seq, au)) => {
                    let header = VideoHeader {
                        window_id,
                        seq,
                        pts_us: au.pts_us,
                        keyframe: au.keyframe,
                    };
                    if let Err(err) = tx.send_video(&header, &au.data).await {
                        tracing::warn!(window = format!("{window_id:#x}"), "视频发送失败: {err:#}");
                        break;
                    }
                }
            }
        }
    }
    tracing::debug!(window = format!("{window_id:#x}"), "视频发送任务退出");
    pipe_cleanup(&ctx, window_id, errors).await;
}

async fn tile_send_task(
    ctx: Arc<SessionCtx>,
    cancel: CancellationToken,
    mut tx: DataTx,
    mut rx: mpsc::Receiver<(u32, TileOut)>,
    window_id: u64,
    errors: Arc<PipeError>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                None => break,
                Some((frame_seq, tile)) => {
                    let header = TileHeader {
                        window_id,
                        frame_seq,
                        x: tile.x,
                        y: tile.y,
                        w: tile.w,
                        h: tile.h,
                        fmt: tile.fmt,
                    };
                    if let Err(err) = tx.send_tile(&header, &tile.data).await {
                        tracing::warn!(window = format!("{window_id:#x}"), "瓦片发送失败: {err:#}");
                        break;
                    }
                }
            }
        }
    }
    tracing::debug!(window = format!("{window_id:#x}"), "瓦片发送任务退出");
    pipe_cleanup(&ctx, window_id, errors).await;
}

/// 发送任务收尾：从注册表移除该窗口（拿到句柄者负责统一 shutdown/恢复布局），
/// 管线自亡时把故障转为 `Error{code:11}` 回给控制端。两个发送任务并发收尾，
/// 以注册表 remove 的互斥保证 shutdown 与错误上报各只执行一次。
async fn pipe_cleanup(ctx: &SessionCtx, window_id: u64, errors: Arc<PipeError>) {
    let handle = pipes()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .remove(&window_id);
    if let Some(handle) = handle {
        handle.shutdown(true);
    }
    if let Some((code, message)) = errors.take() {
        let _ = ctx.send(Msg::Error(Error { code, message }), 0).await;
    }
}

/// Unix epoch 起的微秒时间戳（发送端时钟；控制端据此估算端到端延迟）。
fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 单元测试：合成 BGRA 帧直接驱动 编码+瓦片 核心（不启动 WGC/QUIC）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 均匀填充的合成 BGRA 帧。
    fn frame(w: u32, h: u32, fill: u8, pts_us: u64) -> RawFrame {
        let mut f = RawFrame::new(w, h, pts_us);
        for px in f.data.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[fill, fill, fill, 255]);
        }
        f
    }

    /// 软编管线（测试不探测硬编，稳定且快）。
    fn pipe() -> FramePipeline {
        FramePipeline::new(30, 4000, true, false)
    }

    #[test]
    fn first_frame_is_idr() {
        let mut p = pipe();
        let f = frame(320, 240, 10, 0);
        let step = p.process(&f, None, Instant::now()).expect("首帧处理成功");
        let au = step.au.expect("首帧应有 AU 输出");
        assert!(au.keyframe, "首帧必须为 IDR keyframe");
        assert_eq!(p.size(), Some((320, 240)));
    }

    #[test]
    fn tiles_emitted_only_after_still_300ms() {
        let mut p = pipe();
        let t0 = Instant::now();
        let f = frame(320, 240, 10, 0);
        let step = p.process(&f, None, t0).expect("处理成功");
        assert!(step.invalidates.is_empty(), "首帧初始化不产失效");

        // 相同内容再喂一帧（等价于画面静止后的空转）
        let _ = p.process(&f, None, t0).expect("处理成功");

        let big_budget = 1 << 20;
        let tiles = p.tick(&f, t0 + Duration::from_millis(100), big_budget);
        assert!(tiles.is_empty(), "静止未满 300ms 不应产瓦片");

        let tiles = p.tick(&f, t0 + Duration::from_millis(301), big_budget);
        assert!(!tiles.is_empty(), "静止 300ms 后应产出无损瓦片");
        assert!(
            tiles
                .iter()
                .all(|t| t.fmt != rd_encode::tiles::FMT_INVALIDATE),
            "tick 产物应为数据瓦片而非失效标记"
        );
    }

    #[test]
    fn resize_reconfigures_and_forces_idr() {
        let mut p = pipe();
        let t0 = Instant::now();
        let f1 = frame(320, 240, 10, 0);
        let step = p.process(&f1, None, t0).expect("首帧成功");
        assert!(step.au.expect("AU").keyframe);

        let f2 = frame(640, 480, 20, 1);
        let step = p.process(&f2, None, t0).expect("尺寸变化帧成功");
        assert!(
            step.au.expect("AU").keyframe,
            "尺寸变化后必须重建关键帧（reconfigure → IDR）"
        );
        assert_eq!(p.size(), Some((640, 480)));
    }

    #[test]
    fn refine_disabled_skips_tiles() {
        let mut p = FramePipeline::new(30, 4000, false, false);
        let t0 = Instant::now();
        let f = frame(320, 240, 10, 0);
        p.process(&f, None, t0).expect("处理成功");
        let tiles = p.tick(&f, t0 + Duration::from_millis(400), 1 << 20);
        assert!(tiles.is_empty(), "refine 关闭时不应产出任何瓦片");
    }

    #[test]
    fn change_after_refine_produces_invalidation() {
        let mut p = pipe();
        let t0 = Instant::now();
        let f = frame(320, 240, 10, 0);
        p.process(&f, None, t0).expect("处理成功");
        let _ = p.process(&f, None, t0).expect("处理成功");
        let big_budget = 1 << 20;
        let tiles = p.tick(&f, t0 + Duration::from_millis(301), big_budget);
        assert!(!tiles.is_empty(), "先让区域进入已精修状态");

        // 内容整体变化 → 已精修块回滚为脏，应产出失效标记
        let f2 = frame(320, 240, 77, 2);
        let step = p.process(&f2, None, t0).expect("处理成功");
        assert!(
            step.invalidates
                .iter()
                .all(|t| t.fmt == rd_encode::tiles::FMT_INVALIDATE),
            "内容变化后应只产出失效标记"
        );
        assert!(!step.invalidates.is_empty(), "失效标记不应为空");
    }

    #[test]
    fn tile_budget_matches_spec() {
        // 预算 = 码率 × 50% × 0.05s：8000 kbps → 25_000 B
        assert_eq!(tile_budget_bytes(8000), 25_000);
        assert_eq!(tile_budget_bytes(0), 0);
    }
}
