//! 块 1：窗口实测选档调度（T-049）。
//!
//! 虚拟屏上的全部 rd 根窗口外框构成「需求表」，窗口开/关/尺寸变化后对整表
//! 重算集体档位（能装下全部窗口的最窄档，[`ScreenFit::width_for_windows`]，
//! 可升可降；表空回落基准档）。手动 `SetDisplayWidth` 是一次性档位指令：
//! 立即生效，后续自动事件照常重算——自动选档是常规状态，手动是即时覆盖。
//!
//! 触发点横跨 tokio 任务与编码线程（std 线程，无 tokio 上下文），统一走
//! 静态 `std::sync::mpsc` 入队 + 单一调度线程消费（串行换档，天然防并发
//! CCD 交错）。调度线程内联 debounce：吸收连续 resize 事件（App 拖拽一次
//! 会产生多个 WM_SIZE），合并为一次重算。

use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::session::SessionCtx;
use crate::window_session;

use super::orchestration;

/// 首个 resize 事件后的静默窗：等窗口尺寸稳定再量、再换档。
const DEBOUNCE: Duration = Duration::from_millis(250);

/// 需求表：hwnd → 窗口**客户区** `(w, h)`（物理像素）——内容真需求口径，
/// 与 fit.rs 选档的客户区容量判定配套（外框口径铺满震荡，见 fit.rs）。
fn demands() -> &'static Mutex<HashMap<u64, (u32, u32)>> {
    static DEMANDS: OnceLock<Mutex<HashMap<u64, (u32, u32)>>> = OnceLock::new();
    DEMANDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 活跃会话注册表：SWITCHED 广播目标（fit 线程触发换档时控制端档位 UI
/// 需要同步）。Weak 引用，会话退出由 [`super`] 注销。
fn sessions() -> &'static Mutex<HashMap<usize, Weak<SessionCtx>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<usize, Weak<SessionCtx>>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_session(key: usize, ctx: &std::sync::Arc<SessionCtx>) {
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, std::sync::Arc::downgrade(ctx));
}

pub fn unregister_session(key: usize) {
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
}

enum FitMsg {
    /// 窗口尺寸可能变了：量外框 → 整表重算。
    Refit(u64),
    /// 窗口已关：出表 → 整表重算（可能回落基准档）。
    Closed(u64),
}

fn fit_tx() -> &'static mpsc::Sender<FitMsg> {
    static TX: OnceLock<mpsc::Sender<FitMsg>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<FitMsg>();
        // 首次调用若恰在 tokio 上下文顺手注入广播 Handle（防御：首次调用
        // 早于任何会话建立）；常规注入点是 spawn_start（审查 M-1 修复——
        // 不用 OnceLock 缓存「可能为 None 的结果」，注入式幂等覆盖）。
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            set_broadcast_handle(h);
        }
        std::thread::Builder::new()
            .name("vdisplay-fit".into())
            .spawn(move || fit_loop(rx))
            .expect("spawn fit 调度线程失败");
        tx
    })
}

/// 广播用 tokio Handle：fit 线程自身无 tokio 上下文，由 tokio 侧调用点注入
/// （[`set_broadcast_handle`]，spawn_start 每次会话建立时幂等覆盖）。
fn broadcast_handle() -> &'static Mutex<Option<tokio::runtime::Handle>> {
    static H: OnceLock<Mutex<Option<tokio::runtime::Handle>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(None))
}

/// tokio 侧注入广播 Handle（spawn_start 每次会话建立时调用——会话建立
/// 必经此路，保证编码线程抢先初始化 fit 线程时 Handle 仍会被补上）。
pub fn set_broadcast_handle(handle: tokio::runtime::Handle) {
    *broadcast_handle().lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
}

/// 手动换档保持（T-049 二期）：`SetDisplayWidth` 后记下所选档，作为选档
/// **下限**——竞态窗口里的旧尺寸 refit（换档前的窗口变化滞后处理）不再
/// 覆盖手动档（t15 真机复现：手动 1080 被旧 refit 拉回 550）；需求真实
/// 变大仍跟随升档，需求表清空（全部关窗）则失效回落基准。
static MANUAL_HOLD: OnceLock<Mutex<Option<u32>>> = OnceLock::new();

fn manual_hold() -> &'static Mutex<Option<u32>> {
    MANUAL_HOLD.get_or_init(|| Mutex::new(None))
}

/// 手动换档成功后登记档位下限（`set_width` 调用）。
pub fn set_manual_hold(width: u32) {
    *manual_hold().lock().unwrap_or_else(|e| e.into_inner()) = Some(width);
}

/// 手动档下的目标档裁决（纯函数，单测锚定语义）：
/// - 无手动档 → 自然需求；
/// - 有手动档且表非空 → `max(自然需求, 手动档)`——需求变大跟随升档，
///   需求变小（竞态旧值/关小窗）保持手动档；
/// - 表空（全部关窗）→ 失效回落自然需求（基准档），H-1 关窗回落语义不受
///   手动档影响。
fn resolve_target(natural: u32, hold: Option<u32>, has_windows: bool) -> u32 {
    match hold {
        Some(held) if has_windows && natural < held => held,
        _ => natural,
    }
}

/// 任意线程可调：窗口尺寸变化 → 重算集体档位（非阻塞入队）。
pub fn request_refit(hwnd: u64) {
    let _ = fit_tx().send(FitMsg::Refit(hwnd));
}

/// 任意线程可调：窗口已关 → 出表并重算（非阻塞入队）。
pub fn request_window_closed(hwnd: u64) {
    let _ = fit_tx().send(FitMsg::Closed(hwnd));
}

fn fit_loop(rx: mpsc::Receiver<FitMsg>) {
    while let Ok(first) = rx.recv() {
        // debounce：静默窗后一次性吞掉积压（合并为一次重算）。
        std::thread::sleep(DEBOUNCE);
        let mut refits = HashSet::new();
        let mut closeds = HashSet::new();
        match first {
            FitMsg::Refit(h) => refits.insert(h),
            FitMsg::Closed(h) => closeds.insert(h),
        };
        while let Ok(msg) = rx.try_recv() {
            match msg {
                FitMsg::Refit(h) => refits.insert(h),
                FitMsg::Closed(h) => {
                    // 同一窗口既关又在积压 refit 中 → 只留关。
                    refits.remove(&h);
                    closeds.insert(h)
                }
            };
        }
        refit_blocking(&refits, &closeds);
    }
}

/// 一次重算：更新需求表 → 集体档位 → 必要时换档 + 广播 + 全窗口重新居中。
fn refit_blocking(refits: &HashSet<u64>, closeds: &HashSet<u64>) {
    let Some(target_fit) = orchestration().with_slot(|_, fit| fit.clone()) else {
        // 没有虚拟屏：只清关窗条目（防泄漏），不选档。
        if !closeds.is_empty() {
            let mut d = demands().lock().unwrap_or_else(|e| e.into_inner());
            for h in closeds {
                d.remove(h);
            }
        }
        return;
    };

    {
        let mut d = demands().lock().unwrap_or_else(|e| e.into_inner());
        for h in closeds {
            d.remove(h);
        }
        for h in refits {
            match rd_window_win::resize::window_client_size(*h as isize) {
                Ok((cw, ch)) => {
                    // 客户区口径（T-049 二期）：内容真需求 vs 档位客户区容量
                    // （fit.rs client_capacity），外框口径会把边框标题栏算进
                    // 需求，铺满场景选档震荡（t15 真机复现）。
                    d.insert(*h, (cw, ch));
                }
                Err(err) => {
                    tracing::debug!(window = format!("{h:#x}"), "量窗口失败（可能已关）: {err}");
                    d.remove(h);
                }
            }
        }
    }

    let target = {
        let d = demands().lock().unwrap_or_else(|e| e.into_inner());
        let has = !d.is_empty();
        let natural = target_fit.width_for_windows(&d.values().copied().collect::<Vec<_>>());
        let hold = *manual_hold().lock().unwrap_or_else(|e| e.into_inner());
        resolve_target(natural, hold, has)
    };
    let current = orchestration()
        .with_slot(|handle, _| handle.info().width)
        .unwrap_or(target);
    if target != current {
        let switched = orchestration().with_slot(|vd, _| {
            vd.set_width(target)?;
            Ok::<_, rd_vdisplay::Error>(vd.info())
        });
        match switched {
            Some(Ok(info)) => {
                tracing::info!(from = current, to = target, "窗口选档联动换档");
                broadcast_switched(info.width, info.height);
                window_session::force_idr_all();
                recenter_all();
            }
            Some(Err(err)) => tracing::warn!(to = target, "选档换档失败: {err}"),
            None => {}
        }
    } else {
        // 档位没变也把新窗口居中（开窗路径窗口可能还停在原位置）。
        for h in refits {
            super::center_window_sync(*h);
        }
    }
}

/// 换档后窗口相对新屏可能偏心/超界：全部需求窗口重新居中。
fn recenter_all() {
    let hwnds: Vec<u64> = demands()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect();
    for h in hwnds {
        super::center_window_sync(h);
    }
}

/// SWITCHED 广播：发往全部活跃会话（清掉死引用）。fit 线程无 tokio 上下文，
/// 经注入的 Handle 逐会话 spawn 发送——单个停滞客户端（发送背压未断开）
/// 只拖慢自己的任务，不阻塞 fit 调度线程（审查 M-2 修复）。通知为
/// best-effort：会话已断（send 失败）静默，引用清理交给 unregister_session。
fn broadcast_switched(width: u32, height: u32) {
    use rd_proto::v0::envelope::Msg;
    use rd_proto::v0::virtual_display_state::Kind;
    use rd_proto::v0::VirtualDisplayState;

    let Some(handle) = broadcast_handle()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    else {
        tracing::debug!("广播 Handle 未注入（无会话建立过），跳过 SWITCHED 通知");
        return;
    };
    let targets: Vec<std::sync::Arc<SessionCtx>> = {
        let mut map = sessions().lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, w| w.upgrade().is_some());
        map.values().filter_map(|w| w.upgrade()).collect()
    };
    let state = VirtualDisplayState {
        kind: Kind::Switched as i32,
        width,
        height,
        message: String::new(),
    };
    for ctx in targets {
        let state = state.clone();
        handle.spawn(async move {
            let _ = ctx.send(Msg::VdisplayState(state), 0).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动档裁决三例（T-049）：下限保持 / 需求变大跟随 / 表空失效。
    #[test]
    fn resolve_target_manual_hold_semantics() {
        // 竞态旧尺寸 refit（自然需求 550 < 手动 1080）→ 保持手动档
        assert_eq!(resolve_target(550, Some(1080), true), 1080);
        // 需求真实变大（自然 1100 封顶前）→ 跟随升档
        assert_eq!(resolve_target(1080, Some(1080), true), 1080);
        assert_eq!(resolve_target(550, None, true), 550, "无手动档=全自动");
        // 表空（全部关窗）→ 失效回落自然需求（基准档），H-1 回落不受影响
        assert_eq!(resolve_target(540, Some(1080), false), 540);
    }

    #[test]
    fn demand_table_roundtrip() {
        let d = demands();
        d.lock().unwrap().insert(0x1234, (800, 600));
        let got = d.lock().unwrap().get(&0x1234).copied();
        d.lock().unwrap().remove(&0x1234);
        assert_eq!(got, Some((800, 600)));
    }
}
