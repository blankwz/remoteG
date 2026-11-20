//! 虚拟显示器的建屏/换档/撤屏封装（一期）。
//!
//! 三层拆分：
//! - [`fit`]：档位数学（纯逻辑，移植自 snug 的 ScreenFit）；
//! - [`protocol`]：驱动管道线上格式（纯逻辑）；
//! - [`backend`]：后端抽象，Windows 实现在 `windows_backend`。
//!
//! 本模块只做时序编排，规约全部来自 `03_architecture/虚拟屏权限实测.md`：
//! - 建屏时**一次登记全部档位**，之后换档走 CCD（228ms、屏不掉线、编号不变），
//!   而不是 Notify 缩表（380ms 且先掉后亮）；
//! - 撤屏后要等显示真的从系统拓扑消失（约 277ms 量级），调用方再做窗口归位；
//! - 驱动登记是进程级残留风险：上一次会话崩溃留下的同名 monitor 会占住 id，
//!   建屏前必须先清掉。

pub mod fit;
pub mod protocol;

pub mod backend;

#[cfg(windows)]
pub mod windows_backend;

use std::time::{Duration, Instant};

#[cfg(any(test, feature = "test-util"))]
pub use backend::FakeBackend;
pub use backend::{DisplayInfo, VDisplayBackend};
pub use fit::{PhoneSpec, ScreenFit};
pub use protocol::{Mode, Monitor};
#[cfg(windows)]
pub use windows_backend::{displays, WindowsBackend};

/// 驱动登记里标识「这块屏是 rd 的」的名字（建屏前据它清理残留）。
pub const MONITOR_NAME: &str = "rd-vdisplay";

/// 等屏从系统拓扑出现（实测建屏 528ms，留足宽松余量）。
pub const APPEAR_TIMEOUT: Duration = Duration::from_secs(15);
/// 等 CCD 换档生效（实测 228ms）。
pub const MODE_TIMEOUT: Duration = Duration::from_secs(10);
/// 等屏从系统拓扑消失（实测 277ms；再长说明驱动没收到 Remove）。
pub const GONE_TIMEOUT: Duration = Duration::from_secs(10);
/// 轮询间隔。
pub const POLL: Duration = Duration::from_millis(100);

/// 各阶段等待上限（默认值即上面的常量；测试注入短超时用）。
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub appear: Duration,
    pub mode: Duration,
    pub gone: Duration,
    pub poll: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            appear: APPEAR_TIMEOUT,
            mode: MODE_TIMEOUT,
            gone: GONE_TIMEOUT,
            poll: POLL,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("驱动管道不可用（驱动未安装 / 管道被独占）: {0}")]
    DriverUnavailable(String),
    #[error("驱动管道读写失败: {0}")]
    Io(String),
    #[error("驱动的 monitor id（0~9）已全部被占用，无法建屏")]
    NoFreeId,
    #[error("建屏后 {0:?} 内系统拓扑里没出现虚拟显示器")]
    AppearTimeout(Duration),
    #[error("换档到 {width}×{height} 失败: {reason}")]
    SwitchFailed {
        width: u32,
        height: u32,
        reason: String,
    },
    #[error("撤屏后 {0:?} 内虚拟显示器仍未消失")]
    GoneTimeout(Duration),
    #[error("档位 {0} 未登记（须在基准档与 1080 之间、每 10 一档）")]
    UnknownWidth(u32),
    #[error("虚拟屏已撤，不能再换档")]
    Detached,
}

/// 在驱动登记里挑一个空闲 id（0~9；已被任何 monitor 占用的跳过）。
pub fn pick_free_id(monitors: &[Monitor]) -> Option<u32> {
    (0..=protocol::MAX_MONITOR_ID).find(|id| !monitors.iter().any(|m| m.id == *id))
}

/// 把一套档位编成一条驱动 monitor 登记。
fn monitor_for(fit: &ScreenFit, id: u32) -> Monitor {
    use fit::REFRESH_RATE;
    Monitor {
        id,
        name: Some(MONITOR_NAME.to_string()),
        enabled: true,
        modes: fit
            .widths
            .iter()
            .map(|w| {
                let (width, height) = fit.size(*w);
                Mode {
                    width,
                    height,
                    refresh_rates: vec![REFRESH_RATE],
                }
            })
            .collect(),
    }
}

/// 一块已建好的虚拟屏：持有期间可以换档，丢弃（或 [`close`](Self::close)）即撤屏。
#[derive(Debug)]
pub struct VDisplay<B: VDisplayBackend> {
    backend: B,
    fit: ScreenFit,
    monitor_id: u32,
    device_name: Option<String>,
    current: DisplayInfo,
    timeouts: Timeouts,
    closed: bool,
}

impl<B: VDisplayBackend> VDisplay<B> {
    /// 建屏并切到基准档。
    ///
    /// 时序：清残留登记 → 挑空闲 id → 一次登记全部档位 → 等系统拓扑出现
    /// →（若系统给的初始模式不是基准档）CCD 切基准档。
    pub fn with_backend(backend: B, fit: ScreenFit) -> Result<Self, Error> {
        Self::with_backend_and_timeouts(backend, fit, Timeouts::default())
    }

    /// 同 [`with_backend`](Self::with_backend)，可注入各阶段等待上限。
    pub fn with_backend_and_timeouts(
        mut backend: B,
        fit: ScreenFit,
        timeouts: Timeouts,
    ) -> Result<Self, Error> {
        let mut state = backend.driver_state()?;

        // 上一次异常退出留下的同名登记会占住 id，也会让系统里多一块旧屏。
        let stale: Vec<u32> = state
            .iter()
            .filter(|m| m.name.as_deref() == Some(MONITOR_NAME))
            .map(|m| m.id)
            .collect();
        if !stale.is_empty() {
            tracing::warn!(ids = ?stale, "清理残留的虚拟屏登记");
            backend.driver_remove(&stale)?;
            state.retain(|m| !stale.contains(&m.id));
        }

        let id = pick_free_id(&state).ok_or(Error::NoFreeId)?;

        // 认领判据：建屏前拓扑里没有、适配器是虚拟屏驱动的设备。
        let baseline: Vec<String> = backend
            .displays()?
            .into_iter()
            .map(|d| d.device_name)
            .collect();

        let mut all = state;
        all.push(monitor_for(&fit, id));
        backend.driver_notify(&all)?;

        let device = wait_for_new_display(&mut backend, &baseline, timeouts.appear, timeouts.poll)
            .ok_or(Error::AppearTimeout(timeouts.appear))?;

        let mut this = Self {
            backend,
            fit,
            monitor_id: id,
            device_name: Some(device.device_name.clone()),
            current: device,
            timeouts,
            closed: false,
        };
        let base = this.fit.size(this.fit.base_width);
        if (this.current.width, this.current.height) != base {
            this.apply_mode(base)?;
        }
        Ok(this)
    }

    /// 当前设备名（`\\.\DISPLAYn`）。
    pub fn device_name(&self) -> Option<&str> {
        self.device_name.as_deref()
    }

    /// 当前尺寸。
    pub fn size(&self) -> (u32, u32) {
        (self.current.width, self.current.height)
    }

    /// 当前这块屏在拓扑里的完整信息（设备名/位置/尺寸）。
    pub fn info(&self) -> &DisplayInfo {
        &self.current
    }

    /// 当前档位（基准档为其一）。
    pub fn fit(&self) -> &ScreenFit {
        &self.fit
    }

    /// 换档：系统里同一块屏改模式，不撤不建。
    pub fn set_width(&mut self, width: u32) -> Result<(), Error> {
        if !self.fit.contains(width) {
            return Err(Error::UnknownWidth(width));
        }
        self.apply_mode(self.fit.size(width))
    }

    /// 换比例（换手机）：同 id 重登记档位表，系统侧会重建这块屏，再切基准档。
    pub fn set_fit(&mut self, fit: ScreenFit) -> Result<(), Error> {
        let device_name = self.device_name.clone().ok_or(Error::Detached)?;
        let mut others: Vec<Monitor> = self
            .backend
            .driver_state()?
            .into_iter()
            .filter(|m| m.id != self.monitor_id)
            .collect();
        let baseline: Vec<String> = self
            .backend
            .displays()?
            .into_iter()
            .filter(|d| d.device_name != device_name)
            .map(|d| d.device_name)
            .collect();
        others.push(monitor_for(&fit, self.monitor_id));
        self.backend.driver_notify(&others)?;

        let device = wait_for_new_display(
            &mut self.backend,
            &baseline,
            self.timeouts.appear,
            self.timeouts.poll,
        )
        .ok_or(Error::AppearTimeout(self.timeouts.appear))?;
        self.fit = fit;
        self.device_name = Some(device.device_name.clone());
        self.current = device;
        let base = self.fit.size(self.fit.base_width);
        if (self.current.width, self.current.height) != base {
            self.apply_mode(base)?;
        }
        Ok(())
    }

    /// 撤屏并等系统拓扑真的少了一块（后续窗口归位依赖拓扑稳定）。
    pub fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.backend.driver_remove(&[self.monitor_id])?;
        if let Some(device_name) = self.device_name.take() {
            wait_until_gone(
                &mut self.backend,
                &device_name,
                self.timeouts.gone,
                self.timeouts.poll,
            )
            .ok_or(Error::GoneTimeout(self.timeouts.gone))?;
        }
        Ok(())
    }

    fn apply_mode(&mut self, (width, height): (u32, u32)) -> Result<(), Error> {
        let device_name = self.device_name.clone().ok_or(Error::Detached)?;
        self.backend.switch_mode(&device_name, width, height)?;
        let device = wait_for_size(
            &mut self.backend,
            &device_name,
            (width, height),
            self.timeouts.mode,
            self.timeouts.poll,
        )
        .ok_or_else(|| Error::SwitchFailed {
            width,
            height,
            reason: format!("{:?} 内系统未上报新模式", self.timeouts.mode),
        })?;
        self.current = device;
        Ok(())
    }
}

#[cfg(windows)]
impl VDisplay<WindowsBackend> {
    /// 真机入口：连驱动管道建屏（调用线程会阻塞数百毫秒，异步侧请包 `spawn_blocking`）。
    pub fn open(fit: ScreenFit) -> Result<Self, Error> {
        Self::with_backend(WindowsBackend::connect()?, fit)
    }
}

impl<B: VDisplayBackend> Drop for VDisplay<B> {
    /// 兜底撤屏：不清登记的话，下一次建屏会先清残留、但用户会多看到一块旧屏。
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.backend.driver_remove(&[self.monitor_id]);
        }
    }
}

/// 轮询到「拓扑里新出现一块虚拟屏」为止（排除建屏前已存在的设备）。
fn wait_for_new_display<B: VDisplayBackend>(
    backend: &mut B,
    baseline: &[String],
    timeout: Duration,
    poll: Duration,
) -> Option<DisplayInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(list) = backend.displays() {
            if let Some(found) = list.into_iter().find(|d| {
                d.is_virtual_display()
                    && !baseline.contains(&d.device_name)
                    && d.width > 0
                    && d.height > 0
            }) {
                return Some(found);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        backend.sleep(poll);
    }
}

/// 轮询到指定屏上报目标尺寸为止。
fn wait_for_size<B: VDisplayBackend>(
    backend: &mut B,
    device_name: &str,
    size: (u32, u32),
    timeout: Duration,
    poll: Duration,
) -> Option<DisplayInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(list) = backend.displays() {
            if let Some(found) = list
                .into_iter()
                .find(|d| d.device_name == device_name && (d.width, d.height) == size)
            {
                return Some(found);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        backend.sleep(poll);
    }
}

/// 轮询到指定屏从拓扑消失为止。
fn wait_until_gone<B: VDisplayBackend>(
    backend: &mut B,
    device_name: &str,
    timeout: Duration,
    poll: Duration,
) -> Option<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(list) = backend.displays() {
            if !list.iter().any(|d| d.device_name == device_name) {
                return Some(());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        backend.sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backend::FakeBackend;
    use fit::ScreenFit;

    fn standard_fit() -> ScreenFit {
        ScreenFit::standard()
    }

    /// 测试用短超时：超时路径按百毫秒级验证，别让单测等真实秒数。
    fn fast() -> Timeouts {
        Timeouts {
            appear: Duration::from_millis(300),
            mode: Duration::from_millis(300),
            gone: Duration::from_millis(300),
            poll: Duration::from_millis(5),
        }
    }

    #[test]
    fn open_creates_display_at_base_tier() {
        let mut fake = FakeBackend::new();
        fake.appear_delay = 2; // 模拟建屏延迟
        let vd = VDisplay::with_backend(fake, standard_fit()).unwrap();
        assert_eq!(vd.size(), (540, 950));
        assert_eq!(vd.device_name(), Some(r"\\.\DISPLAY_VD"));
    }

    #[test]
    fn open_registers_every_tier_once() {
        let mut vd = VDisplay::with_backend(FakeBackend::new(), standard_fit()).unwrap();
        let state = vd.backend.driver_state().unwrap();
        assert_eq!(state.len(), 1);
        let mon = &state[0];
        assert_eq!(mon.name.as_deref(), Some(MONITOR_NAME));
        assert_eq!(mon.modes.len(), standard_fit().widths.len());
        assert_eq!(mon.modes.first().unwrap().width, 540);
        assert_eq!(mon.modes.last().unwrap().width, 1080);
        // 基准档已是系统当前模式 → 不该有多余的 CCD 切换
        assert!(
            vd.backend.switch_calls.is_empty(),
            "{:?}",
            vd.backend.switch_calls
        );
    }

    #[test]
    fn open_picks_lowest_free_id() {
        let mut fake = FakeBackend::new();
        for id in 0..=4 {
            fake.monitors.push(Monitor {
                id,
                name: None,
                enabled: true,
                modes: vec![],
            });
        }
        let mut vd = VDisplay::with_backend(fake, standard_fit()).unwrap();
        assert_eq!(vd.monitor_id, 5);
        // 别人的登记必须原样保留
        let state = vd.backend.driver_state().unwrap();
        assert_eq!(state.len(), 6);
        assert!(state.iter().filter(|m| m.name.is_none()).count() == 5);
    }

    #[test]
    fn open_fails_when_all_ids_taken() {
        let mut fake = FakeBackend::new();
        for id in 0..=protocol::MAX_MONITOR_ID {
            fake.monitors.push(Monitor {
                id,
                name: None,
                enabled: true,
                modes: vec![],
            });
        }
        let err = VDisplay::with_backend(fake, standard_fit()).unwrap_err();
        assert!(matches!(err, Error::NoFreeId));
    }

    #[test]
    fn open_cleans_stale_registration_left_by_crash() {
        let mut fake = FakeBackend::new();
        fake.monitors.push(Monitor {
            id: 3,
            name: Some(MONITOR_NAME.into()),
            enabled: true,
            modes: vec![Mode {
                width: 540,
                height: 950,
                refresh_rates: vec![60],
            }],
        });
        let mut vd = VDisplay::with_backend(fake, standard_fit()).unwrap();
        // 残留那条被清掉，且它的 id 被复用（最低空闲）
        assert_eq!(vd.monitor_id, 0);
        let state = vd.backend.driver_state().unwrap();
        assert_eq!(
            state
                .iter()
                .filter(|m| m.name.as_deref() == Some(MONITOR_NAME))
                .count(),
            1
        );
    }

    #[test]
    fn open_times_out_when_display_never_appears() {
        let mut fake = FakeBackend::new();
        fake.appear_fail = true;
        let err = VDisplay::with_backend_and_timeouts(fake, standard_fit(), fast()).unwrap_err();
        assert!(matches!(err, Error::AppearTimeout(_)), "{err}");
    }

    #[test]
    fn set_width_switches_mode_without_recreate() {
        let mut vd = VDisplay::with_backend(FakeBackend::new(), standard_fit()).unwrap();
        vd.set_width(1080).unwrap();
        assert_eq!(vd.size(), (1080, 1900));
        assert_eq!(
            vd.backend.switch_calls,
            vec![(r"\\.\DISPLAY_VD".to_string(), 1080, 1900)]
        );
        // 屏身份没变（编号不变 = 窗口不归位的语义基础）
        assert_eq!(vd.device_name(), Some(r"\\.\DISPLAY_VD"));
    }

    #[test]
    fn set_width_rejects_unregistered_tier() {
        let mut vd = VDisplay::with_backend(FakeBackend::new(), standard_fit()).unwrap();
        assert!(matches!(vd.set_width(545), Err(Error::UnknownWidth(545))));
        assert!(matches!(vd.set_width(1090), Err(Error::UnknownWidth(1090))));
    }

    #[test]
    fn set_width_fails_when_system_does_not_apply() {
        let mut fake = FakeBackend::new();
        fake.switch_fail = true;
        let mut vd = VDisplay::with_backend(fake, standard_fit()).unwrap();
        let err = vd.set_width(1080).unwrap_err();
        assert!(matches!(err, Error::SwitchFailed { .. }));
        assert_eq!(vd.size(), (540, 950));
    }

    #[test]
    fn set_fit_replaces_tiers_and_returns_to_new_base() {
        let fit = ScreenFit::for_ratio(2.0);
        let mut vd = VDisplay::with_backend(FakeBackend::new(), standard_fit()).unwrap();
        vd.set_fit(fit.clone()).unwrap();
        let state = vd.backend.driver_state().unwrap();
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].modes.len(), fit.widths.len());
        assert_eq!(
            state[0].modes.first().unwrap(),
            &Mode {
                width: 540,
                height: 1080,
                refresh_rates: vec![60]
            }
        );
        assert_eq!(vd.size(), (540, 1080));
    }

    #[test]
    fn close_removes_registration_and_waits_for_gone() {
        let mut vd = VDisplay::with_backend(FakeBackend::new(), standard_fit()).unwrap();
        vd.close().unwrap();
        assert!(vd.backend.driver_state().unwrap().is_empty());
        assert_eq!(vd.device_name(), None);
        // 二次 close 幂等
        vd.close().unwrap();
    }

    #[test]
    fn drop_removes_registration() {
        let vd = VDisplay::with_backend(FakeBackend::new(), standard_fit()).unwrap();
        drop(vd);
    }

    #[test]
    fn set_width_after_close_is_detached() {
        let mut vd = VDisplay::with_backend(FakeBackend::new(), standard_fit()).unwrap();
        vd.close().unwrap();
        assert!(matches!(vd.set_width(540), Err(Error::Detached)));
    }
}
