//! 编排层竞态单测（T-049 M-4 清偿）。
//!
//! 状态机与 IO 分离后，`rd-vdisplay` 的 `FakeBackend`（`test-util` feature）
//! 可注入 [`Orchestrator`]：一期审查指出的三个竞态分支——建屏排队归零、
//! 建屏完成归零立即撤回、撤屏与复用交错——全部确定性复现（交错点用工厂
//! 闭包/调用顺序精确控制，不靠睡眠概率）。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use rd_vdisplay::FakeBackend;

use super::orch::{Action, DisplayFactory, Orchestrator};

/// 假工厂：每次建屏用全新 FakeBackend，记录调用次数。
fn fake_factory(calls: &Arc<AtomicUsize>) -> DisplayFactory {
    let calls = Arc::clone(calls);
    Arc::new(move |fit| {
        calls.fetch_add(1, Ordering::SeqCst);
        let vd = rd_vdisplay::VDisplay::with_backend(FakeBackend::new(), fit)?;
        Ok(Box::new(vd) as _)
    })
}

fn std_fit() -> rd_vdisplay::ScreenFit {
    rd_vdisplay::ScreenFit::standard()
}

#[tokio::test]
async fn happy_path_acquire_ensure_reuse_release_destroy() {
    let calls = Arc::new(AtomicUsize::new(0));
    let orch = Orchestrator::new(fake_factory(&calls));

    // A 接入：首个会话 → 建屏
    assert!(orch.acquire());
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::Created {
            width: 540,
            height: 950
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // B 接入：复用（不再建屏）
    assert!(!orch.acquire());
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::Created {
            width: 540,
            height: 950
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "复用不得再次建屏");

    // A 离开（B 仍持有）→ 不撤
    assert!(!orch.release());
    assert!(orch.take_if_last().await.is_none(), "引用 > 0 不得撤屏");

    // B 离开 → 归零撤屏
    assert!(orch.release());
    let (fit, mut handle) = orch.take_if_last().await.expect("归零后应拿到句柄");
    assert_eq!(fit.base_width, 540);
    handle.close().expect("撤屏应成功");
    // 撤屏后槽已空：再撤无事发生；新会话接入会重新建屏
    assert!(orch.take_if_last().await.is_none());
    assert!(orch.acquire());
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::Created {
            width: 540,
            height: 950
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "撤屏后新会话重建");
}

#[tokio::test]
async fn queued_ensure_with_zero_refs_skips_factory() {
    // 竞态 1：建屏任务排队期间会话全部断开——工厂不应被调用。
    let calls = Arc::new(AtomicUsize::new(0));
    let orch = Orchestrator::new(fake_factory(&calls));
    assert!(orch.acquire());
    assert!(orch.release(), "先归零再触发 ensure");

    assert_eq!(orch.ensure_display(std_fit()).await, Action::None);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "归零后工厂不应被调用");
    assert!(orch.take_if_last().await.is_none());
}

#[tokio::test]
async fn display_ready_after_refs_dropped_is_withdrawn() {
    // 竞态 2：建屏完成那一刻引用已归零——立即撤回，不让空屏残留。
    // 交错点用工厂闭包精确控制：工厂返回前 release（= 建屏期间会话离开）。
    let cell: Arc<OnceLock<Arc<Orchestrator>>> = Arc::new(OnceLock::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let cell_for_factory = Arc::clone(&cell);
    let calls_for_factory = Arc::clone(&calls);
    let factory: DisplayFactory = Arc::new(move |fit| {
        let orch = cell_for_factory.get().expect("orch 已就绪");
        orch.release(); // 模拟建屏阻塞期间最后一个会话断开
        calls_for_factory.fetch_add(1, Ordering::SeqCst);
        let vd = rd_vdisplay::VDisplay::with_backend(FakeBackend::new(), fit)?;
        Ok(Box::new(vd) as _)
    });
    let orch = Arc::new(Orchestrator::new(factory));
    cell.set(Arc::clone(&orch)).expect("OnceLock 只设置一次");

    assert!(orch.acquire());
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::None,
        "建屏完成但引用归零 → 撤回"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(orch.take_if_last().await.is_none(), "撤回后槽必须为空");
}

#[tokio::test]
async fn stop_racing_new_session_leaves_display_for_reuse() {
    // 竞态 3（撤屏与复用交错）：A 归零发起撤屏，同一时刻 B 接入——
    // take_if_last 的锁内复查必须把屏留给 B（B 复用，不得拿到将消失的屏）。
    let calls = Arc::new(AtomicUsize::new(0));
    let orch = Orchestrator::new(fake_factory(&calls));

    assert!(orch.acquire()); // A
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::Created {
            width: 540,
            height: 950
        }
    );
    assert!(orch.release()); // A 归零（B 尚未 +1）
    assert!(orch.acquire(), "B 接入：refs 0→1，成为新的首个持有者");
    // B 复用（工厂不再调用），A 的撤屏决策随后裁决：refs 已因 B 回到 1 → 不撤
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::Created {
            width: 540,
            height: 950
        }
    );
    // A 的 stop 走到撤屏决策：refs 已因 B 回到 1 → 不撤
    assert!(orch.take_if_last().await.is_none(), "B 持有期间 A 不得撤屏");

    // B 离开才真正撤屏
    assert!(orch.release());
    let (_, mut handle) = orch.take_if_last().await.expect("B 归零后可撤");
    handle.close().expect("撤屏应成功");
}

#[tokio::test]
async fn factory_failure_reports_failed_and_session_retries() {
    let calls = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(AtomicBool::new(true));
    let fail_for_factory = Arc::clone(&fail);
    let factory: DisplayFactory = Arc::new(move |fit| {
        calls.fetch_add(1, Ordering::SeqCst);
        if fail_for_factory.load(Ordering::SeqCst) {
            return Err(rd_vdisplay::Error::NoFreeId);
        }
        let vd = rd_vdisplay::VDisplay::with_backend(FakeBackend::new(), fit)?;
        Ok(Box::new(vd) as _)
    });
    let orch = Orchestrator::new(factory);

    assert!(orch.acquire());
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::Failed("驱动的 monitor id（0~9）已全部被占用，无法建屏".into())
    );
    assert!(orch.take_if_last().await.is_none(), "失败不得占槽");

    // 会话仍持有引用；下一次接入重试（fail 解除）→ 成功
    fail.store(false, Ordering::SeqCst);
    assert_eq!(
        orch.ensure_display(std_fit()).await,
        Action::Created {
            width: 540,
            height: 950
        }
    );
    assert!(orch.release());
    let (_, mut handle) = orch.take_if_last().await.expect("成功后可撤");
    handle.close().expect("撤屏应成功");
}
