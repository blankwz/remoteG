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

/// 需求表：hwnd → 窗口外框 `(w, h)`（虚拟屏屏幕物理像素）。
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
            match rd_window_win::resize::window_rect(*h as isize) {
                Ok(rect) => {
                    d.insert(*h, (rect.w, rect.h));
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
        target_fit.width_for_windows(&d.values().copied().collect::<Vec<_>>())
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

    #[test]
    fn demand_table_roundtrip() {
        let d = demands();
        d.lock().unwrap().insert(0x1234, (800, 600));
        let got = d.lock().unwrap().get(&0x1234).copied();
        d.lock().unwrap().remove(&0x1234);
        assert_eq!(got, Some((800, 600)));
    }
}
