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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
use rd_encode::{create_best, EncodedAu, EncoderConfig, PixelFormat, VideoEncoder};
use rd_proto::v0::envelope::Msg;
use rd_proto::v0::window_event::Kind;
use rd_proto::v0::{
    session_state, CloseWindow, Codec, Error, FitMode, ListWindows, OpenWindow, OpenWindowAck,
    QualityPreset, RenderLevel, SessionState, SetQuality, StatsReport, ViewportInfo, WindowEvent,
    WindowInfo, WindowList,
};
use rd_transport::framing::encode_video_header;
use rd_transport::framing::{TILE_HEADER_LEN, VIDEO_HEADER_LEN};
use rd_transport::{DataTx, StreamKind, TileHeader, VideoHeader};
use rd_window_win::events::{spawn_event_watcher, WatcherHandle, WindowEventRaw};
use rd_window_win::resize::{fit_window, restore_placement, save_placement, target_client_size};
use rd_window_win::resize::{FitResult, SavedPlacement};
use rd_window_win::{list_windows, window_group, window_icon_png, GroupMember, WindowAttrs};
use switchboard_core::stages::{FrameSink, MediaAdapt};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio_util::sync::CancellationToken;

use crate::session::{AgentState, SessionCtx};

/// 协议错误码 10：窗口不存在（05_api §4）。
const ERR_NO_WINDOW: u32 = 10;
/// 协议错误码 11：窗口无法捕获（受保护/管线失败；05_api §4）。最小化已改为
/// 自动还原后捕获（用户实测反馈 1），不再以此码拒绝。
const ERR_NOT_CAPTURABLE: u32 = 11;
/// 协议错误码 12：无法调整尺寸（已退化为 SCALE；05_api §4，非致命）。
const ERR_FIT_DEGRADED: u32 = 12;
/// 协议错误码 60：画质档位无效（T-028 F5）。
const ERR_QUALITY: u32 = 60;

/// 手动画质档位的目标码率（kbps；下标 = `QualityPreset`，AUTO 不使用）。
/// 上限仍受会话 `cfg` 约束见 `set_quality`——档位只升到配置允许的范围。
const PRESET_KBPS: [u32; 4] = [0, 20_000, 8_000, 2_000];
/// 手动画质档位的帧率上限（fps；同上）。
const PRESET_FPS: [u32; 4] = [0, 60, 30, 15];
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
/// 最小化还原生效确认的轮询间隔（`ShowWindowAsync` 只投递命令，生效异步）。
const RESTORE_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 还原确认轮询轮数上限（20 × 50 ms = 1 s；期间持有 open 串行锁）。
const RESTORE_POLL_ROUNDS: usize = 20;

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
/// 管线世代号（进程级自增）：注册表条目的身份标识。同窗口快速重开时，旧
/// 管线的收尾任务（[`pipe_cleanup`]，由发送任务在取消后异步触发）可能与
/// "停旧建新"并发——仅按 window_id 移除会把新注册的管线误杀（管线消失
/// 黑屏），收尾必须比对世代号，见 [`unregister_pipe`]。
static PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 窗口账本目录（E1）：记账/销账与 main.rs 的启动恢复必须同一目录——
/// `--config-dir` 覆盖启动时若两条路径各自取目录（此前记账/销账写
/// `default_config_dir`，启动恢复读覆盖目录），账本永远对不上。lib `start`
/// 时登记运行时目录；未登记（单测直调）回退默认目录。
static LEDGER_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 登记窗口账本目录（lib `start` 调用；进程内一次）。
pub(crate) fn set_ledger_dir(dir: &std::path::Path) {
    let _ = LEDGER_DIR.set(dir.to_path_buf());
}

/// 窗口账本目录：运行时目录（已登记）或默认目录（单测兜底）。
fn ledger_dir() -> PathBuf {
    LEDGER_DIR
        .get()
        .cloned()
        .unwrap_or_else(crate::default_config_dir)
}

fn pipes() -> &'static Mutex<HashMap<u64, PipeHandle>> {
    PIPES.get_or_init(Default::default)
}

/// 注册一条窗口管线，返回被顶替的旧条目（调用方负责 shutdown）。
///
/// `HashMap::insert` 的顶替在锁内原子完成：任何注册路径（根窗口 open、
/// 窗口组成员接入）若与既有同窗口条目相撞，注册完成即只剩一条——
/// "同一窗口至多一条存活管线"的注册侧兜底。
/// 置位本连接全部窗口管线的「下一帧 IDR」标志（虚拟屏换档后调用）。
///
/// 换档改变了被捕获窗口的像素尺寸；编码器随后按新尺寸重配，旧引用链
/// 对新尺寸无效，首帧必须是 IDR 才能解码（与陈旧帧丢弃同一消费路径）。
pub(crate) fn force_idr_owned(ctx: &SessionCtx) {
    let key = ctx_key(ctx);
    let map = pipes().lock().unwrap_or_else(lock_poisoned);
    for h in map.values() {
        if h.owner == key {
            h.drop_to_idr.store(true, Ordering::Relaxed);
        }
    }
}

/// 全部管线强制下一帧 IDR（fit 调度线程换档：虚拟屏尺寸变了，全部在途
/// 参考链作废。跨会话广播，不区分 owner）。
pub(crate) fn force_idr_all() {
    let map = pipes().lock().unwrap_or_else(lock_poisoned);
    for h in map.values() {
        h.drop_to_idr.store(true, Ordering::Relaxed);
    }
}

fn register_pipe(handle: PipeHandle) -> Option<PipeHandle> {
    pipes()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .insert(handle.hwnd, handle)
}

/// 按世代号移除窗口管线：注册条目仍是本世代时才移除并返回，否则不动注册表。
///
/// 旧管线的收尾与新管线的注册存在竞态（发送任务取消后的异步收尾 vs
/// open_window 停旧建新的多次 await），世代比对保证收尾只处理自己那条。
fn unregister_pipe(window_id: u64, serial: u64) -> Option<PipeHandle> {
    let mut map = pipes().lock().unwrap_or_else(lock_poisoned);
    match map.get(&window_id) {
        Some(h) if h.serial == serial => map.remove(&window_id),
        _ => None,
    }
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
pub(crate) fn ctx_key(ctx: &SessionCtx) -> usize {
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

    /// 窗口（根）是否以只看不控模式打开（v0.1/D-013 包4）。
    /// 非根/未打开 → false（注入准入由 `is_open` 另行把关）。
    pub fn is_view_only(&self, id: u64) -> bool {
        pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .get(&id)
            .is_some_and(|h| h.view_only)
    }
}

/// 处理一条窗口消息。业务错误回 `Error{code:10/11/12}`（异步）；
/// 返回 Err 仅用于协议级致命错误（本模块当前无此类错误）。
pub fn handle(ctx: &SessionCtx, req_id: u32, msg: WindowMsg) -> anyhow::Result<()> {
    match msg {
        WindowMsg::List(_) => handle_list(ctx, req_id),
        WindowMsg::Open(m) => {
            // 审计（合规留痕）：远端打开窗口的画面即开始被观看——先记后做，
            // 失败（不可捕获）会话里有 Error 上报，审计侧不回滚（痕迹宁多勿缺）
            crate::audit::log(
                "window_open",
                &ctx.peer_name,
                &ctx.peer_fp,
                serde_json::json!({ "window": format!("{:#x}", m.window_id) }),
            );
            handle_open(ctx, req_id, m)
        }
        WindowMsg::Close(m) => {
            crate::audit::log(
                "window_close",
                &ctx.peer_name,
                &ctx.peer_fp,
                serde_json::json!({ "window": format!("{:#x}", m.window_id) }),
            );
            handle_close(m)
        }
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

/// E2 自扰静默窗（privacy::set 登记，桥接线程消费）。
fn event_quiet() -> &'static Mutex<rd_window_win::quiet::QuietWindow> {
    static Q: OnceLock<Mutex<rd_window_win::quiet::QuietWindow>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(rd_window_win::quiet::QuietWindow::default()))
}

/// 单调秒基准（静默窗与节流共用；进程内一致即可）。
pub(crate) fn mono_now() -> f64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

/// E2：登记一次本进程引起的窗口扰动（隐私屏开关、未来虚拟屏换档）。
pub(crate) fn note_self_disturbance() {
    let mut q = event_quiet().lock().unwrap_or_else(lock_poisoned);
    q.note(mono_now());
    tracing::debug!("窗口事件自扰静默 5s");
}

/// E2：是否本进程自造的窗口（隐私屏黑窗）——其事件永不推给控制端。
fn is_self_induced(ev: &WindowEventRaw) -> bool {
    let hwnd = match ev {
        WindowEventRaw::Created(h)
        | WindowEventRaw::Destroyed(h)
        | WindowEventRaw::Moved(h)
        | WindowEventRaw::Title(h)
        | WindowEventRaw::MenuStart(h)
        | WindowEventRaw::MenuEnd(h) => *h,
    };
    is_shield_window(hwnd)
}

/// 类名是否隐私屏黑窗（与 privacy::SHIELD_CLASS 同源；黑窗类名稳定）。
fn is_shield_window(hwnd: isize) -> bool {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let h = windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void);
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(h, &mut buf) };
        if n <= 0 {
            return false;
        }
        HSTRING::from_wide(&buf[..n as usize]) == rd_agent_privacy_class()
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

/// 隐私屏黑窗类名（与 privacy.rs 的 w!("rd-privacy-shield") 同源）。
fn rd_agent_privacy_class() -> &'static str {
    "rd-privacy-shield"
}

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
    // E2 自扰静默：本进程黑窗（rd-privacy-shield）的事件永不外推；
    // 自扰静默期（privacy::set 登记）内的窗口事件暂缓推送（控制端稍后
    // 刷新列表即可，静默期取 max 防风暴无限拖延——snug ConnectionTracker
    // 语义）。
    let bridge_tx = tx.clone();
    let bridged = std::thread::Builder::new()
        .name("rd-winevent-bridge".into())
        .spawn(move || {
            while let Ok(ev) = rx.recv() {
                let now = mono_now();
                let quiet = event_quiet().lock().unwrap_or_else(lock_poisoned);
                if is_self_induced(&ev) || quiet.is_quiet(now) {
                    continue;
                }
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
// Open：校验（最小化自动还原）→ 保存布局 → 捕获 → 数据流 → 管线线程/任务 → ack
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
    // 全局串行化（D-011 收尾）：open 内部多处 await（枚举/编码器创建/流建立），
    // 并发到达的两个 OpenWindow 若各自走完"停旧→建新"，在注册前旧条目尚不
    // 存在、双方 remove 全部落空，产出并存双管线（StrictMode 双挂载实测，
    // 前端收到交织双码流必然解码失败黑屏）。串行后重复打开走"停旧建新"
    // 幂等路径。纵深防御：注册侧另有"同 wid 顶替 + 世代号收尾"兜底（见
    // [`register_pipe`]/[`unregister_pipe`]），任何调用序列下同一窗口至多
    // 一条存活管线。
    static OPEN_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _serial = OPEN_SERIAL.lock().await;

    let wid = open.window_id;
    // 同窗口重复打开：先按关闭语义停掉旧管线（恢复布局，避免覆盖原始 placement）
    if let Some(old) = pipes().lock().unwrap_or_else(lock_poisoned).remove(&wid) {
        tracing::info!(window = format!("{wid:#x}"), "窗口已打开，先关闭旧会话");
        old.shutdown(true);
    }

    // 校验：不存在/不可捕获 → 10；最小化 → 自动还原再捕获（用户实测反馈 1，
    // 行业标准做法 RustDesk 同款；列表页 UI 已提示"将被还原"），不再拒绝。
    let mut attrs = {
        let wins = list_windows().context("枚举窗口失败")?;
        wins.into_iter().find(|a| a.hwnd as u64 == wid)
    };
    // 布局快照先于还原（D-013 包2）：还原后 showCmd 已变为普通态，
    // 会话结束 `restore_placement` 将无法把窗口放回最小化——对最小化窗口
    // GetWindowPlacement 依旧返回正确 rcNormalPosition + showCmd=SW_MINIMIZE。
    let saved =
        match save_placement(attrs.as_ref().map_or(wid as isize, |a| a.hwnd)) {
            Ok(p) => {
                // E1 记账还原：改动前落盘（崩溃重启后 recover 可放回原布局）
                if let Err(err) = rd_window_win::ledger::OperationLedger::open(&ledger_dir())
                    .record(wid as isize, "remote-session", &p)
                {
                    tracing::warn!(window = format!("{wid:#x}"), "窗口账本记账失败: {err:#}");
                }
                Some(p)
            }
            Err(err) => {
                tracing::warn!(window = format!("{wid:#x}"), "保存窗口布局失败: {err:#}");
                None
            }
        };
    // 最小化自动还原（用户实测反馈 1：任何状态必须可控制）。最小化有两条
    // 呈现路径，都必须还原后再捕获：
    // - 在可捕获列表中（如资源管理器，最小化后客户区仍非 0）→ `minimized`
    //   字段为 true；
    // - 不在列表中（多数窗口最小化后客户区塌缩为 0×0，被 rd-window-win 的
    //   尺寸过滤排除）→ 以 `IsIconic` 直接判定（不在列表即无字段可看）。
    let minimized_now = match &attrs {
        Some(a) => a.minimized,
        None => is_iconic_window(wid as isize),
    };
    if minimized_now {
        tracing::info!(
            window = format!("{wid:#x}"),
            "窗口处于最小化，自动还原后捕获"
        );
        restore_minimized(attrs.as_ref().map_or(wid as isize, |a| a.hwnd));
        match after_restore(wid).await {
            AfterRestore::Ready(a) => attrs = Some(a),
            AfterRestore::Gone => {
                return send_error(
                    ctx,
                    req_id,
                    ERR_NO_WINDOW,
                    format!("窗口 {wid:#x} 在还原前已关闭"),
                )
                .await;
            }
            AfterRestore::Pending => {
                tracing::warn!(
                    window = format!("{wid:#x}"),
                    "还原未在限时内生效，仍按计划捕获（还原生效后 WGC 即出帧）"
                );
                // 最小化 rect 是 -32000 哨兵值/0×0；用布局快照的还原矩形兜底，
                // 避免错误的 ack 尺寸与码率面积权重。
                if let Some(a) = attrs.as_mut() {
                    if let Ok(p) = save_placement(a.hwnd) {
                        a.rect = p.normal_position();
                    }
                }
            }
        }
    }
    let Some(attrs) = attrs else {
        // 含：还原超时后窗口始终不在可捕获列表（隐藏/cloaked 等）。
        return send_error(
            ctx,
            req_id,
            ERR_NO_WINDOW,
            format!("窗口 {wid:#x} 不存在或不可捕获"),
        )
        .await;
    };

    let cfg = &ctx.state.config;
    // max_fps = min(请求, 配置)；请求 0 视为未指定，用配置默认
    let mut fps = if open.max_fps == 0 {
        cfg.max_fps
    } else {
        open.max_fps.min(cfg.max_fps)
    }
    .max(1);
    let mut kbps = cfg.start_kbps.max(1);
    // 会话已有手动档位时（T-028 F5），新窗口直接按档位起步（否则首个
    // rate tick 前的 ~200 ms 会以配置默认值全速发送）
    if let Ok(p) = QualityPreset::try_from(ctx.user_quality.load(Ordering::Relaxed) as i32) {
        if p != QualityPreset::Auto {
            let i = p as usize;
            kbps = PRESET_KBPS[i].max(1);
            fps = fps.min(PRESET_FPS[i].max(1)).max(1);
        }
    }
    let refine = cfg.refine_enabled;

    // 编解码协商（T-027 步骤 4）：本机能力 ∩ 控制端能力 ∩ supports_444。
    // proto 的 repeated Codec 在 prost 生成物中为 Vec<i32>，需转回枚举。
    let decode_caps: Vec<Codec> = ctx
        .hello
        .decode_caps
        .iter()
        .filter_map(|&c| Codec::try_from(c).ok())
        .collect();
    // D-011 T-027 遗留收尾：444 不再无条件回退——ack 前以 Yuv444 实际创建
    // 验证，失败才降级 420（resolve_codec，OpenWindowAck 不得谎报解码格式）。
    // 协商在管线搭建前完成：编码线程与 ack 使用同一次落地结果。
    let codec = resolve_codec(
        &attrs,
        effective_codec(negotiate(
            &probe_encode_caps(),
            &decode_caps,
            ctx.hello.supports_444,
        )),
        fps,
        kbps,
    );

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
        codec,
        owner: ctx_key(ctx),
        session_bytes: ensure_services(ctx),
        view_only: open.view_only,
    };
    let started = match start_pipeline(ctx, spec).await {
        Ok(s) => s,
        Err(err) => {
            return send_error(ctx, req_id, ERR_NOT_CAPTURABLE, format!("{err:#}")).await;
        }
    };

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

// ---------------------------------------------------------------------------
// 最小化自动还原（用户实测反馈 1）
// ---------------------------------------------------------------------------

/// [`after_restore`] 的轮询结论。
enum AfterRestore {
    /// 已还原：携带重取后的窗口属性（最小化 rect 是 -32000 哨兵值，必须
    /// 重取，否则 ack 尺寸与码率面积权重全错）。
    Ready(WindowAttrs),
    /// 轮询期间窗口消失（还原前被销毁/关闭）→ `Error{10}`。
    Gone,
    /// 限时内仍未还原（目标窗口线程繁忙/挂起）：按计划捕获，尽力而为，
    /// 还原最终生效后 WGC 即出帧。
    Pending,
}

/// hwnd 是否处于最小化（iconic）。窗口不存在时返回 false（存在性判定另行处理）。
fn is_iconic_window(hwnd: isize) -> bool {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    unsafe { IsIconic(HWND(hwnd as *mut c_void)) }.as_bool()
}

/// 最小化窗口投递还原命令（`ShowWindowAsync(SW_RESTORE)`）。
///
/// 用 Async 变体而非同步 `ShowWindow`（rd-window-win 私有 helper 同款）：
/// 跨线程同步调用会在目标窗口线程挂起时阻塞调用方，open_window 的全局
/// 串行锁会被单个 hung 窗口拖死；Async 只投递命令立即返回，生效与否由
/// [`after_restore`] 轮询确认。
fn restore_minimized(hwnd: isize) {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindowAsync, SW_RESTORE};
    let ok = unsafe { ShowWindowAsync(HWND(hwnd as *mut c_void), SW_RESTORE) };
    if !ok.as_bool() {
        tracing::warn!(
            hwnd = format!("{hwnd:#x}"),
            "ShowWindowAsync(SW_RESTORE) 投递失败"
        );
    }
}

/// 还原生效确认：轮询重枚举直到窗口脱离最小化，或超时/消失。
///
/// 枚举放 blocking 池（与 [`handle_list`] 同理由：hung 窗口可能拖慢单次
/// 枚举，不能卡 async 执行线程）；枚举瞬时失败继续等待（窗口此刻必然
/// 存在，失败多为瞬时资源问题），超时返回 [`AfterRestore::Pending`]。
async fn after_restore(wid: u64) -> AfterRestore {
    for _ in 0..RESTORE_POLL_ROUNDS {
        tokio::time::sleep(RESTORE_POLL_INTERVAL).await;
        let Ok(Ok(wins)) = tokio::task::spawn_blocking(list_windows).await else {
            continue; // 枚举瞬时失败或任务异常，下轮重试
        };
        match wins.into_iter().find(|a| a.hwnd as u64 == wid) {
            None => return AfterRestore::Gone,
            Some(a) if !a.minimized => return AfterRestore::Ready(a),
            Some(_) => {} // 仍在还原中，继续等
        }
    }
    AfterRestore::Pending
}

/// 发送任务的共享记账状态（T-027：码率自适应的在途积压信号 + 会话字节累计）。
struct SendAccounting {
    /// 本管线在途字节（发送任务入队 +N、写完 -N）。
    backlog: Arc<AtomicU64>,
    /// 本会话累计发送字节（StatsReport.kbps 数据源）。
    session_bytes: Arc<AtomicU64>,
    /// 陈旧帧丢弃标志（D-013 包2）：发送任务跳帧后置位，编码循环消费——
    /// 引用链已断，下一帧必须 IDR，否则后续 P 帧全部无法解码。
    drop_to_idr: Arc<AtomicBool>,
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
    /// 落地的编解码器（协商 + 444 创建验证后的结果；OpenWindowAck 同源）。
    codec: Codec,
    /// 打开者连接标识。
    owner: usize,
    /// 所属会话的发送字节累计（StatsReport 用）。
    session_bytes: Arc<AtomicU64>,
    /// 只看不控（v0.1/D-013 包4）：注册表存位，注入路径经 `is_view_only` 拦截。
    view_only: bool,
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
    let serial = PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
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

    // T-027：协商开启 fec-shards 时，视频走 datagram（FEC 分片），不开可靠流；
    // 瓦片/终端仍走可靠流不变。
    let (mut video_out, video_stream_id) = if ctx.media_fec {
        let ch: Arc<dyn switchboard_core::channel::Channel> =
            Arc::new(rd_switch::RdChannel::from_conn(
                Arc::clone(&ctx.conn),
                switchboard_core::types::PathKind::Direct,
            ));
        let fec = switchboard_mod_fec_rs::FecRs::new(Default::default());
        let bus = Arc::new(switchboard_core::telemetry::EventBus::new());
        let (sink, _src) = fec
            .bind(Arc::clone(&ch), ch, bus)
            .await
            .context("绑定 fec-shards 视频路径失败")?;
        (
            VideoOut::Fec {
                sink: tokio::sync::Mutex::new(sink),
                next_fid: Arc::new(AtomicU32::new(0)),
            },
            u32::MAX,
        )
    } else {
        let t = match ctx.conn.open_data(StreamKind::Video).await {
            Ok(t) => t,
            Err(err) => {
                drop(capture);
                restore_on_fail(&spec);
                anyhow::bail!("打开视频流失败: {err:#}");
            }
        };
        let sid = u32::try_from(t.id()).unwrap_or(u32::MAX);
        (VideoOut::Stream(t), sid)
    };
    let mut tile_tx = match ctx.conn.open_data(StreamKind::Tile).await {
        Ok(t) => t,
        Err(err) => {
            drop(capture);
            if let VideoOut::Stream(t) = &mut video_out {
                let _ = t.finish();
            }
            restore_on_fail(&spec);
            anyhow::bail!("打开瓦片流失败: {err:#}");
        }
    };
    let tile_stream_id = u32::try_from(tile_tx.id()).unwrap_or(u32::MAX);

    let cancel = CancellationToken::new();
    let errors = Arc::new(PipeError::default());
    let (au_tx, au_rx) = mpsc::channel(AU_CHANNEL_CAP);
    let (tile_out_tx, tile_rx) = mpsc::channel(TILE_CHANNEL_CAP);
    let backlog = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let target_kbps = Arc::new(AtomicU32::new(spec.kbps));
    let fps_cap = Arc::new(AtomicU32::new(spec.fps));
    // 陈旧帧丢弃→IDR 恢复(D-013 包2):视频发送任务置位、编码循环消费
    let drop_to_idr = Arc::new(AtomicBool::new(false));
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
            pixel_format: pixel_format_of(spec.codec),
            errors: Arc::clone(&errors),
            cancel: cancel.clone(),
            target_kbps: Arc::clone(&target_kbps),
            fps_cap: Arc::clone(&fps_cap),
            frames: Arc::clone(&frames),
            drop_to_idr: Arc::clone(&drop_to_idr),
            window_id: wid,
            is_root: spec.is_root,
        };
        match std::thread::Builder::new()
            .name(format!("rd-enc-{wid:#x}"))
            .spawn(move || encode_loop(run))
        {
            Ok(t) => t,
            Err(err) => {
                drop(capture);
                if let VideoOut::Stream(t) = &mut video_out {
                    let _ = t.finish();
                }
                let _ = tile_tx.finish();
                restore_on_fail(&spec);
                anyhow::bail!("启动编码线程失败: {err}");
            }
        }
    };

    tokio::spawn(video_send_task(
        Arc::clone(ctx),
        cancel.clone(),
        video_out,
        au_rx,
        PipeIdentity {
            window_id: wid,
            serial,
        },
        Arc::clone(&errors),
        SendAccounting {
            backlog: Arc::clone(&backlog),
            session_bytes: Arc::clone(&spec.session_bytes),
            drop_to_idr: Arc::clone(&drop_to_idr),
        },
    ));
    tokio::spawn(tile_send_task(
        Arc::clone(ctx),
        cancel.clone(),
        tile_tx,
        tile_rx,
        PipeIdentity {
            window_id: wid,
            serial,
        },
        errors,
        SendAccounting {
            backlog: Arc::clone(&backlog),
            session_bytes: Arc::clone(&spec.session_bytes),
            drop_to_idr: Arc::clone(&drop_to_idr),
        },
    ));

    // 注册（含同窗口旧条目兜底顶替）：open_window 的"停旧建新"已覆盖根窗口
    // 重复打开，此处兜底拦截一切其余路径（窗口组成员并发接入等）的同窗口
    // 并存管线——注册完成即只剩本条，保证"同一窗口至多一条存活管线"。
    let replaced = register_pipe(PipeHandle {
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
        codec: spec.codec,
        view_only: spec.view_only,
        backlog,
        frames,
        target_kbps,
        fps_cap,
        members: group_snapshot,
        serial,
        drop_to_idr,
    });
    if let Some(old) = replaced {
        tracing::warn!(
            window = format!("{wid:#x}"),
            "注册时发现同窗口旧管线，兜底关闭"
        );
        // 不恢复布局：恢复归 open 开头的停旧路径 / CloseWindow 负责，避免与
        // 本次会话刚保存的布局叠加。
        old.shutdown(false);
    }

    // 虚拟屏（一期）：根窗口摆到虚拟屏中央（物理屏保持可用）。只在建屏后
    // 生效，无虚拟屏时静默跳过。块 1：随后按窗口外框联动集体选档（fit 调度
    // 线程重算需求表，可升可降；手动 SetDisplayWidth 后的自动事件照常重算）。
    if spec.is_root {
        crate::vdisplay::center_window(wid).await;
        crate::vdisplay::request_refit(wid);
    }

    Ok(PipeStarted {
        video_stream_id,
        tile_stream_id,
    })
}

fn restore_saved(hwnd: isize, saved: &Option<SavedPlacement>) {
    if let Some(p) = saved {
        if let Err(err) = restore_placement(hwnd, p) {
            tracing::warn!(hwnd = format!("{hwnd:#x}"), "恢复窗口布局失败: {err:#}");
        } else {
            // E1 记账还原：复原成功销账（崩溃兜底账本）
            rd_window_win::ledger::OperationLedger::open(&ledger_dir()).clear(hwnd);
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
// 画质档位（T-028 F5）
// ---------------------------------------------------------------------------

/// `SetQuality` 处理（请求-应答）：校验档位 → 记入会话 → 立即落到本会话
/// 全部管线（`window_id` 限定单窗口时只落该窗口）。成功原样 echo。
///
/// 生效语义：
/// - 非 AUTO：会话码率自适应停用（[`apply_rate`] 读到档位后按固定预算
///   下发），直到控制端改回 AUTO——固定期间网络劣化不再自动降码率，
///   由帧率上限与 QUIC 拥塞控制兜底；
/// - AUTO：立即恢复自适应——下个 200 ms rate tick 由 [`RateController`]
///   接管（EWMA 基线重新收敛，无需复位）。
pub async fn set_quality(ctx: &SessionCtx, req_id: u32, m: SetQuality) -> anyhow::Result<()> {
    let Ok(preset) = QualityPreset::try_from(m.preset) else {
        return send_error(
            ctx,
            req_id,
            ERR_QUALITY,
            format!("画质档位无效: {}", m.preset),
        )
        .await;
    };
    let key = ctx_key(ctx);
    // window_id 限定：窗口须已由本会话打开（防跨会话误伤其他连接的管线）
    if m.window_id != 0 {
        let owned = pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .get(&m.window_id)
            .map(|h| h.owner == key)
            .unwrap_or(false);
        if !owned {
            return send_error(
                ctx,
                req_id,
                ERR_NO_WINDOW,
                format!("窗口 {:#x} 不在当前会话中", m.window_id),
            )
            .await;
        }
    }
    ctx.user_quality.store(m.preset as u32, Ordering::Relaxed);
    apply_quality(ctx, preset, m.window_id);
    tracing::info!(preset = ?preset, window = format!("{:#x}", m.window_id), "画质档位已生效");
    ctx.send(
        Msg::SetQuality(SetQuality {
            window_id: m.window_id,
            preset: m.preset,
        }),
        req_id,
    )
    .await
}

/// 把档位立即写入管线槽位（不等下个 rate tick，控制端切档即时可感）。
///
/// - 非 AUTO：目标码率 = 档位值（多窗口按面积分摊，总预算 = 档位值），
///   帧率上限 = min(档位, 配置 max_fps)；
/// - AUTO：不动槽位——下个 rate tick 由 [`RateController`] 全量覆盖。
fn apply_quality(ctx: &SessionCtx, preset: QualityPreset, window_id: u64) {
    if preset == QualityPreset::Auto {
        return;
    }
    let key = ctx_key(ctx);
    let idx = preset as usize;
    let fps = PRESET_FPS[idx].min(ctx.state.config.max_fps.max(1)).max(1);
    let mut slots: Vec<(u64, Arc<AtomicU32>, Arc<AtomicU32>)> = pipes()
        .lock()
        .unwrap_or_else(lock_poisoned)
        .iter()
        .filter(|(_, h)| h.owner == key && (window_id == 0 || h.hwnd == window_id))
        .map(|(id, h)| (*id, Arc::clone(&h.target_kbps), Arc::clone(&h.fps_cap)))
        .collect();
    if slots.is_empty() {
        return;
    }
    // 多窗口按面积分摊档位总预算（与 apply_rate 同一策略）
    let areas: Vec<u64> = {
        let map = pipes().lock().unwrap_or_else(lock_poisoned);
        slots
            .iter()
            .map(|(id, _, _)| map.get(id).map(|h| h.area).unwrap_or(1))
            .collect()
    };
    let shares = allocate_kbps(PRESET_KBPS[idx], &areas);
    for ((_, target, cap), kbps) in slots.drain(..).zip(shares) {
        target.store(kbps, Ordering::Relaxed);
        cap.store(fps, Ordering::Relaxed);
    }
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
            // 尺寸变了就重新摆到虚拟屏中央（换档后窗口仍居中），并联动选档
            crate::vdisplay::center_window(window_id).await;
            crate::vdisplay::request_refit(window_id);
        }
        Ok(FitResult::Partial { actual_w, actual_h }) => {
            tracing::warn!(
                window = format!("{window_id:#x}"),
                target = format!("{target_w}x{target_h}"),
                actual = format!("{actual_w}x{actual_h}"),
                "窗口尺寸调整受程序限制，退化本地缩放"
            );
            crate::vdisplay::center_window(window_id).await;
            crate::vdisplay::request_refit(window_id);
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
    /// 落地的编解码器（组成员管线沿用根窗口的协商结果，保证同一窗口组
    /// 的解码格式一致）。
    codec: Codec,
    /// 只看不控（v0.1/D-013 包4）：注入路径经 `WindowSessions::is_view_only` 拦截。
    view_only: bool,
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
    /// 管线世代号（[`PIPE_SEQ`]）：收尾按它确认"注册表里还是不是自己这条"，
    /// 防止旧管线收尾误杀同窗口重开的新管线。
    serial: u64,
    /// 强制下一帧 IDR（虚拟屏换档）：尺寸变了，旧引用链全部作废，首帧必须
    /// 关键帧。与捕获层的 `drop_to_idr` 同一标志（编码循环消费）。
    drop_to_idr: Arc<AtomicBool>,
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
    /// 编码输入色度采样（协商编解码器落地，420/444）。
    pixel_format: PixelFormat,
    errors: Arc<PipeError>,
    cancel: CancellationToken,
    /// 会话分配的目标码率（kbps）；变化时经 `set_bitrate` 落到编码器。
    target_kbps: Arc<AtomicU32>,
    /// 会话建议帧率上限（低于捕获帧率时丢帧节流）。
    fps_cap: Arc<AtomicU32>,
    /// 累计产出 AU 帧数（StatsReport 用）。
    frames: Arc<AtomicU64>,
    /// 陈旧帧丢弃标志：发送任务跳帧后置位，编码循环消费（下帧 force IDR）。
    drop_to_idr: Arc<AtomicBool>,
    /// 窗口 id 与是否根窗口（块 1：窗口尺寸变化联动虚拟屏选档，仅根窗口
    /// 参与需求表；组成员窗口跟随主窗口布局，不占独立档位语义）。
    window_id: u64,
    is_root: bool,
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
        pixel_format,
        errors,
        drop_to_idr,
        cancel,
        target_kbps,
        fps_cap,
        frames,
        window_id,
        is_root,
    } = run;
    let mut pipe = FramePipeline::new(fps, kbps, refine, true, pixel_format);
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
                // 发送任务丢过陈旧帧 → 引用链已断，本帧必须 IDR（D-013 包2）
                if drop_to_idr.swap(false, Ordering::Relaxed) {
                    pipe.force_key = true;
                }
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
                        // 块 1（T-049）：窗口尺寸变化联动虚拟屏选档。编码线程
                        // 无 tokio 上下文，走 fit 调度线程（std mpsc 入队）。
                        if step.resized && is_root {
                            crate::vdisplay::request_refit(window_id);
                        }
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
    /// 本帧相对上一帧尺寸变化（块 1：虚拟屏选档联动信号）。
    resized: bool,
}

/// 捕获帧 → H.264 AU + 无损瓦片的可测核心（不含捕获/传输 IO）。
///
/// 尺寸变化检测与 spike.rs 同款（`CapturedFrame.resized` 已于 rd-capture-win 修复并真机验证；
/// 以帧尺寸差量为准）；变化时 `reconfigure` + 重建 [`TileRefiner`]，下一帧
/// 强制 IDR。`refine = false` 时跳过全部瓦片路径。
struct FramePipeline {
    fps: u32,
    kbps: u32,
    refine: bool,
    prefer_hw: bool,
    /// 编码输入色度采样（协商编解码器落地；444 创建失败已在 ack 前回退）。
    pixel_format: PixelFormat,
    encoder: Option<Box<dyn VideoEncoder>>,
    refiner: Option<TileRefiner>,
    last_size: Option<(u32, u32)>,
    force_key: bool,
    /// 编码器是否尚需持续喂帧才能产出输出（首包 / reconfigure 后）。
    ///
    /// 带缓冲延迟的硬件编码器（h264_mf 等 MFT 类）首次 `encode` 可能返回
    /// `Ok(None)`，需后续帧喂入才吐出首包；静态帧 gating 若在此时跳过，
    /// 关键帧将永不产出（控制器收不到首帧）。故 gating 仅在编码器输出已
    /// 流动（最近一次 `encode` 返回 `Some`）后启用。
    need_feed: bool,
}

impl FramePipeline {
    fn new(fps: u32, kbps: u32, refine: bool, prefer_hw: bool, pixel_format: PixelFormat) -> Self {
        Self {
            fps,
            kbps,
            refine,
            prefer_hw,
            pixel_format,
            encoder: None,
            refiner: None,
            last_size: None,
            force_key: true, // 首帧 IDR
            need_feed: true, // 首包前禁止 gating
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
                        pixel_format: self.pixel_format,
                    };
                    let enc = create_best(&cfg).context("创建视频编码器失败")?;
                    self.encoder = Some(enc);
                }
            }
            self.force_key = true;
            self.last_size = Some((raw.width, raw.height));
            self.need_feed = true; // reconfigure 后编码器可能重新缓冲
            self.refiner = self
                .refine
                .then(|| TileRefiner::new(raw.width, raw.height, TILE_STILL_MS));
            tracing::info!(
                width = raw.width,
                height = raw.height,
                "窗口尺寸变化：编码器已 reconfigure（下一帧 IDR），瓦片精修器已重建"
            );
        }

        // 静止判定：本帧是否有内容变化。refiner 存在时以其块网格为准
        // （`on_frame` 无论 WGC 是否给 dirty hint 都算出精确变更块数）；
        // refiner 关闭时退回 WGC 脏矩形判定（空 = 纯静止）。
        let mut invalidates: Vec<TileOut> = Vec::new();
        let changed = match self.refiner.as_mut() {
            Some(r) => {
                invalidates = r.on_frame(raw, dirty, now);
                r.changed() > 0
            }
            None => !dirty.is_some_and(|d| d.is_empty()),
        };
        // 静止帧跳过视频编码：窗口无内容变化（且编码器输出已流动）时不再跑
        // 色彩转换 + 编码 + 发整帧（静止区已由无损瓦片覆盖，控制器保持上一帧）；
        // 置 `force_key` 使恢复变化首帧为干净 IDR——长时间静止后不依赖编码器
        // 对旧参考帧的依赖，画面无缝跳回。首帧 / 尺寸重建 / 编码器未出首包时恒编码
        // （后者保证 h264_mf 类带缓冲编码器能被持续喂帧，直到吐出首包关键帧）。
        if !changed && !size_changed && !self.need_feed {
            self.force_key = true;
            return Ok(FrameStep {
                au: None,
                invalidates,
                resized: false,
            });
        }
        let enc = self
            .encoder
            .as_mut()
            .expect("encoder created on first frame");
        let au = enc.encode(raw, std::mem::take(&mut self.force_key))?;
        // 输出已流动（返回 Some）则 gating 生效；仍为 None（编码器缓冲中）则
        // 继续喂帧，禁止 gating。
        self.need_feed = au.is_none();
        Ok(FrameStep {
            au,
            invalidates,
            resized: size_changed,
        })
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

/// 管线身份：注册表窗口 id + 世代号（收尾按世代号防误杀，见 [`unregister_pipe`]）。
struct PipeIdentity {
    window_id: u64,
    serial: u64,
}

/// 视频出口（T-027）：可靠流（旧路径）或 fec-shards datagram（协商开启时）
enum VideoOut {
    Stream(DataTx),
    Fec {
        sink: tokio::sync::Mutex<Box<dyn FrameSink>>,
        next_fid: Arc<AtomicU32>,
    },
}

async fn video_send_task(
    ctx: Arc<SessionCtx>,
    cancel: CancellationToken,
    mut video: VideoOut,
    mut rx: mpsc::Receiver<(u32, EncodedAu)>,
    id: PipeIdentity,
    errors: Arc<PipeError>,
    acct: SendAccounting,
) {
    let PipeIdentity { window_id, serial } = id;
    let SendAccounting {
        backlog,
        session_bytes,
        drop_to_idr,
    } = acct;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                None => break, // 编码线程退出（通道关闭）
                Some((seq, au)) => {
                    // 陈旧帧丢弃（D-013 包2）：通道里已积压 ≥2 帧时，发送侧
                    // 带宽已低于编码速率，FIFO 全发只会让接收端延迟单调增长
                    // （弱网下雪崩到秒级）。跳到最新一帧；丢帧后引用链断裂，
                    // 置位 drop_to_idr 让编码循环下帧出 IDR 恢复。
                    let mut current = (seq, au);
                    let mut dropped = 0u32;
                    while !current.1.keyframe {
                        match rx.try_recv() {
                            Ok((s2, au2)) => {
                                dropped += 1;
                                current = (s2, au2);
                            }
                            Err(_) => break,
                        }
                    }
                    if dropped > 0 {
                        drop_to_idr.store(true, Ordering::Relaxed);
                        tracing::debug!(
                            window = format!("{window_id:#x}"),
                            dropped,
                            seq = current.0,
                            "发送积压：丢弃陈旧帧，下帧 IDR"
                        );
                    }
                    let (seq, au) = current;
                    let header = VideoHeader {
                        window_id,
                        seq,
                        pts_us: au.pts_us,
                        keyframe: au.keyframe,
                    };
                    // 在途字节记账（码率自适应的积压信号）：写完前计入，
                    // 完成后扣除（成功与否都扣，失败时管线即将关闭）。
                    let mut n = (au.data.len() + VIDEO_HEADER_LEN) as u64;
                    backlog.fetch_add(n, Ordering::Relaxed);
                    let sent = match &mut video {
                        VideoOut::Stream(tx) => tx
                            .send_video(&header, &au.data)
                            .await
                            .map(|_| ())
                            .map_err(|e| anyhow::anyhow!("{e:#}")),
                        VideoOut::Fec { sink, next_fid } => {
                            // T-027：fec 无接收端 IDR 请求（v0.2），每 120 帧（≈2s）
                            // 兜底一个关键帧，限制画面卡死的最大时长
                            let fid = next_fid.fetch_add(1, Ordering::Relaxed);
                            if fid % 120 == 0 {
                                drop_to_idr.store(true, Ordering::Relaxed);
                            }
                            let mut payload =
                                Vec::with_capacity(VIDEO_HEADER_LEN + au.data.len());
                            payload
                                .extend_from_slice(&encode_video_header(&header, au.data.len()));
                            payload.extend_from_slice(&au.data);
                            n = payload.len() as u64;
                            sink.lock()
                                .await
                                .submit(switchboard_core::stages::EncodedFrame {
                                    frame_id: fid,
                                    data: switchboard_core::Bytes::from(payload),
                                })
                                .await
                                .inspect(|qd| {
                                    tracing::debug!(
                                        window = format!("{window_id:#x}"),
                                        fid,
                                        bytes = n,
                                        "fec 帧已提交(切块发送)"
                                    );
                                    let _ = qd;
                                })
                                .map(|_| ())
                                .map_err(|e| anyhow::anyhow!("fec submit: {e}"))
                        }
                    };
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
    pipe_cleanup(&ctx, window_id, serial, errors).await;
}

async fn tile_send_task(
    ctx: Arc<SessionCtx>,
    cancel: CancellationToken,
    mut tx: DataTx,
    mut rx: mpsc::Receiver<(u32, TileOut)>,
    id: PipeIdentity,
    errors: Arc<PipeError>,
    acct: SendAccounting,
) {
    let PipeIdentity { window_id, serial } = id;
    let SendAccounting {
        backlog,
        session_bytes,
        drop_to_idr: _, // 瓦片无引用链，不参与陈旧帧丢弃
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
    pipe_cleanup(&ctx, window_id, serial, errors).await;
}

/// 发送任务收尾：按世代号从注册表移除该窗口（拿到句柄者负责统一
/// shutdown/恢复布局），管线自亡时把故障转为 `Error{code:11}` 回给控制端。
/// 两个发送任务并发收尾，世代比对保证 shutdown 与错误上报各只执行一次，
/// 且不会误杀同窗口快速重开后注册的新管线（误杀即黑屏，见
/// [`unregister_pipe`]）。
async fn pipe_cleanup(ctx: &SessionCtx, window_id: u64, serial: u64, errors: Arc<PipeError>) {
    let handle = unregister_pipe(window_id, serial);
    if let Some(handle) = handle {
        handle.shutdown(true);
        // 块 1：窗口退出虚拟屏 → 出需求表并重算（最后一个窗口 → 回落基准档）
        crate::vdisplay::request_window_closed(window_id);
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
        // 锁屏检测状态(v0.1/D-013 包4):变化沿才发 SessionState
        let mut last_locked: Option<bool> = None;
        // 本机优先状态(D-013 包5):同上,变化沿才发
        let mut last_busy: Option<bool> = None;
        let mut next_lock_check = Instant::now();

        loop {
            let wake = next_rate
                .min(next_group)
                .min(next_stats)
                .min(next_lock_check);
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
            if now >= next_lock_check {
                next_lock_check = now + Duration::from_secs(1);
                let locked = workstation_locked();
                if last_locked != Some(locked) {
                    last_locked = Some(locked);
                    let kind = if locked {
                        session_state::Kind::Locked
                    } else {
                        session_state::Kind::Unlocked
                    } as i32;
                    if let Err(err) = ctx
                        .send(
                            Msg::SessionState(SessionState {
                                kind,
                                reason: String::new(),
                            }),
                            0,
                        )
                        .await
                    {
                        tracing::debug!("SessionState(锁屏)发送失败: {err:#}");
                    }
                }
                // 本机优先状态沿(D-013 包5):被控机有人在用 → 控制端显示禁止
                // 光标;恢复空闲 → 解除。1s 轮询足够(注入挂起本身是硬保证,
                // 此通知只是 UI 反馈)。
                let busy = crate::host_activity::busy();
                if last_busy != Some(busy) {
                    last_busy = Some(busy);
                    let kind = if busy {
                        session_state::Kind::HostBusy
                    } else {
                        session_state::Kind::HostIdle
                    } as i32;
                    if let Err(err) = ctx
                        .send(
                            Msg::SessionState(SessionState {
                                kind,
                                reason: String::new(),
                            }),
                            0,
                        )
                        .await
                    {
                        tracing::debug!("SessionState(本机占用)发送失败: {err:#}");
                    }
                }
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
            // 根管线本身一并收场（D-013 包2：此前只清子窗口，根管线滞留
            // 注册表 → 控制端冻结帧 + 孤儿管线直到断连）。DESTROYED 事件
            // 已走推送路径，这里只做资源收场，不再额外通知。
            if let Some(h) = pipes().lock().unwrap_or_else(lock_poisoned).remove(&root) {
                tracing::info!(root = format!("{root:#x}"), "根窗口已销毁，收场根管线");
                h.shutdown(false);
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
            // T-027 遗留收尾：moved 成员携带新几何，子管线存在时同步刷新
            // 面积权重（码率按面积比例分配；此前沿用打开时尺寸，子窗口
            // 变大后权重偏小）。仅尺寸变化（偏移不变）不进 moved，维持
            // diff_group 既有语义——编码器重配置由管线内帧尺寸差量处理。
            let area = u64::from(m.w).saturating_mul(u64::from(m.h));
            if let Some(h) = pipes()
                .lock()
                .unwrap_or_else(lock_poisoned)
                .get_mut(&(m.hwnd as u64))
            {
                h.area = area;
            }
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
    // 子管线沿用根窗口的协商结果与只读位（同一窗口组解码格式/注入策略必须
    // 一致）；根已退出（竞态）时按 420 降级，管线随会话清理，无额外代价。
    let (codec, view_only) = {
        pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .get(&root_id)
            .map(|h| (h.codec, h.view_only))
            .unwrap_or((Codec::H264420, false))
    };
    let spec = PipeSpec {
        hwnd: m.hwnd,
        fps: cfg.max_fps.max(1),
        kbps: cfg.start_kbps.max(1),
        refine: cfg.refine_enabled && !is_menu,
        fit: FitMode::Scale as i32,
        saved: None,
        is_root: false,
        area: u64::from(m.w).saturating_mul(u64::from(m.h)),
        codec,
        owner: ctx_key(ctx),
        session_bytes: ensure_services(ctx),
        view_only,
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

/// 协商结果透传（T-027 步骤 4；D-011 收尾：rd-encode 444 创建接口已就绪）。
///
/// 4:4:4 的实际可用性不再在此无条件回退：`resolve_codec` 在 ack 前以
/// Yuv444 配置实际创建一次编码器验证，失败才回退 `H264_420` 并 warn。
/// 本函数保留为纯透传 + 协商日志（单测锚定"协商即生效"）。
fn effective_codec(negotiated: Codec) -> Codec {
    tracing::info!(?negotiated, "编解码协商结果生效");
    negotiated
}

/// 把协商结果落成实际可用的编解码器（D-011 T-027 遗留收尾）。
///
/// 协商出 `H264_444` 时以 Yuv444 配置实际创建一次编码器验证
/// （`create_best(444)` 只探测 444 硬编且不静默回退）；失败则诚实回退
/// `H264_420` 并 warn——`OpenWindowAck.codec` 告知控制端以何种格式解码，
/// 谎报 444 会导致 WebCodecs 以错误配置建解码器而失败。探测出的编码器
/// 用后即弃（正式编码器由管线在首帧按同一 pixel_format 惰性创建）。
fn resolve_codec(attrs: &WindowAttrs, negotiated: Codec, fps: u32, kbps: u32) -> Codec {
    if negotiated != Codec::H264444 {
        return negotiated;
    }
    let cfg = EncoderConfig {
        width: attrs.rect.w.max(2),
        height: attrs.rect.h.max(2),
        fps: fps.max(1),
        bitrate_kbps: kbps.max(1),
        prefer_hw: true,
        pixel_format: PixelFormat::Yuv444,
    };
    match create_best(&cfg) {
        Ok(probe) => {
            let info = probe.info();
            tracing::info!(name = info.name, "4:4:4 编码器创建验证通过");
            drop(probe);
            Codec::H264444
        }
        Err(err) => {
            tracing::warn!("4:4:4 编码器创建失败，回退 H264_420: {err:#}");
            Codec::H264420
        }
    }
}

/// 协议编解码器 → 编码输入色度采样。
fn pixel_format_of(codec: Codec) -> PixelFormat {
    match codec {
        Codec::H264444 => PixelFormat::Yuv444,
        _ => PixelFormat::Yuv420,
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

    // 手动画质档位优先（T-028 F5）：非 AUTO 时停用自适应，按档位预算全量
    // 下发（编码器 set_bitrate / fps_cap 节流兜底网络劣化）；采样与决策
    // 全部跳过，改回 AUTO 后由下个 tick 无缝接管。
    let user = ctx.user_quality.load(Ordering::Relaxed);
    if let Ok(preset) = QualityPreset::try_from(user as i32) {
        if preset != QualityPreset::Auto {
            let idx = preset as usize;
            let fps = PRESET_FPS[idx].min(ctx.state.config.max_fps.max(1)).max(1);
            let areas: Vec<u64> = slots.iter().map(|s| s.area).collect();
            let shares = allocate_kbps(PRESET_KBPS[idx], &areas);
            for (s, kbps) in slots.iter().zip(shares) {
                s.target.store(kbps, Ordering::Relaxed);
                s.fps_cap.store(fps, Ordering::Relaxed);
            }
            tracing::debug!(preset = ?preset, kbps = PRESET_KBPS[idx], fps, windows = slots.len(), "手动档位码率下发");
            return;
        }
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

/// 输入桌面名 == Winlogon → 工作站已锁定（安全桌面）。检测纯本地 Win32，
/// 失败保守返回 false（宁可少提示也不误报）。
#[cfg(windows)]
fn workstation_locked() -> bool {
    use windows::Win32::Foundation::{GENERIC_ACCESS_RIGHTS, GENERIC_ALL};
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
        DESKTOP_CONTROL_FLAGS, UOI_NAME,
    };

    unsafe {
        let Ok(desktop) = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ACCESS_RIGHTS(GENERIC_ALL.0).0),
        ) else {
            return true; // 输入桌面打不开：通常意味着正在切换/锁定
        };
        let mut buf = [0u16; 64];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(desktop.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            u32::try_from(buf.len() * 2).unwrap_or(u32::MAX),
            Some(&mut needed),
        );
        let _ = CloseDesktop(desktop);
        if ok.is_err() {
            return false;
        }
        let len = (needed as usize / 2).min(buf.len());
        String::from_utf16_lossy(&buf[..len])
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("Winlogon")
    }
}

#[cfg(not(windows))]
fn workstation_locked() -> bool {
    false
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
        FramePipeline::new(30, 4000, true, false, PixelFormat::Yuv420)
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
    fn static_frame_skips_video_encode_refine_on() {
        let mut p = pipe(); // refine=true
        let t0 = Instant::now();
        let f = frame(320, 240, 10, 0);
        assert!(
            p.process(&f, None, t0).expect("首帧").au.is_some(),
            "首帧必须编码（IDR）"
        );
        // 相同内容静止帧：refine 全扫描变更块=0 → 跳过视频编码
        let step = p.process(&f, None, t0).expect("静止帧处理成功");
        assert!(step.au.is_none(), "静止帧不应产出视频 AU");
        // 恢复变化：内容变 → 重新编码，且静止期 force_key 未消费仍是 IDR
        let f2 = frame(320, 240, 50, 1);
        let step = p.process(&f2, None, t0).expect("变化帧处理成功");
        let au = step.au.expect("变化帧应产出 AU");
        assert!(au.keyframe, "静止后恢复变化首帧应为 IDR");
    }

    #[test]
    fn static_frame_skips_video_encode_refine_off() {
        let mut p = FramePipeline::new(30, 4000, false, false, PixelFormat::Yuv420);
        let t0 = Instant::now();
        let f = frame(320, 240, 10, 0);
        assert!(p.process(&f, None, t0).expect("首帧").au.is_some());
        // refine 关闭：退回 WGC 脏矩形判定，空脏区 = 纯静止 → 跳过编码
        let empty: Vec<DirtyRect> = Vec::new();
        let step = p
            .process(&f, Some(&empty), t0)
            .expect("空脏区静止帧处理成功");
        assert!(step.au.is_none(), "refine 关闭 + 空脏区 → 跳过视频编码");
        // 无 dirty hint（全扫描兜底）→ 无法判定静止，保持原编码行为
        let step = p.process(&f, None, t0).expect("无 hint 帧处理成功");
        assert!(step.au.is_some(), "refine 关闭 + 无 hint → 保持编码");
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
        let mut p = FramePipeline::new(30, 4000, false, false, PixelFormat::Yuv420);
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
        let mut p = FramePipeline::new(30, 4000, false, false, PixelFormat::Yuv420);
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
    fn effective_codec_passes_negotiated_result_through() {
        // rd-encode 444 创建接口就绪后协商结果真实透传；实际可用性由
        // resolve_codec 在 ack 前以 create_best(Yuv444) 验证（失败回退
        // 420 并 warn），此处锚定"协商即生效"。
        assert_eq!(effective_codec(Codec::H264444), Codec::H264444);
        assert_eq!(effective_codec(Codec::H264420), Codec::H264420);
    }

    #[test]
    fn pixel_format_of_maps_codec() {
        assert_eq!(pixel_format_of(Codec::H264444), PixelFormat::Yuv444);
        assert_eq!(pixel_format_of(Codec::H264420), PixelFormat::Yuv420);
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

    // ----- 管线注册表去重（重复 open / 旧管线收尾竞态的状态机核心） -----

    /// 测试用空管线句柄（不持有捕获/线程；shutdown 只取消令牌，无 Win32 副作用）。
    fn pipe_handle(hwnd: u64, serial: u64) -> PipeHandle {
        PipeHandle {
            cancel: CancellationToken::new(),
            capture: None,
            enc_thread: None,
            saved: None,
            hwnd,
            fit: 0,
            deb: Arc::new(ViewportDeb::default()),
            owner: 0,
            is_root: true,
            area: 1,
            codec: Codec::H264420,
            view_only: false,
            backlog: Arc::new(AtomicU64::new(0)),
            frames: Arc::new(AtomicU64::new(0)),
            target_kbps: Arc::new(AtomicU32::new(0)),
            fps_cap: Arc::new(AtomicU32::new(0)),
            members: None,
            serial,
            drop_to_idr: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 清理注册表条目（各测试用独立 hwnd，此处防御性兜底）。
    fn drop_pipe(wid: u64) {
        pipes().lock().unwrap_or_else(lock_poisoned).remove(&wid);
    }

    #[test]
    fn stale_pipe_cleanup_never_kills_newer_generation() {
        let wid = 0x5EED_0001;
        drop_pipe(wid);

        // 第 1 次 open：注册世代 0；第 2 次 open 先停旧（世代 0 出表）再建新
        // 注册世代 1——open_window"停旧建新"的正常序列。
        assert!(register_pipe(pipe_handle(wid, 0)).is_none());
        let old = pipes().lock().unwrap_or_else(lock_poisoned).remove(&wid);
        assert_eq!(old.map(|h| h.serial), Some(0), "open 开头去重应移除旧世代");
        assert!(register_pipe(pipe_handle(wid, 1)).is_none());

        // 旧管线（世代 0）的发送任务收尾此刻才到：必须被世代比对拦下，
        // 否则会 shutdown 新管线（误杀即黑屏）。
        assert!(
            unregister_pipe(wid, 0).is_none(),
            "旧世代收尾不得移除新世代条目"
        );
        assert!(pipes()
            .lock()
            .unwrap_or_else(lock_poisoned)
            .contains_key(&wid));
        // 新世代正常收尾出表。
        assert_eq!(unregister_pipe(wid, 1).map(|h| h.serial), Some(1));
        drop_pipe(wid);
    }

    #[test]
    fn register_replaces_same_window_pipeline() {
        let wid = 0x5EED_0002;
        drop_pipe(wid);

        // 未经 open 开头去重的重复注册（如窗口组成员并发接入）：注册侧兜底
        // 顶替——后注册顶掉先注册，注册完成即只剩一条。
        assert!(register_pipe(pipe_handle(wid, 7)).is_none());
        let replaced = register_pipe(pipe_handle(wid, 8));
        assert_eq!(
            replaced.map(|h| h.serial),
            Some(7),
            "同窗口二次注册应顶掉旧条目"
        );
        assert!(
            unregister_pipe(wid, 7).is_none(),
            "被顶掉的旧世代不得再动注册表"
        );
        assert_eq!(unregister_pipe(wid, 8).map(|h| h.serial), Some(8));
        drop_pipe(wid);
    }

    // ── 画质档位（T-028 F5） ──

    /// 档位表与 proto 枚举对齐：echo/apply 都按下标索引，错位即静默错档。
    #[test]
    fn preset_tables_align_with_proto_enum() {
        for raw in 0..=3i32 {
            let p = QualityPreset::try_from(raw).expect("0..=3 都是合法档位");
            assert_eq!(p as i32, raw, "枚举值即表下标");
        }
        assert!(
            QualityPreset::try_from(4).is_err(),
            "越界档位必须报 Error{{60}}"
        );
        // AUTO 不查表（apply_quality/apply_rate 对 AUTO 直接放行自适应）
        assert_eq!(QualityPreset::try_from(0), Ok(QualityPreset::Auto));
    }

    /// 档位语义单调：高 ≥ 中 ≥ 低（码率与帧率），且帧率不超过常见配置上限。
    #[test]
    fn preset_tables_monotonic() {
        let (hi, mid, low) = (1, 2, 3);
        assert!(PRESET_KBPS[hi] > PRESET_KBPS[mid]);
        assert!(PRESET_KBPS[mid] > PRESET_KBPS[low]);
        assert!(PRESET_FPS[hi] > PRESET_FPS[mid]);
        assert!(PRESET_FPS[mid] > PRESET_FPS[low]);
        // 高档也不越过 RateController 的兜底上限语义（50M），低档可用即流畅
        assert!(PRESET_KBPS[hi] <= 50_000);
        assert!(PRESET_FPS[low] >= 10, "低档至少保留可用流畅度");
    }

    /// echo 语义：SetQuality 原样回传依赖 `preset as i32 → try_from` 往返稳定。
    #[test]
    fn preset_roundtrip() {
        for raw in 0..=3i32 {
            let p = QualityPreset::try_from(raw).unwrap();
            assert_eq!(p as i32, raw);
        }
    }
}
