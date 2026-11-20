//! 虚拟屏编排层：引用计数状态机 + 建屏/撤屏动作（T-049 M-4 清偿）。
//!
//! 编排与 IO 分离：状态机方法产出 [`Action`] 指令（测试断言指令序列），
//! 生产侧（[`super`]）负责执行（发通知 / spawn_blocking 撤屏）。后端经
//! [`DisplayHandle`] trait object 注入，建屏经 [`DisplayFactory`] 注入——
//! `Shared` 不再硬绑 `WindowsBackend`，竞态分支（建屏排队归零 / 建屏完成
//! 归零 / 撤屏与复用交错）可用 `rd-vdisplay` 的 `FakeBackend`（`test-util`
//! feature）确定性单测，见 [`super::tests`]。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rd_vdisplay::{DisplayInfo, ScreenFit, VDisplay, VDisplayBackend};

/// 一块已建好的虚拟屏的句柄抽象（编排层视角）。
pub trait DisplayHandle: Send {
    /// 换档（CCD，阻塞数百毫秒；调用方自行放到阻塞线程）。
    fn set_width(&mut self, width: u32) -> Result<(), rd_vdisplay::Error>;
    /// 撤屏并等系统拓扑真消失（阻塞数百毫秒）。
    fn close(&mut self) -> Result<(), rd_vdisplay::Error>;
    /// 当前屏信息（设备名/位置/尺寸）。
    fn info(&self) -> DisplayInfo;
}

impl<B: VDisplayBackend + Send> DisplayHandle for VDisplay<B> {
    fn set_width(&mut self, width: u32) -> Result<(), rd_vdisplay::Error> {
        VDisplay::set_width(self, width)
    }

    fn close(&mut self) -> Result<(), rd_vdisplay::Error> {
        VDisplay::close(self)
    }

    fn info(&self) -> DisplayInfo {
        VDisplay::info(self).clone()
    }
}

/// 状态机产出的待执行动作（IO 与状态分离，见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 无事发生（含竞态分支吞掉的情形）。
    None,
    /// 建屏失败（会话继续，窗口留在物理屏）。
    Failed(String),
    /// 屏已就绪（新建或复用），报当前尺寸。
    Created { width: u32, height: u32 },
}

/// 建屏工厂：按档位建一块屏。生产 = `VDisplay::open`；测试注入假工厂。
pub type DisplayFactory =
    Arc<dyn Fn(ScreenFit) -> Result<Box<dyn DisplayHandle>, rd_vdisplay::Error> + Send + Sync>;

/// 生产工厂：连真驱动管道建屏（阻塞，调用方放阻塞线程）。
pub fn production_factory() -> DisplayFactory {
    Arc::new(|fit| VDisplay::open(fit).map(|vd| Box::new(vd) as Box<dyn DisplayHandle>))
}

/// 屏槽位：档位表与句柄同存（fit 调度重算集体档位需要档位表）。
struct Slot {
    fit: ScreenFit,
    handle: Box<dyn DisplayHandle>,
}

/// 引用计数编排器：全部字段内部可变，方法 `&self`（全局单例与测试实例同型）。
pub struct Orchestrator {
    /// 建好的屏（None = 未建/已撤/正在撤）。
    slot: Mutex<Option<Slot>>,
    /// 建屏/撤屏串行化（撤屏决策在锁内复查引用，防「A 归零撤屏」与
    /// 「B 刚 +1 复用」交错——一期语义原样保留）。
    serial: tokio::sync::Mutex<()>,
    /// 持有虚拟屏的会话数（降到 0 才真正撤屏）。
    refs: AtomicUsize,
    /// 建屏工厂。
    factory: DisplayFactory,
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new(production_factory())
    }
}

impl std::fmt::Debug for Orchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Orchestrator")
            .field("refs", &self.refs.load(Ordering::SeqCst))
            .field(
                "display",
                &self
                    .with_slot(|vd, _| vd.info().device_name.clone())
                    .is_some(),
            )
            .finish()
    }
}

impl Orchestrator {
    pub fn new(factory: DisplayFactory) -> Self {
        Self {
            slot: Mutex::new(None),
            serial: tokio::sync::Mutex::const_new(()),
            refs: AtomicUsize::new(0),
            factory,
        }
    }

    /// 会话接入：引用 +1，返回是否本会话是唯一持有者（首个接入者发起建屏）。
    pub fn acquire(&self) -> bool {
        self.refs.fetch_add(1, Ordering::SeqCst) == 0
    }

    /// 会话离开：引用 -1，返回是否归零（归零者负责撤屏）。
    pub fn release(&self) -> bool {
        self.refs.fetch_sub(1, Ordering::SeqCst) == 1
    }

    /// 当前引用数。
    #[cfg(test)]
    pub fn refs(&self) -> usize {
        self.refs.load(Ordering::SeqCst)
    }

    /// 对屏槽位执行一次操作（未建屏 → None）。阻塞数百毫秒的操作由调用方
    /// 决定线程（RPC 路径包 spawn_blocking；fit 线程直接调）。
    pub fn with_slot<R>(
        &self,
        f: impl FnOnce(&mut Box<dyn DisplayHandle>, &ScreenFit) -> R,
    ) -> Option<R> {
        let mut guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let Slot { fit, handle } = guard.as_mut()?;
        Some(f(handle, fit))
    }

    /// 撤屏决策：归零且屏在槽时拿走句柄（调用方执行 close）。与建屏同一把
    /// 串行锁，锁内复查引用——引用 > 0 说明撤屏间隙有新会话接入，屏留给它。
    pub async fn take_if_last(&self) -> Option<(ScreenFit, Box<dyn DisplayHandle>)> {
        let _serial = self.serial.lock().await;
        if self.refs.load(Ordering::SeqCst) > 0 {
            return None;
        }
        self.slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(|s| (s.fit, s.handle))
    }

    /// 建屏（若尚未建）：串行锁内查引用 → 复用/建屏 → 归零竞态兜底。
    ///
    /// 竞态分支（一期语义原样保留，现可测）：
    /// - 建屏排队期间引用归零 → 不建（工厂不应被调用）；
    /// - 工厂返回时引用已归零 → 立即撤回，不让空屏残留。
    pub async fn ensure_display(&self, fit: ScreenFit) -> Action {
        let _serial = self.serial.lock().await;
        if self.refs.load(Ordering::SeqCst) == 0 {
            return Action::None;
        }
        // 别的会话已经建好 → 直接复用（回报现有尺寸，不因新会话的 fit 重建）。
        if let Some(info) = self
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.handle.info())
        {
            tracing::info!(display = %info.device_name, "会话复用已存在的虚拟屏");
            return Action::Created {
                width: info.width,
                height: info.height,
            };
        }

        let factory = Arc::clone(&self.factory);
        let slot_fit = fit.clone();
        let opened = tokio::task::spawn_blocking(move || factory(fit)).await;
        let handle = match opened {
            Ok(Ok(handle)) => handle,
            Ok(Err(err)) => {
                tracing::warn!("建屏失败(会话继续,窗口留在物理屏): {err}");
                return Action::Failed(err.to_string());
            }
            Err(err) => {
                tracing::warn!("建屏任务异常: {err}");
                return Action::Failed("建屏任务异常".to_string());
            }
        };

        let info = handle.info();
        // 建屏期间会话已全部结束：立刻撤回（阻塞，但已无会话可拖慢）。
        if self.refs.load(Ordering::SeqCst) == 0 {
            tracing::info!("建屏完成但已无会话,立即撤屏");
            let _ = tokio::task::spawn_blocking(move || {
                let mut handle = handle;
                if let Err(err) = handle.close() {
                    tracing::warn!("撤屏失败: {err}");
                }
            })
            .await;
            return Action::None;
        }
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Slot {
            fit: slot_fit,
            handle,
        });
        tracing::info!(
            display = %info.device_name,
            w = info.width,
            h = info.height,
            "虚拟屏已建立"
        );
        Action::Created {
            width: info.width,
            height: info.height,
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// trait object 装箱/方法转发 sanity（真测试在 tests.rs，走 FakeBackend）。
    #[test]
    fn display_handle_is_object_safe_and_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn DisplayHandle>>();
    }
}
