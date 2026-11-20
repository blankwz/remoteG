//! 虚拟屏编排(一期 + 二期块 1):会话级引用计数 + 建屏/撤屏 + 换档 + 窗口落屏。
//!
//! 语义(任务卡 T-044 一期步骤 5~6 / T-049 块 1):
//! - 会话进入已鉴权状态后按用户手机规格建一块虚拟显示器(进程内共享,多连接
//!   只建一块;全部会话断开才撤屏);
//! - 建屏/换档失败**不影响远控会话**,只回 `VirtualDisplayState{FAILED}`;
//! - 撤屏在窗口布局还原之后(调用方顺序保证),并等屏真的从拓扑消失;
//! - 打开/适配窗口时把窗口摆到虚拟屏中央(物理屏因此保持可用);
//! - 换档成功后置位全部在途管线 `drop_to_idr`,新尺寸首帧即 IDR;
//! - 块 1:虚拟屏上的根窗口外框构成需求表,窗口开/关/尺寸变化联动集体选档
//!   (见 [`fit_sched`]);手动 `SetDisplayWidth` 是一次性档位指令。
//!
//! 结构:`orch`(引用计数状态机,IO 与状态分离,T-049 M-4 清偿) +
//! `fit_sched`(选档调度线程) + 本文件(生产 IO 适配)。

pub mod fit_sched;
pub mod orch;

#[cfg(test)]
mod tests;

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use rd_proto::v0::envelope::Msg;
use rd_proto::v0::virtual_display_state::Kind;
use rd_proto::v0::{Error as ProtoError, SetDisplayWidth, VirtualDisplayState};
use rd_vdisplay::fit::PhoneSpec;
use rd_vdisplay::{DisplayInfo, ScreenFit};

use crate::config::VirtualDisplayConfig;
use crate::session::SessionCtx;
use orch::{Action, Orchestrator};

/// 协议错误码 60:参数无效(05_api §4)。
const ERR_PARAM: u32 = 60;

/// 全局编排单例(进程内共享一块虚拟屏)。
fn orchestration() -> &'static Orchestrator {
    static ORCH: OnceLock<Orchestrator> = OnceLock::new();
    ORCH.get_or_init(Orchestrator::default)
}

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
/// 建屏由首个接入的会话发起;其余会话经 `ensure_display` 的复用分支拿到
/// 已建好的屏(或等建屏完成后收到同一份 Created 通知——各会话独立 spawn,
/// 串行锁保证只有一个真正建屏)。
pub fn spawn_start(ctx: &Arc<SessionCtx>) {
    if !enabled(&ctx.state.config.virtual_display) {
        // 未启用也回一次 FAILED：控制端不知道被控端配置（agent.toml 在被
        // 控端），静默会让它分不清「功能没开」与「消息丢失」——契约 §9.1
        // 「任何 Start 必有响应」的本意（审查 MEDIUM-3 修正）
        let ctx = Arc::clone(ctx);
        tokio::spawn(async move {
            let _ = notify(
                &ctx,
                Kind::Failed,
                0,
                0,
                "被控端未启用虚拟屏（agent.toml [virtual_display] enabled=false）".into(),
            )
            .await;
        });
        return;
    }
    if ctx.vdisplay_on.swap(true, Ordering::SeqCst) {
        return; // 同一连接只计一次
    }
    // 广播 Handle 注入（审查 M-1）：会话建立必经此路（tokio 上下文内），
    // 幂等覆盖——即使 fit 线程被编码线程抢先初始化（无 tokio 上下文捕获
    // 不到 Handle），这里也会补上。
    fit_sched::set_broadcast_handle(tokio::runtime::Handle::current());
    fit_sched::register_session(crate::window_session::ctx_key(ctx), ctx);
    if !orchestration().acquire() {
        return;
    }
    let fit = fit_of(&ctx.state.config.virtual_display);
    let ctx = Arc::clone(ctx);
    tokio::spawn(async move {
        let action = orchestration().ensure_display(fit).await;
        let result = match action {
            Action::Created { width, height } => {
                notify(&ctx, Kind::Created, width, height, String::new()).await
            }
            Action::Failed(message) => notify(&ctx, Kind::Failed, 0, 0, message).await,
            Action::None => Ok(()),
        };
        if let Err(err) = result {
            tracing::warn!("虚拟屏状态通知失败: {err:#}");
        }
    });
}

/// 会话结束:引用 -1;最后一个会话断开时撤屏(等屏真消失后返回)。
///
/// 调用方须先完成窗口布局还原(撤屏后窗口会随拓扑变化被系统重排)。
pub async fn stop(ctx: &SessionCtx) {
    if !ctx.vdisplay_on.swap(false, Ordering::SeqCst) {
        return;
    }
    fit_sched::unregister_session(crate::window_session::ctx_key(ctx));
    if !orchestration().release() {
        return;
    }
    let Some((_, mut handle)) = orchestration().take_if_last().await else {
        return; // 建屏失败/尚未完成(由 ensure_display 的引用归零分支兜底)
    };
    let joined = tokio::task::spawn_blocking(move || {
        if let Err(err) = handle.close() {
            tracing::warn!("撤屏失败: {err}");
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
    let switched =
        tokio::task::spawn_blocking(move || orchestration().with_slot(|vd, _| vd.set_width(width)))
            .await;
    match switched {
        Ok(Some(Ok(()))) => {
            let info = current_info().await;
            ctx.send(Msg::SetDisplayWidth(SetDisplayWidth { width }), req_id)
                .await?;
            if let Some(info) = &info {
                notify(ctx, Kind::Switched, info.width, info.height, String::new()).await?;
            }
            crate::window_session::force_idr_owned(ctx);
            tracing::info!(width, "虚拟屏换档完成");
            Ok(())
        }
        Ok(Some(Err(err))) => {
            tracing::warn!(width, "虚拟屏换档失败: {err}");
            send_err(ctx, req_id, err.to_string()).await
        }
        Ok(None) => send_err(ctx, req_id, "虚拟屏未启用或不可用").await,
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
    orchestration().with_slot(|vd, _| vd.info())
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

/// 把窗口摆到虚拟屏中央(同步版;fit 调度线程与 async 调用方共用)。
pub(crate) fn center_window_sync(window_id: u64) {
    let area = orchestration().with_slot(|vd, _| {
        let info = vd.info();
        Area {
            x: info.position.0,
            y: info.position.1,
            w: info.width,
            h: info.height,
        }
    });
    let Some(area) = area else {
        return;
    };
    let rect = match rd_window_win::resize::window_rect(window_id as isize) {
        Ok(rect) => rect,
        Err(_) => return,
    };
    let x = area.x + (area.w.saturating_sub(rect.w) / 2) as i32;
    let y = area.y + (area.h.saturating_sub(rect.h) / 2) as i32;
    if rect.x == x && rect.y == y {
        return;
    }
    if let Err(err) = rd_window_win::resize::move_window_to(window_id as isize, x, y) {
        tracing::warn!(
            window = format!("{window_id:#x}"),
            "窗口落虚拟屏失败: {err}"
        );
    }
}

/// 把窗口摆到虚拟屏中央(没建屏 / 窗口无效时静默跳过)。
pub async fn center_window(window_id: u64) {
    let _ = tokio::task::spawn_blocking(move || center_window_sync(window_id)).await;
}

/// 块 1 触发点(转发到 fit 调度线程;见 [`fit_sched`])。
pub fn request_refit(window_id: u64) {
    fit_sched::request_refit(window_id);
}

/// 窗口关闭出表(见 [`fit_sched`])。
pub fn request_window_closed(window_id: u64) {
    fit_sched::request_window_closed(window_id);
}
