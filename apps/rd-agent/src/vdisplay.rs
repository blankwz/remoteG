//! 虚拟屏编排(一期):会话级引用计数 + 建屏/撤屏 + 换档 + 窗口落屏。
//!
//! 语义(任务卡 T-044 一期步骤 5~6):
//! - 会话进入已鉴权状态后按用户手机规格建一块虚拟显示器(进程内共享,多连接
//!   只建一块;全部会话断开才撤屏);
//! - 建屏/换档失败**不影响远控会话**,只回 `VirtualDisplayState{FAILED}`;
//! - 撤屏在窗口布局还原之后(调用方顺序保证),并等屏真的从拓扑消失;
//! - 打开/适配窗口时把窗口摆到虚拟屏中央(物理屏因此保持可用);
//! - 换档成功后置位全部在途管线 `drop_to_idr`,新尺寸首帧即 IDR。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rd_proto::v0::envelope::Msg;
use rd_proto::v0::virtual_display_state::Kind;
use rd_proto::v0::{Error as ProtoError, SetDisplayWidth, VirtualDisplayState};
use rd_vdisplay::fit::PhoneSpec;
use rd_vdisplay::{DisplayInfo, ScreenFit, VDisplay, WindowsBackend};

use crate::config::VirtualDisplayConfig;
use crate::session::SessionCtx;

/// 协议错误码 60:参数无效(05_api §4)。
const ERR_PARAM: u32 = 60;

/// 进程内共享的虚拟屏句柄(缺省 = 未建)。
type Shared = Arc<Mutex<Option<VDisplay<WindowsBackend>>>>;

#[derive(Default)]
struct Hub {
    display: Option<Shared>,
}

fn hub() -> &'static tokio::sync::Mutex<Hub> {
    static HUB: OnceLock<tokio::sync::Mutex<Hub>> = OnceLock::new();
    HUB.get_or_init(|| tokio::sync::Mutex::new(Hub::default()))
}

/// 建屏串行化:同一时刻只允许一个会话在建屏,其余复用结果。
fn creating() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 当前持有虚拟屏的会话数(降到 0 才真正撤屏)。
static REFS: AtomicUsize = AtomicUsize::new(0);

/// 虚拟屏在桌面上的区域(屏幕物理像素)。
#[derive(Debug, Clone, Copy)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

fn env_flag() -> Option<bool> {
    match std::env::var("RD_VDISPLAY").ok().as_deref() {
        Some("0") | Some("off") | Some("false") => Some(false),
        Some("1") | Some("on") | Some("true") => Some(true),
        _ => None,
    }
}

fn enabled(cfg: &VirtualDisplayConfig) -> bool {
    env_flag().unwrap_or(cfg.enabled)
}

/// 按配置推出档位(未填手机规格 → 标准机 540×950 基准)。
fn fit_of(cfg: &VirtualDisplayConfig) -> ScreenFit {
    if cfg.phone_width == 0 || cfg.phone_height == 0 {
        return ScreenFit::standard();
    }
    let points = (cfg.points > 0).then_some(cfg.points);
    ScreenFit::for_phone(&PhoneSpec {
        width: cfg.phone_width,
        height: cfg.phone_height,
        points,
    })
}

/// 会话开始:引用 +1 并异步确保虚拟屏存在(建屏结果经 `VirtualDisplayState` 通知)。
///
/// 同步部分只做计数与标志(保证 [`stop`] 无论多早到达都能对称减引用)。
pub fn spawn_start(ctx: &Arc<SessionCtx>) {
    if !enabled(&ctx.state.config.virtual_display) {
        return;
    }
    if ctx.vdisplay_on.swap(true, Ordering::SeqCst) {
        return; // 同一连接只计一次
    }
    REFS.fetch_add(1, Ordering::SeqCst);
    let ctx = Arc::clone(ctx);
    tokio::spawn(async move {
        if let Err(err) = ensure_display(&ctx).await {
            tracing::warn!("确保虚拟屏失败: {err:#}");
        }
    });
}

async fn ensure_display(ctx: &Arc<SessionCtx>) -> anyhow::Result<()> {
    let _serial = creating().lock().await;
    // 建屏排队期间本会话已断开（引用归零）：不必再建，撤屏由 stop 负责
    if REFS.load(Ordering::SeqCst) == 0 {
        return Ok(());
    }
    // 别的会话已经建好 → 直接复用它并回报尺寸
    if let Some(info) = current_info().await {
        tracing::info!(display = %info.device_name, "会话复用已存在的虚拟屏");
        return notify(ctx, Kind::Created, info.width, info.height, String::new()).await;
    }

    let fit = fit_of(&ctx.state.config.virtual_display);
    let cfg = ctx.state.config.virtual_display.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let _ = cfg;
        VDisplay::open(fit)
    })
    .await;

    match opened {
        Ok(Ok(vd)) => {
            let info = vd.info().clone();
            // 建屏期间会话已全部结束(引用归零):立刻撤回,不让空屏残留
            if REFS.load(Ordering::SeqCst) == 0 {
                tracing::info!("建屏完成但已无会话,立即撤屏");
                let _ = tokio::task::spawn_blocking(move || {
                    let mut vd = vd;
                    if let Err(err) = vd.close() {
                        tracing::warn!("撤屏失败: {err}");
                    }
                })
                .await;
                return Ok(());
            }
            let shared: Shared = Arc::new(Mutex::new(Some(vd)));
            hub().lock().await.display = Some(shared);
            tracing::info!(display = %info.device_name, w = info.width, h = info.height, "虚拟屏已建立");
            notify(ctx, Kind::Created, info.width, info.height, String::new()).await
        }
        Ok(Err(err)) => {
            tracing::warn!("建屏失败(会话继续,窗口留在物理屏): {err}");
            notify(ctx, Kind::Failed, 0, 0, err.to_string()).await
        }
        Err(err) => {
            tracing::warn!("建屏任务异常: {err}");
            notify(ctx, Kind::Failed, 0, 0, "建屏任务异常".to_string()).await
        }
    }
}

/// 会话结束:引用 -1;最后一个会话断开时撤屏(等屏真消失后返回)。
///
/// 调用方须先完成窗口布局还原(撤屏后窗口会随拓扑变化被系统重排)。
pub async fn stop(ctx: &SessionCtx) {
    if !ctx.vdisplay_on.swap(false, Ordering::SeqCst) {
        return;
    }
    REFS.fetch_sub(1, Ordering::SeqCst);
    // 撤屏决策与建屏同一把锁，且在锁内**复查**引用：否则「A 归零 → 正在
    // 撤屏」与「B 刚 +1 → 复用 A 的屏」会交错，B 会拿到一块马上消失的屏。
    let _serial = creating().lock().await;
    if REFS.load(Ordering::SeqCst) > 0 {
        return;
    }
    let shared = hub().lock().await.display.take();
    let Some(shared) = shared else {
        return; // 建屏失败/尚未完成(由 ensure_display 的引用归零分支兜底)
    };
    let joined = tokio::task::spawn_blocking(move || {
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut vd) = guard.take() {
            if let Err(err) = vd.close() {
                tracing::warn!("撤屏失败: {err}");
            }
        }
    })
    .await;
    if let Err(err) = joined {
        tracing::warn!("撤屏任务异常: {err}");
    } else {
        tracing::info!("虚拟屏已撤除");
    }
    notify(ctx, Kind::Destroyed, 0, 0, String::new()).await.ok();
}

/// `SetDisplayWidth` 处理(请求-应答):成功 echo + 状态通知 + 全部管线强制 IDR。
pub async fn set_width(ctx: &SessionCtx, req_id: u32, req: SetDisplayWidth) -> anyhow::Result<()> {
    let width = req.width;
    let Some(shared) = hub().lock().await.display.clone() else {
        return send_err(ctx, req_id, "虚拟屏未启用或不可用").await;
    };
    let probe = Arc::clone(&shared);
    let switched = tokio::task::spawn_blocking(move || {
        let mut guard = probe.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(vd) => vd.set_width(width),
            None => Err(rd_vdisplay::Error::Detached),
        }
    })
    .await;
    match switched {
        Ok(Ok(())) => {
            let info = shared_info(&shared);
            ctx.send(Msg::SetDisplayWidth(SetDisplayWidth { width }), req_id)
                .await?;
            if let Some(info) = &info {
                notify(ctx, Kind::Switched, info.width, info.height, String::new()).await?;
            }
            crate::window_session::force_idr_owned(ctx);
            tracing::info!(width, "虚拟屏换档完成");
            Ok(())
        }
        Ok(Err(err)) => {
            tracing::warn!(width, "虚拟屏换档失败: {err}");
            send_err(ctx, req_id, err.to_string()).await
        }
        Err(err) => send_err(ctx, req_id, format!("换档任务异常: {err}")).await,
    }
}

async fn send_err(ctx: &SessionCtx, req_id: u32, message: impl Into<String>) -> anyhow::Result<()> {
    let message = message.into();
    ctx.send(
        Msg::Error(ProtoError {
            code: ERR_PARAM,
            message,
        }),
        req_id,
    )
    .await
}

async fn notify(
    ctx: &SessionCtx,
    kind: Kind,
    width: u32,
    height: u32,
    message: String,
) -> anyhow::Result<()> {
    let state = VirtualDisplayState {
        kind: kind as i32,
        width,
        height,
        message,
    };
    ctx.send(Msg::VdisplayState(state), 0).await
}

/// 当前虚拟屏信息(未建屏 → None)。
pub async fn current_info() -> Option<DisplayInfo> {
    let shared = hub().lock().await.display.clone()?;
    let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|vd| vd.info().clone())
}

fn shared_info(shared: &Shared) -> Option<DisplayInfo> {
    let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|vd| vd.info().clone())
}

/// 虚拟屏当前区域(未建屏 → None)。
pub async fn area() -> Option<Area> {
    let info = current_info().await?;
    Some(Area {
        x: info.position.0,
        y: info.position.1,
        w: info.width,
        h: info.height,
    })
}

/// 把窗口摆到虚拟屏中央(没建屏 / 窗口无效时静默跳过)。
pub async fn center_window(window_id: u64) {
    let Some(area) = area().await else {
        return;
    };
    let hwnd = window_id as isize;
    let rect =
        match tokio::task::spawn_blocking(move || rd_window_win::resize::window_rect(hwnd)).await {
            Ok(Ok(rect)) => rect,
            _ => return,
        };
    let x = area.x + (area.w.saturating_sub(rect.w) / 2) as i32;
    let y = area.y + (area.h.saturating_sub(rect.h) / 2) as i32;
    if rect.x == x && rect.y == y {
        return;
    }
    if let Err(err) =
        tokio::task::spawn_blocking(move || rd_window_win::resize::move_window_to(hwnd, x, y)).await
    {
        tracing::warn!(
            window = format!("{window_id:#x}"),
            "窗口落虚拟屏失败: {err}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_defaults_to_standard_phone() {
        let cfg = VirtualDisplayConfig::default();
        assert_eq!(fit_of(&cfg), ScreenFit::standard());
        // 只填一半规格同样回落标准机
        let half = VirtualDisplayConfig {
            phone_width: 1080,
            ..VirtualDisplayConfig::default()
        };
        assert_eq!(fit_of(&half), ScreenFit::standard());
    }

    #[test]
    fn fit_uses_configured_phone() {
        let cfg = VirtualDisplayConfig {
            phone_width: 1080,
            phone_height: 2400,
            points: 400,
            ..VirtualDisplayConfig::default()
        };
        let fit = fit_of(&cfg);
        assert!((fit.ratio - (2400.0 / 1080.0 - 2.0 * 91.0 / 400.0)).abs() < 1e-9);
        assert!(fit.contains(540) && fit.contains(1080));
    }
}
