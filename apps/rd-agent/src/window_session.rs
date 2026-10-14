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
//! `WindowEvent` 推送（顶层窗口事件；子窗口/菜单事件由窗口组轮询改以
//! CHILD_ADDED/CHILD_REMOVED/MOVED 形式上报，见下）。
//!
//! ## 窗口组与码率自适应（T-027）
//! 每连接一个会话 tick 任务（首次窗口消息时启动）：
//! - **组轮询**（250 ms 周期；MenuStart/Created 事件经 `Notify` 提前触发）：
//!   对本连接每个已打开根窗口求 [`window_group`] 并与上次快照 diff——
//!   新增成员即发 `CHILD_ADDED`（x/y 为相对根客户区偏移；事件与管线建立
//!   解耦，FIX-1 缺陷 2），随后尝试开独立子管线，失败（含菜单类 `#32768`
//!   不可捕获的 Windows 硬限制）则无视频流降级；子窗口一律不做 Fit、菜单
//!   类不精修；消失成员发 `CHILD_REMOVED` 并停管线；偏移变化发 `MOVED`。
//!   子窗口 id 进组成员表（与管线成败无关），`is_open` 对其返回 true
//!   （T-017 的点击可作用于菜单）。
//! - **码率自适应**（200 ms 周期）：每会话一个 [`RateController`]，以
//!   `conn.rtt()`、发送在途字节（视频/瓦片发送任务在途记账）、QUIC 丢包
//!   差分更新，决策码率按各窗口客户区面积比例落到全部编码器
//!   （`set_bitrate`），建议帧率在编码线程做节流丢帧。
//! - **StatsReport**（每 1 s）：fps（编码 AU 计数）、rtt_ms、kbps（实际
//!   发送字节差分），req_id=0 经控制流发送。
//!
//! 注：`handle` 内的工作为异步执行，同连接连续两条窗口消息的到达顺序
//! 不保证与协议顺序严格一致（tokio 调度粒度）；控制端按协议不应在
//! 未收到 ack 前对同一窗口发后续窗口消息，此为 M1 的既定约束。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use rd_capture_win::{CaptureOptions, CapturedFrame, WindowCapture};
use rd_encode::caps::{negotiate, probe_encode_caps};
use rd_encode::dirty::Rect as DirtyRect;
use rd_encode::frame::RawFrame;
use rd_encode::rate::RateController;
use rd_encode::rate::RateInput;
use rd_encode::tiles::{TileOut, TileRefiner};
use rd_encode::{create_best, EncodedAu, EncoderConfig, VideoEncoder};
use rd_proto::v0::envelope::Msg;
use rd_proto::v0::window_event::Kind;
use rd_proto::v0::{
    CloseWindow, Codec, Error, FitMode, ListWindows, OpenWindow, OpenWindowAck, RenderLevel,
    StatsReport, ViewportInfo, WindowEvent, WindowInfo, WindowList,
};
use rd_transport::framing::{TILE_HEADER_LEN, VIDEO_HEADER_LEN};
use rd_transport::{DataTx, StreamKind, TileHeader, VideoHeader};
use rd_window_win::events::{spawn_event_watcher, WatcherHandle, WindowEventRaw};
use rd_window_win::resize::{fit_window, restore_placement, save_placement, target_client_size};
use rd_window_win::resize::{FitResult, SavedPlacement};
use rd_window_win::{list_windows, window_group, window_icon_png, GroupMember, WindowAttrs};
use tokio::sync::{broadcast, mpsc, Notify};
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
/// 窗口组轮询周期（T-027 步骤 1）。
const GROUP_POLL: Duration = Duration::from_millis(250);
/// 码率自适应反馈周期（T-011：RateController 每 200 ms 消费一条反馈）。
const RATE_TICK: Duration = Duration::from_millis(200);
/// StatsReport 周期（T-027 步骤 5：每 1 s）。
const STATS_PERIOD: Duration = Duration::from_secs(1);
/// 弹出菜单窗口类名（rd-window-win group.rs 的 MENU_CLASS 未跨 crate 导出，
/// 此处按同一常量复刻；修改须两处同步）。
const MENU_CLASS: &str = "#32768";

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
/// 连接标识 → 会话 tick 任务句柄（组轮询 / 码率自适应 / StatsReport）。
static SERVICES: OnceLock<Mutex<HashMap<usize, ServiceEntry>>> = OnceLock::new();
/// 连接标识 → 该连接窗口组内的子窗口 id 集合（is_open 对子窗口放行）。
static CHILD_MEMBERS: OnceLock<Mutex<HashMap<usize, HashSet<u64>>>> = OnceLock::new();

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

fn services() -> &'static Mutex<HashMap<usize, ServiceEntry>> {
    SERVICES.get_or_init(Default::default)
}

fn child_members() -> &'static Mutex<HashMap<usize, HashSet<u64>>> {
    CHILD_MEMBERS.get_or_init(Default::default)
}

/// 每连接的会话级服务句柄：tick 任务取消令牌、组轮询提前唤醒、发送字节累计。
struct ServiceEntry {
    /// 令 tick 任务退出（断开时撤销）。
    cancel: CancellationToken,
    /// 事件订阅任务在 MenuStart/Created 时 notify，组轮询立即跑一轮（菜单
    /// 弹出 300 ms 内出现的时效保证；250 ms 轮询为兜底）。
    kick: Arc<Notify>,
    /// 本会话（连接）累计发送字节（视频+瓦片；StatsReport.kbps 的数据源）。
    bytes_sent: Arc<AtomicU64>,
}

/// 幂等启动本连接的会话 tick 任务，返回共享的发送字节计数器。
///
/// 首条窗口类消息（List/Open）时调用；任务随断开（`on_disconnect`）取消。
fn ensure_services(ctx: &Arc<SessionCtx>) -> Arc<AtomicU64> {
    let key = ctx_key(ctx);
    let mut map = services().lock().unwrap_or_else(lock_poisoned);
    if let Some(s) = map.get(&key) {
        return Arc::clone(&s.bytes_sent);
    }
    let entry = ServiceEntry {
        cancel: CancellationToken::new(),
        kick: Arc::new(Notify::new()),
        bytes_sent: Arc::new(AtomicU64::new(0)),
    };
    let cancel = entry.cancel.clone();
    let kick = Arc::clone(&entry.kick);
    let bytes = Arc::clone(&entry.bytes_sent);
    map.insert(key, entry);
    drop(map);
    spawn_session_tick(Arc::clone(ctx), Arc::clone(&bytes), cancel, kick);
    bytes
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
    /// 指定窗口是否属于本进程的已打开会话。
    ///
    /// T-027 扩展：窗口组内的子窗口（弹出菜单/owned 对话框）也返回 true，
    /// T-017 的点击即可作用于菜单（前台窗口判定同样放行）。
    #[allow(dead_code)]
    pub fn is_open(&self, id: u64) -> bool {
        if pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .contains_key(&id)
        {
            return true;
        }
        child_members()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .values()
            .any(|set| set.contains(&id))
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

/// 会话断开：停止本连接的会话 tick 任务、打开的全部窗口管线（含子窗口），
/// 并撤销事件订阅与组成员登记。
pub fn on_disconnect(ctx: &SessionCtx) {
    let key = ctx_key(ctx);
    if let Some(s) = services().lock().unwrap_or_else(lock_poisoned).remove(&key) {
        s.cancel.cancel(); // tick 任务退出（组轮询/码率/Stats 停止）
    }
    child_members()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .remove(&key);
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
    // 窗口类首条消息：启动会话 tick（组轮询/码率/Stats）。
    ensure_services(&ctx);
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
            // 菜单弹出/窗口创建：提前唤醒本连接的组轮询（T-027，菜单时效）。
            if matches!(
                ev,
                WindowEventRaw::MenuStart(_) | WindowEventRaw::Created(_)
            ) {
                if let Some(s) = services().lock().unwrap_or_else(lock_poisoned).get(&key) {
                    s.kick.notify_one();
                }
            }
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

    let area = u64::from(attrs.rect.w).saturating_mul(u64::from(attrs.rect.h));
    let spec = PipeSpec {
        hwnd: attrs.hwnd,
        fps,
        kbps,
        refine,
        fit: open.fit,
        saved,
        is_root: true,
        area,
        owner: ctx_key(ctx),
        session_bytes: ensure_services(ctx),
    };
    let started = match start_pipeline(ctx, spec).await {
        Ok(s) => s,
        Err(err) => {
            return send_error(ctx, req_id, ERR_NOT_CAPTURABLE, format!("{err:#}")).await;
        }
    };

    // 编解码协商（T-027 步骤 4）：本机能力 ∩ 控制端能力 ∩ supports_444。
    // proto 的 repeated Codec 在 prost 生成物中为 Vec<i32>，需转回枚举。
    let decode_caps: Vec<Codec> = ctx
        .hello
        .decode_caps
        .iter()
        .filter_map(|&c| Codec::try_from(c).ok())
        .collect();
    let codec = effective_codec(negotiate(
        &probe_encode_caps(),
        &decode_caps,
        ctx.hello.supports_444,
    ));

    ctx.send(
        Msg::OpenWindowAck(OpenWindowAck {
            window_id: wid,
            level: RenderLevel::L0Pixel as i32,
            video_stream_id: started.video_stream_id,
            tile_stream_id: started.tile_stream_id,
            codec: codec as i32,
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
        ?codec,
        video_stream_id = started.video_stream_id,
        tile_stream_id = started.tile_stream_id,
        "窗口管线已建立"
    );
    Ok(())
}

/// 发送任务的共享记账状态（T-027：码率自适应的在途积压信号 + 会话字节累计）。
struct SendAccounting {
    /// 本管线在途字节（发送任务入队 +N、写完 -N）。
    backlog: Arc<AtomicU64>,
    /// 本会话累计发送字节（StatsReport.kbps 数据源）。
    session_bytes: Arc<AtomicU64>,
}

/// 一条窗口管线的搭建参数（根窗口 OpenWindow 与窗口组成员共用）。
struct PipeSpec {
    hwnd: isize,
    fps: u32,
    /// 初始码率（kbps）；随后由会话 RateController 动态覆盖。
    kbps: u32,
    refine: bool,
    /// 适配模式（proto 枚举原始 i32）；子窗口恒 SCALE（不做 Fit）。
    fit: i32,
    /// 会话前布局（根 Some；子窗口 None，关闭时不恢复）。
    saved: Option<SavedPlacement>,
    /// 根窗口（组轮询对象、可接收 Viewport）；子窗口 false。
    is_root: bool,
    /// 客户区面积（码率按面积比例分配的权重）。
    area: u64,
    /// 打开者连接标识。
    owner: usize,
    /// 所属会话的发送字节累计（StatsReport 用）。
    session_bytes: Arc<AtomicU64>,
}

/// 管线搭建结果（供 ack / CHILD_ADDED 使用的流 id）。
struct PipeStarted {
    video_stream_id: u32,
    tile_stream_id: u32,
}

/// 搭建一条完整管线：捕获 → 数据流 → 编码线程 → 双发送任务 → 注册表。
///
/// 失败时清理已建资源（捕获/流 finish）并把错误原样上抛；错误回包由
/// 调用方决定（根窗口回 `Error{11}`，子窗口仅记日志降级）。
async fn start_pipeline(ctx: &Arc<SessionCtx>, spec: PipeSpec) -> anyhow::Result<PipeStarted> {
    let wid = spec.hwnd as u64;
    let restore_on_fail = |s: &PipeSpec| restore_saved(s.hwnd, &s.saved);
    let started_capture = WindowCapture::start(spec.hwnd, CaptureOptions { max_fps: spec.fps })
        .with_context(|| format!("启动捕获失败（hwnd {wid:#x}）"));
    let (capture, frame_rx) = match started_capture {
        Ok(v) => v,
        Err(err) => {
            restore_on_fail(&spec);
            return Err(err);
        }
    };

    let mut video_tx = match ctx.conn.open_data(StreamKind::Video).await {
        Ok(t) => t,
        Err(err) => {
            drop(capture);
            restore_on_fail(&spec);
            anyhow::bail!("打开视频流失败: {err:#}");
        }
    };
    let mut tile_tx = match ctx.conn.open_data(StreamKind::Tile).await {
        Ok(t) => t,
        Err(err) => {
            drop(capture);
            let _ = video_tx.finish();
            restore_on_fail(&spec);
            anyhow::bail!("打开瓦片流失败: {err:#}");
        }
    };
    let video_stream_id = u32::try_from(video_tx.id()).unwrap_or(u32::MAX);
    let tile_stream_id = u32::try_from(tile_tx.id()).unwrap_or(u32::MAX);

    let cancel = CancellationToken::new();
    let errors = Arc::new(PipeError::default());
    let (au_tx, au_rx) = mpsc::channel(AU_CHANNEL_CAP);
    let (tile_out_tx, tile_rx) = mpsc::channel(TILE_CHANNEL_CAP);
    let backlog = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let target_kbps = Arc::new(AtomicU32::new(spec.kbps));
    let fps_cap = Arc::new(AtomicU32::new(spec.fps));
    // 根窗口预置组快照（仅含根自身）：首次轮询 diff 时根不会误报为新增。
    let group_snapshot = spec.is_root.then(|| {
        Arc::new(Mutex::new(HashMap::from([(
            spec.hwnd,
            GroupMember {
                hwnd: spec.hwnd,
                offset_x: 0,
                offset_y: 0,
                w: 0,
                h: 0,
            },
        )])))
    });

    let enc_thread = {
        let run = PipeRun {
            frame_rx,
            au_tx,
            tile_tx: tile_out_tx,
            state: Arc::clone(&ctx.state),
            fps: spec.fps,
            kbps: spec.kbps,
            refine: spec.refine,
            errors: Arc::clone(&errors),
            cancel: cancel.clone(),
            target_kbps: Arc::clone(&target_kbps),
            fps_cap: Arc::clone(&fps_cap),
            frames: Arc::clone(&frames),
        };
        match std::thread::Builder::new()
            .name(format!("rd-enc-{wid:#x}"))
            .spawn(move || encode_loop(run))
        {
            Ok(t) => t,
            Err(err) => {
                drop(capture);
                let _ = video_tx.finish();
                let _ = tile_tx.finish();
                restore_on_fail(&spec);
                anyhow::bail!("启动编码线程失败: {err}");
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
        SendAccounting {
            backlog: Arc::clone(&backlog),
            session_bytes: Arc::clone(&spec.session_bytes),
        },
    ));
    tokio::spawn(tile_send_task(
        Arc::clone(ctx),
        cancel.clone(),
        tile_tx,
        tile_rx,
        wid,
        errors,
        SendAccounting {
            backlog: Arc::clone(&backlog),
            session_bytes: Arc::clone(&spec.session_bytes),
        },
    ));

    pipes().lock().unwrap_or_else(lock_poisoned).insert(
        wid,
        PipeHandle {
            cancel: cancel.clone(),
            capture: Some(capture),
            enc_thread: Some(enc_thread),
            saved: spec.saved,
            hwnd: wid,
            fit: spec.fit,
            deb: Arc::new(ViewportDeb::default()),
            owner: spec.owner,
            is_root: spec.is_root,
            area: spec.area,
            backlog,
            frames,
            target_kbps,
            fps_cap,
            members: group_snapshot,
        },
    );

    Ok(PipeStarted {
        video_stream_id,
        tile_stream_id,
    })
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
    /// 根窗口（组轮询对象、Viewport 接受者）；子窗口（组成员）false。
    is_root: bool,
    /// 客户区面积（码率按面积比例分配的权重）。
    area: u64,
    /// 发送在途字节（发送任务入队 +N、写完 -N；码率自适应的积压信号）。
    backlog: Arc<AtomicU64>,
    /// 累计产出 AU 帧数（StatsReport.fps 的数据源）。
    frames: Arc<AtomicU64>,
    /// 会话 RateController 分配给本管线的目标码率（kbps）。
    target_kbps: Arc<AtomicU32>,
    /// 会话建议帧率上限（编码线程节流）。
    fps_cap: Arc<AtomicU32>,
    /// 根窗口的窗口组快照（hwnd → 成员，diff 基准）；子窗口 None。
    members: Option<Arc<Mutex<HashMap<isize, GroupMember>>>>,
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
    /// 会话分配的目标码率（kbps）；变化时经 `set_bitrate` 落到编码器。
    target_kbps: Arc<AtomicU32>,
    /// 会话建议帧率上限（低于捕获帧率时丢帧节流）。
    fps_cap: Arc<AtomicU32>,
    /// 累计产出 AU 帧数（StatsReport 用）。
    frames: Arc<AtomicU64>,
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
///
/// T-027：每帧与每瓦片 tick 前应用会话码率决策（`set_bitrate`）；建议帧率
/// 低于捕获帧率时按时间间隔丢帧节流（事件驱动管线，节流在编码侧执行）。
///
/// FIX-1（D-011 缺陷 1）：瓦片 tick 是时间驱动，不能挂在 `recv_timeout` 的
/// Timeout 分支上——WGC 对常显窗口会以不低于编码消费的节奏持续推帧，此时
/// recv 每次都立即取到帧、永不超时，tick 被饿死（瓦片与失效标记全程零
/// 产出）。tick 检查按绝对期限放在循环顶部执行，`recv_timeout(wait)` 只是
/// "最多等到下个 tick"的有界等待，帧到得再密也不影响 tick 节拍。
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
        target_kbps,
        fps_cap,
        frames,
    } = run;
    let mut pipe = FramePipeline::new(fps, kbps, refine, true);
    let mut applied_kbps = kbps;
    let mut frame_seq: u32 = 0;
    // 最近一帧（瓦片 tick 需要；refine 关闭时不保留，省一帧内存）
    let mut last: Option<(u32, RawFrame)> = None;
    let mut next_tick = Instant::now() + TILE_TICK;
    // 上次产出 AU 的时刻（帧率节流基准）
    let mut last_encode: Option<Instant> = None;

    loop {
        if cancel.is_cancelled() {
            return;
        }
        // 瓦片 tick：按绝对期限执行（帧流持续时 recv 同样不会超时，见函数文档）
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TILE_TICK;
            // 静止期同样应用码率决策（长静止后恢复时使用新码率）
            applied_kbps = retune_pipeline(&mut pipe, &target_kbps, applied_kbps);
            let budget = tile_budget_bytes(applied_kbps);
            if let Some((seq, raw)) = last.as_ref() {
                for t in pipe.tick(raw, now, budget) {
                    if tile_tx.blocking_send((*seq, t)).is_err() {
                        return; // 瓦片发送任务已退出
                    }
                }
            }
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
                let now = Instant::now();
                // 帧率节流（T-027）：会话建议帧率低于捕获帧率时按间隔丢帧。
                // 丢帧不更新 last（瓦片基线保持为最近编码帧）。
                let cap = fps_cap.load(Ordering::Relaxed).max(1);
                if let Some(prev) = last_encode {
                    let min_gap = Duration::from_nanos(1_000_000_000 / u64::from(cap));
                    if now.duration_since(prev) < min_gap {
                        continue;
                    }
                }
                // 应用码率决策（编码器创建/重配置即用最新值）
                applied_kbps = retune_pipeline(&mut pipe, &target_kbps, applied_kbps);
                let raw = RawFrame {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.stride as i32,
                    pts_us: unix_us(),
                    data: frame.data,
                };
                let dirty = dirty_rects(frame.dirty);
                match pipe.process(&raw, dirty.as_deref(), now) {
                    Ok(step) => {
                        if let Some(au) = step.au {
                            frames.fetch_add(1, Ordering::Relaxed);
                            last_encode = Some(now);
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
                continue; // 到点与否由循环顶部的期限检查裁决
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return; // 捕获会话已关闭（正常关闭路径）
            }
        }
    }
}

/// 把会话目标码率落到管线（编码器存在时运行中调整，不存在时仅更新创建
/// 参数，首帧创建即用该值）。返回当前生效值（无变化时透传 `applied`）。
fn retune_pipeline(pipe: &mut FramePipeline, target: &AtomicU32, applied: u32) -> u32 {
    let want = target.load(Ordering::Relaxed);
    if want == 0 || want == applied {
        return applied;
    }
    pipe.retune(want);
    want
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

    /// 当前目标码率（kbps；供单测断言 retune 生效）。
    #[cfg(test)]
    fn bitrate(&self) -> u32 {
        self.kbps
    }

    /// 应用新的目标码率（码率自适应 T-027）：更新创建参数；编码器已存在
    /// 时经 `set_bitrate` 运行中调整（OpenH264 即时生效，硬编经重建）。
    /// 调整失败不致命（保持旧码率编码，下个决策周期再试）。
    fn retune(&mut self, kbps: u32) {
        if kbps == self.kbps {
            return;
        }
        self.kbps = kbps;
        if let Some(enc) = self.encoder.as_mut() {
            if let Err(err) = enc.set_bitrate(kbps) {
                tracing::warn!("编码器 set_bitrate({kbps} kbps) 失败: {err:#}");
            }
        }
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
    acct: SendAccounting,
) {
    let SendAccounting {
        backlog,
        session_bytes,
    } = acct;
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
                    // 在途字节记账（码率自适应的积压信号）：写完前计入，
                    // 完成后扣除（成功与否都扣，失败时管线即将关闭）。
                    let n = (au.data.len() + VIDEO_HEADER_LEN) as u64;
                    backlog.fetch_add(n, Ordering::Relaxed);
                    let sent = tx.send_video(&header, &au.data).await;
                    backlog.fetch_sub(n, Ordering::Relaxed);
                    if let Err(err) = sent {
                        tracing::warn!(window = format!("{window_id:#x}"), "视频发送失败: {err:#}");
                        break;
                    }
                    session_bytes.fetch_add(n, Ordering::Relaxed);
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
    acct: SendAccounting,
) {
    let SendAccounting {
        backlog,
        session_bytes,
    } = acct;
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
                    let n = (tile.data.len() + TILE_HEADER_LEN) as u64;
                    backlog.fetch_add(n, Ordering::Relaxed);
                    let sent = tx.send_tile(&header, &tile.data).await;
                    backlog.fetch_sub(n, Ordering::Relaxed);
                    if let Err(err) = sent {
                        tracing::warn!(window = format!("{window_id:#x}"), "瓦片发送失败: {err:#}");
                        break;
                    }
                    session_bytes.fetch_add(n, Ordering::Relaxed);
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
// 会话 tick：窗口组轮询 + 码率自适应 + StatsReport（T-027）
// ---------------------------------------------------------------------------

/// 会话 tick 任务：组轮询（250 ms 周期，MenuStart/Created 事件提前唤醒）、
/// 码率自适应（200 ms 周期）、StatsReport（1 s 周期）。断开时经 `cancel`
/// 退出并自清理 [`SERVICES`] 注册表。
fn spawn_session_tick(
    ctx: Arc<SessionCtx>,
    bytes_sent: Arc<AtomicU64>,
    cancel: CancellationToken,
    kick: Arc<Notify>,
) {
    tokio::spawn(async move {
        let cfg = &ctx.state.config;
        let mut rate = RateController::new(cfg.start_kbps.max(1), cfg.max_fps.max(1));
        let mut next_rate = Instant::now() + RATE_TICK;
        let mut next_group = Instant::now() + GROUP_POLL;
        let mut next_stats = Instant::now() + STATS_PERIOD;
        // QUIC 丢包差分基线（丢包率 = Δlost / Δsent）
        let path = ctx.conn.stats().path;
        let mut prev_sent = path.sent_packets;
        let mut prev_lost = path.lost_packets;
        // StatsReport 差分基线
        let mut prev_bytes = 0u64;
        let mut prev_frames = 0u64;

        loop {
            let wake = next_rate.min(next_group).min(next_stats);
            tokio::select! {
                _ = cancel.cancelled() => break,
                // 菜单弹出/窗口创建：立即跑一轮组轮询（250 ms 轮询为兜底）
                _ = kick.notified() => {
                    next_group = Instant::now();
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {}
            }
            let now = Instant::now();
            if now >= next_group {
                next_group = now + GROUP_POLL;
                poll_groups(&ctx).await;
            }
            if now >= next_rate {
                next_rate = now + RATE_TICK;
                apply_rate(&ctx, &mut rate, &mut prev_sent, &mut prev_lost);
            }
            if now >= next_stats {
                next_stats = now + STATS_PERIOD;
                send_stats(&ctx, &bytes_sent, &mut prev_bytes, &mut prev_frames).await;
            }
        }
        services()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .remove(&ctx_key(&ctx));
        tracing::debug!("会话 tick 任务退出");
    });
}

/// 一轮窗口组轮询：对本连接每个根窗口求 `window_group` 并 diff（T-027 步骤 1）。
///
/// `window_group` 内部全部为本地 Win32 调用（枚举/类名/客户区），无跨进程
/// 同步消息，不会挂起，直接在当前任务执行。
async fn poll_groups(ctx: &Arc<SessionCtx>) {
    let key = ctx_key(ctx);
    let roots: Vec<u64> = {
        pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .iter()
            .filter(|(_, h)| h.owner == key && h.is_root)
            .map(|(id, _)| *id)
            .collect()
    };
    for root in roots {
        let Some(snap) = pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .get(&root)
            .and_then(|h| h.members.clone())
        else {
            continue; // 管线刚被关闭
        };
        let members = window_group(root as isize);
        if members.is_empty() {
            // 根已销毁：子窗口随根消亡，停管线但不发 CHILD_REMOVED
            // （DESTROYED 已由事件推送路径通知控制端）。
            let stale: Vec<isize> = {
                let mut s = snap.lock().unwrap_or_else(lock_poisoned);
                let keys: Vec<isize> = s.keys().copied().filter(|&h| h != root as isize).collect();
                s.retain(|h, _| *h == root as isize);
                keys
            };
            for h in stale {
                teardown_child(ctx, root, h, false).await;
            }
            continue;
        }
        let diff = {
            let mut s = snap.lock().unwrap_or_else(lock_poisoned);
            let d = diff_group(&s, &members);
            *s = members.iter().map(|m| (m.hwnd, m.clone())).collect();
            d
        };
        for m in diff.added {
            add_child(ctx, root, &m).await;
        }
        for h in diff.removed {
            teardown_child(ctx, root, h, true).await;
        }
        for m in diff.moved {
            let info = child_window_info(&m);
            tracing::debug!(
                child = format!("{:#x}", m.hwnd),
                root = format!("{root:#x}"),
                "组成员偏移变化"
            );
            if ctx
                .send(
                    Msg::WindowEvent(WindowEvent {
                        kind: Kind::Moved as i32,
                        info: Some(info),
                        root_id: root,
                    }),
                    0,
                )
                .await
                .is_err()
            {
                return; // 连接已断
            }
        }
    }
}

/// 窗口组新增成员：先发 `CHILD_ADDED`（事件与管线建立解耦；T-027 步骤 1/2
/// 与 FIX-1 缺陷 2），再登记组成员表并尝试建独立子管线。子窗口不做 Fit
/// （恒 SCALE）；菜单类 `#32768` 不精修；不保存布局（无 placement 可恢复）。
///
/// D-011 缺陷 2：弹出菜单窗口（`#32768`）无法被 WGC 捕获（CreateForWindow
/// 返回 E_INVALIDARG，Windows 平台硬限制），子管线注定失败——但事件必须
/// 照发（控制端"菜单可交互感知"），成员表照常登记（`is_open` 放行，T-017
/// 的点击可作用于菜单），管线失败仅记日志降级（无视频流）。事件先行同时
/// 消除了"管线先建、CHILD_ADDED 后发"的控制端首帧竞态（帧先于子窗口映射
/// 到达被丢）。成员已写入 diff 快照，消失重现前不重试（避免 250 ms 周期
/// 反复拉起失败管线）。
async fn add_child(ctx: &Arc<SessionCtx>, root_id: u64, m: &GroupMember) {
    let child = m.hwnd as u64;
    let is_menu = class_name_is_menu(m.hwnd);
    let info = child_window_info(m);
    if let Err(err) = ctx
        .send(
            Msg::WindowEvent(WindowEvent {
                kind: Kind::ChildAdded as i32,
                info: Some(info),
                root_id,
            }),
            0,
        )
        .await
    {
        tracing::debug!("CHILD_ADDED 发送失败（连接已断）: {err:#}");
        return; // 连接已断，无需再建管线
    }
    // 成员表登记与管线成败无关：is_open 对子窗口放行（点击可作用于菜单）。
    child_members()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .entry(ctx_key(ctx))
        .or_default()
        .insert(child);
    let cfg = &ctx.state.config;
    let spec = PipeSpec {
        hwnd: m.hwnd,
        fps: cfg.max_fps.max(1),
        kbps: cfg.start_kbps.max(1),
        refine: cfg.refine_enabled && !is_menu,
        fit: FitMode::Scale as i32,
        saved: None,
        is_root: false,
        area: u64::from(m.w).saturating_mul(u64::from(m.h)),
        owner: ctx_key(ctx),
        session_bytes: ensure_services(ctx),
    };
    match start_pipeline(ctx, spec).await {
        Ok(started) => {
            tracing::info!(
                child = format!("{child:#x}"),
                root = format!("{root_id:#x}"),
                menu = is_menu,
                video_stream_id = started.video_stream_id,
                tile_stream_id = started.tile_stream_id,
                "窗口组成员已接入（独立管线）"
            );
        }
        Err(err) => {
            // 含菜单类不可捕获（Windows 硬限制）：事件已发、无视频流降级。
            tracing::warn!(
                child = format!("{child:#x}"),
                root = format!("{root_id:#x}"),
                "子窗口管线建立失败（无视频流降级，成员消失重现前不重试）: {err:#}"
            );
        }
    }
}

/// 窗口组成员消失：停管线（不恢复布局）并移出组成员表；`notify` 为 true
/// 时发 `CHILD_REMOVED`（根销毁引发的批量清理不发——DESTROYED 已另行通知）。
async fn teardown_child(ctx: &Arc<SessionCtx>, root_id: u64, child: isize, notify: bool) {
    let handle = pipes()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .remove(&(child as u64));
    if let Some(h) = handle {
        h.shutdown(false); // 子窗口无 saved，restore 为 no-op
    }
    if let Some(set) = child_members()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .get_mut(&ctx_key(ctx))
    {
        set.remove(&(child as u64));
    }
    if notify {
        let info = WindowInfo {
            id: child as u64,
            ..WindowInfo::default()
        };
        let _ = ctx
            .send(
                Msg::WindowEvent(WindowEvent {
                    kind: Kind::ChildRemoved as i32,
                    info: Some(info),
                    root_id,
                }),
                0,
            )
            .await;
    }
    tracing::debug!(
        child = format!("{child:#x}"),
        root = format!("{root_id:#x}"),
        "窗口组成员已移除"
    );
}

/// 窗口组成员 → 协议 `WindowInfo`：x/y 为相对根客户区偏移（物理像素），
/// 供控制端按相对坐标合成（架构 §3.3 M1）。仅填 id/几何字段：跨进程的
/// `GetWindowText` 类查询可能等待目标窗口，菜单弹出时效优先，不做。
fn child_window_info(m: &GroupMember) -> WindowInfo {
    WindowInfo {
        id: m.hwnd as u64,
        title: String::new(),
        process: String::new(),
        pid: 0,
        icon_png: Vec::new(),
        x: m.offset_x,
        y: m.offset_y,
        w: m.w,
        h: m.h,
        dpi: 0,
        resizable: false,
        minimized: false,
    }
}

/// 窗口类名是否为弹出菜单（`#32768`）。`GetClassNameW` 为本地查询，不挂起。
fn class_name_is_menu(hwnd: isize) -> bool {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf = [0u16; 32];
    let h = HWND(hwnd as *mut c_void);
    let len = unsafe { GetClassNameW(h, &mut buf) };
    let n = (len.max(0) as usize).min(buf.len());
    String::from_utf16_lossy(&buf[..n]) == MENU_CLASS
}

/// 窗口组 diff 结果。
struct GroupDiff {
    /// 本次新出现的成员（保持 Z 序）。
    added: Vec<GroupMember>,
    /// 已消失成员的 hwnd。
    removed: Vec<isize>,
    /// 相对根客户区偏移变化的成员（携带新偏移/尺寸，保持 Z 序）。
    moved: Vec<GroupMember>,
}

/// 上次组快照 → 本次组列表的 diff（纯函数，供单测；T-027 步骤 1）。
///
/// - 新 hwnd → `added`；
/// - 消失 hwnd → `removed`；
/// - 偏移（x/y）变化 → `moved`（尺寸变化不单独算，随 moved 事件顺带刷新；
///   管线内部以帧尺寸差量自行处理编码器重配置）。
fn diff_group(prev: &HashMap<isize, GroupMember>, next: &[GroupMember]) -> GroupDiff {
    let next_ids: HashSet<isize> = next.iter().map(|m| m.hwnd).collect();
    let mut added = Vec::new();
    let mut moved = Vec::new();
    for m in next {
        match prev.get(&m.hwnd) {
            None => added.push(m.clone()),
            Some(p) if p.offset_x != m.offset_x || p.offset_y != m.offset_y => {
                moved.push(m.clone());
            }
            _ => {}
        }
    }
    let removed: Vec<isize> = prev
        .keys()
        .copied()
        .filter(|h| !next_ids.contains(h))
        .collect();
    GroupDiff {
        added,
        removed,
        moved,
    }
}

/// 会话总码率按窗口客户区面积比例分配（纯函数，供单测；T-027 步骤 3）。
///
/// 每份下限 1 kbps、上限为总额（比例取整不超发）；面积和为 0（异常）时
/// 均分总额。
fn allocate_kbps(total: u32, areas: &[u64]) -> Vec<u32> {
    let total = total.max(1);
    let sum: u64 = areas.iter().sum();
    if sum == 0 {
        return vec![total; areas.len()];
    }
    areas
        .iter()
        .map(|&a| ((u64::from(total) * a / sum).max(1).min(u64::from(total))) as u32)
        .collect()
}

/// 协商结果的"实际可用"编解码器（T-027 步骤 4）。
///
/// [`rd_encode::caps::negotiate`] 依据双方能力选出目标；当前 rd-encode 的
/// 编码器创建路径（`create_best`/`FfmpegEncoder`，NV12 输入、字段私有）尚
/// 无 4:4:4 创建接口——该层不在本任务可写范围（见上报事项）。此时必须
/// 诚实回退 `H264_420`：`OpenWindowAck.codec` 告知控制端以何种格式解码，
/// 谎报 444 会导致 WebCodecs 以错误配置建解码器而失败。rd-encode 暴露
/// 444 创建接口后改为透传协商结果即可。
fn effective_codec(negotiated: Codec) -> Codec {
    match negotiated {
        Codec::H264444 => {
            tracing::info!("协商结果 H264_444：rd-encode 暂无 4:4:4 编码创建路径，回退 H264_420");
            Codec::H264420
        }
        other => other,
    }
}

/// 一次码率自适应反馈（T-027 步骤 3，每 [`RATE_TICK`] 一次）：
/// 采样 RTT / 发送在途字节 / QUIC 丢包差分 → [`RateController`] 决策；
/// 码率按各窗口面积比例写入全部管线的 `target_kbps`（编码线程经
/// `set_bitrate` 落到编码器），建议帧率写入 `fps_cap` 做节流。
fn apply_rate(
    ctx: &SessionCtx,
    rate: &mut RateController,
    prev_sent: &mut u64,
    prev_lost: &mut u64,
) {
    let key = ctx_key(ctx);
    struct Slot {
        area: u64,
        target: Arc<AtomicU32>,
        fps_cap: Arc<AtomicU32>,
        backlog: Arc<AtomicU64>,
    }
    let slots: Vec<Slot> = {
        let map = pipes().lock().unwrap_or_else(lock_poisoned);
        map.iter()
            .filter(|(_, h)| h.owner == key)
            .map(|(_, h)| Slot {
                area: h.area,
                target: Arc::clone(&h.target_kbps),
                fps_cap: Arc::clone(&h.fps_cap),
                backlog: Arc::clone(&h.backlog),
            })
            .collect()
    };
    if slots.is_empty() {
        return; // 本会话暂无打开窗口
    }

    let path = ctx.conn.stats().path;
    let d_sent = path.sent_packets.saturating_sub(*prev_sent);
    let d_lost = path.lost_packets.saturating_sub(*prev_lost);
    *prev_sent = path.sent_packets;
    *prev_lost = path.lost_packets;
    let lost_ratio = if d_sent > 0 {
        (d_lost as f32 / d_sent as f32).min(1.0)
    } else {
        0.0
    };

    let rtt_ms = u32::try_from(ctx.conn.rtt().as_millis()).unwrap_or(u32::MAX);
    // 在途字节：视频/瓦片发送任务在 QUIC 写入期间的未完成量（应用层积压；
    // 慢网络使 send await 阻塞 → AU 通道排队 → 该值上升）
    let queued: usize = slots
        .iter()
        .map(|s| s.backlog.load(Ordering::Relaxed))
        .sum::<u64>() as usize;
    let dec = rate.update(RateInput {
        rtt_ms,
        queued_bytes: queued,
        lost_ratio,
    });

    let areas: Vec<u64> = slots.iter().map(|s| s.area).collect();
    let shares = allocate_kbps(dec.kbps, &areas);
    for (s, kbps) in slots.iter().zip(shares) {
        s.target.store(kbps, Ordering::Relaxed);
        s.fps_cap.store(dec.fps, Ordering::Relaxed);
    }
    tracing::debug!(
        rtt_ms,
        queued,
        lost = format!("{lost_ratio:.3}"),
        kbps = dec.kbps,
        fps = dec.fps,
        windows = slots.len(),
        "码率自适应决策"
    );
}

/// 每秒一条 `StatsReport`（T-027 步骤 5）：fps（本会话编码 AU 差分）、
/// rtt_ms（QUIC 平滑 RTT）、kbps（实际发送字节差分 ×8/1000）。req_id=0。
async fn send_stats(
    ctx: &SessionCtx,
    bytes_sent: &AtomicU64,
    prev_bytes: &mut u64,
    prev_frames: &mut u64,
) {
    let key = ctx_key(ctx);
    let frames: u64 = {
        let map = pipes().lock().unwrap_or_else(lock_poisoned);
        map.iter()
            .filter(|(_, h)| h.owner == key)
            .map(|(_, h)| h.frames.load(Ordering::Relaxed))
            .sum()
    };
    let sent = bytes_sent.load(Ordering::Relaxed);
    let d_frames = frames.saturating_sub(*prev_frames);
    let d_bytes = sent.saturating_sub(*prev_bytes);
    *prev_frames = frames;
    *prev_bytes = sent;

    let report = StatsReport {
        fps: d_frames.min(u64::from(u32::MAX)) as u32,
        rtt_ms: u32::try_from(ctx.conn.rtt().as_millis()).unwrap_or(u32::MAX),
        kbps: (d_bytes.saturating_mul(8) / 1000).min(u64::from(u32::MAX)) as u32,
        decode_ms: 0,
    };
    if let Err(err) = ctx.send(Msg::Stats(report), 0).await {
        tracing::debug!("StatsReport 发送失败（连接可能已断）: {err:#}");
    }
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

    #[test]
    fn retune_updates_target_and_encoder() {
        let mut p = pipe();
        let t0 = Instant::now();
        let f = frame(320, 240, 10, 0);
        let _ = p.process(&f, None, t0).expect("首帧成功（创建编码器）");
        p.retune(2000);
        assert_eq!(p.bitrate(), 2000, "retune 后管线目标码率应更新");
        // 变化后再编码一帧：OpenH264 set_bitrate 应成功不 panic
        let f2 = frame(320, 240, 11, 1);
        let _ = p.process(&f2, None, t0).expect("retune 后编码成功");
    }

    #[test]
    fn retune_pipeline_noop_on_same_value() {
        let mut p = FramePipeline::new(30, 4000, false, false);
        let target = AtomicU32::new(4000);
        assert_eq!(
            retune_pipeline(&mut p, &target, 4000),
            4000,
            "目标未变化时不动作"
        );
        target.store(8000, Ordering::Relaxed);
        assert_eq!(
            retune_pipeline(&mut p, &target, 4000),
            8000,
            "目标变化时返回新值"
        );
        assert_eq!(p.bitrate(), 8000);
        // 目标为 0（尚未分配）视为无效，保持原值
        target.store(0, Ordering::Relaxed);
        assert_eq!(retune_pipeline(&mut p, &target, 8000), 8000);
    }

    // ----- 窗口组 diff（T-027 步骤 1 的纯函数核心） -----

    fn member(hwnd: isize, x: i32, y: i32) -> GroupMember {
        GroupMember {
            hwnd,
            offset_x: x,
            offset_y: y,
            w: 200,
            h: 100,
        }
    }

    fn snapshot(members: &[GroupMember]) -> HashMap<isize, GroupMember> {
        members.iter().map(|m| (m.hwnd, m.clone())).collect()
    }

    #[test]
    fn group_diff_added_keeps_z_order() {
        let prev = snapshot(&[member(0x10, 0, 0)]);
        let next = vec![
            member(0x10, 0, 0),
            member(0x20, 5, 10),
            member(0x30, 50, 60),
        ];
        let d = diff_group(&prev, &next);
        assert_eq!(d.added, vec![member(0x20, 5, 10), member(0x30, 50, 60)]);
        assert!(d.removed.is_empty());
        assert!(d.moved.is_empty());
    }

    #[test]
    fn group_diff_removed_reports_hwnds() {
        let prev = snapshot(&[member(0x10, 0, 0), member(0x20, 5, 10)]);
        let next = vec![member(0x10, 0, 0)];
        let d = diff_group(&prev, &next);
        assert!(d.added.is_empty());
        assert_eq!(d.removed, vec![0x20]);
        assert!(d.moved.is_empty());
    }

    #[test]
    fn group_diff_moved_on_offset_change_only() {
        let prev = snapshot(&[
            member(0x10, 0, 0),
            member(0x20, 5, 10),
            GroupMember {
                hwnd: 0x30,
                offset_x: 0,
                offset_y: 0,
                w: 200,
                h: 100,
            },
        ]);
        // 0x20 偏移变化；0x30 仅尺寸变化、偏移不变（不算 moved，也不算 added）
        let next = vec![
            member(0x10, 0, 0),
            GroupMember {
                hwnd: 0x20,
                offset_x: 5,
                offset_y: 40,
                w: 200,
                h: 100,
            },
            GroupMember {
                hwnd: 0x30,
                offset_x: 0,
                offset_y: 0,
                w: 640,
                h: 480,
            },
        ];
        let d = diff_group(&prev, &next);
        assert_eq!(d.moved.len(), 1);
        assert_eq!(d.moved[0].hwnd, 0x20);
        assert_eq!(d.moved[0].offset_y, 40, "moved 应携带新偏移");
        assert!(d.added.is_empty(), "既有成员仅尺寸变化不算新增");
        assert!(d.removed.is_empty());
    }

    #[test]
    fn group_diff_mixed_add_remove_move() {
        let prev = snapshot(&[member(0x10, 0, 0), member(0x20, 5, 10), member(0x40, 1, 1)]);
        let next = vec![
            member(0x10, 0, 0),
            member(0x30, 7, 8), // 新增
            member(0x40, 2, 1), // 移动
        ]; // 0x20 消失
        let d = diff_group(&prev, &next);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].hwnd, 0x30);
        assert_eq!(d.removed, vec![0x20]);
        assert_eq!(d.moved.len(), 1);
        assert_eq!(d.moved[0].hwnd, 0x40);
    }

    #[test]
    fn group_diff_empty_next_reports_all_removed() {
        let prev = snapshot(&[member(0x10, 0, 0), member(0x20, 5, 10)]);
        let d = diff_group(&prev, &[]);
        assert!(d.added.is_empty());
        assert!(d.moved.is_empty());
        assert_eq!(d.removed.len(), 2);
    }

    // ----- 码率按面积分配（T-027 步骤 3 的纯函数核心） -----

    #[test]
    fn allocate_kbps_splits_by_area() {
        // 1:3 面积 → 1:3 码率（取整）
        let shares = allocate_kbps(8000, &[100, 300]);
        assert_eq!(shares, vec![2000, 6000]);
    }

    #[test]
    fn allocate_kbps_single_window_gets_all() {
        assert_eq!(allocate_kbps(8000, &[1920 * 1080]), vec![8000]);
    }

    #[test]
    fn allocate_kbps_zero_total_area_splits_evenly() {
        assert_eq!(allocate_kbps(8000, &[0, 0]), vec![8000, 8000]);
    }

    #[test]
    fn allocate_kbps_min_share_is_one() {
        // 微小面积仍应分到至少 1 kbps；整数截断 + 下限 1，总和不超过总额
        let shares = allocate_kbps(1000, &[1_000_000, 1]);
        assert_eq!(shares[0], 999); // 1000×10^6/(10^6+1) 截断
        assert_eq!(shares[1], 1); // 0 截断后抬到下限 1
        assert!(shares.iter().all(|&s| s >= 1));
        assert!(shares.iter().sum::<u32>() <= 1000, "不超发");
    }

    // ----- 编解码协商回退（T-027 步骤 4） -----

    #[test]
    fn effective_codec_falls_back_to_420_when_444_unavailable() {
        // 当前 rd-encode 无 4:4:4 创建路径：协商出 444 也必须诚实回退
        assert_eq!(effective_codec(Codec::H264444), Codec::H264420);
        assert_eq!(effective_codec(Codec::H264420), Codec::H264420);
    }

    #[test]
    fn negotiate_matches_caps_both_sides() {
        use rd_encode::caps::negotiate;
        let both = [Codec::H264420, Codec::H264444];
        assert_eq!(negotiate(&both, &both, true), Codec::H264444);
        assert_eq!(negotiate(&both, &both, false), Codec::H264420);
        let only420 = [Codec::H264420];
        assert_eq!(negotiate(&only420, &both, true), Codec::H264420);
    }
}
