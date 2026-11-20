//! 驱动管道与显示系统的最小抽象。
//!
//! 建屏/撤屏/换档的真实时序（等屏出现、等模式生效、等屏消失）是这一层之上
//! 的事；这里只要求后端能「发命令 + 报当前拓扑」，便于用假后端测编排逻辑。

use std::time::Duration;

use crate::protocol::Monitor;
use crate::Error;

/// 系统拓扑中的一块显示器（仅本 crate 关心的字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    /// `\\.\DISPLAY7` 形态的设备名（CCD 换档的入参）。
    pub device_name: String,
    /// 适配器字符串（`EnumDisplayDevices.DeviceString`，虚拟屏含 "Virtual Display"）。
    pub adapter: String,
    /// 桌面坐标（虚拟屏建到哪个位置由系统决定，换档后可能变化）。
    pub position: (i32, i32),
    pub width: u32,
    pub height: u32,
}

impl DisplayInfo {
    /// 是否是 virtual-display-rs 的屏（按适配器字符串识别，跨设备名重排稳定）。
    pub fn is_virtual_display(&self) -> bool {
        self.adapter
            .to_ascii_lowercase()
            .contains("virtual display")
    }
}

/// 驱动管道 + 显示系统的最小操作集。
pub trait VDisplayBackend {
    /// 驱动当前的 monitor 登记状态（`State` 请求）。
    fn driver_state(&mut self) -> Result<Vec<Monitor>, Error>;
    /// 全量替换驱动登记状态（`Notify`）。
    fn driver_notify(&mut self, monitors: &[Monitor]) -> Result<(), Error>;
    /// 按 id 移除登记（`Remove`）。
    fn driver_remove(&mut self, ids: &[u32]) -> Result<(), Error>;
    /// 当前系统拓扑（含各屏当前模式与位置）。
    fn displays(&mut self) -> Result<Vec<DisplayInfo>, Error>;
    /// 用 CCD 把指定屏切到 `width × height`（调用方负责等生效）。
    fn switch_mode(&mut self, device_name: &str, width: u32, height: u32) -> Result<(), Error>;
    /// 等待/轮询用（假后端可立即返回，测编排逻辑时不用真睡眠）。
    fn sleep(&mut self, duration: Duration);
}

/// 测试用假后端：驱动状态 + 显示拓扑都由内存模拟，含「屏出现延迟」与故障注入。
///
/// rd-vdisplay 自身测试与本 crate 的 `test-util` feature 消费方（rd-agent
/// 编排层单测，T-049 M-4 清偿）共用。
#[cfg(any(test, feature = "test-util"))]
#[derive(Debug, Default)]
pub struct FakeBackend {
    /// 驱动登记的 monitor（与 `Notify` 语义一致：全量替换）。
    pub monitors: Vec<Monitor>,
    /// 建屏前已有的屏（物理屏 + 第三方虚拟屏），不会被本 crate 认领。
    pub base_displays: Vec<DisplayInfo>,
    /// 驱动每次 Notify 之后，屏要「过几轮 displays() 才出现」（模拟 500ms 级建屏延迟）。
    pub appear_delay: u32,
    /// true = 屏永远不出现（模拟驱动拒绝/超界 id）。
    pub appear_fail: bool,
    /// true = 换档永远不生效（模拟驱动不支持该模式）。
    pub switch_fail: bool,
    /// CCD 换档调用记录（(设备名, 宽, 高)）。
    pub switch_calls: Vec<(String, u32, u32)>,
    /// 我们那块屏的当前尺寸（屏还没出现时为 None）。
    our_size: Option<(u32, u32)>,
    pending: u32,
}

#[cfg(any(test, feature = "test-util"))]
impl FakeBackend {
    /// 建一个只有物理屏、驱动状态为空的世界。
    pub fn new() -> Self {
        Self {
            base_displays: vec![DisplayInfo {
                device_name: r"\\.\DISPLAY1".into(),
                adapter: "NVIDIA GeForce GTX 1060".into(),
                position: (0, 0),
                width: 1920,
                height: 1080,
            }],
            ..Self::default()
        }
    }

    /// 预置一条驱动登记（模拟上一次会话崩溃残留）。
    pub fn with_monitor(mut self, monitor: Monitor) -> Self {
        self.our_size = monitor.modes.first().map(|m| (m.width, m.height));
        self.pending = 0;
        self.monitors.push(monitor);
        self
    }

    fn our_present(&self) -> bool {
        self.monitors
            .iter()
            .any(|m| m.name.as_deref() == Some(crate::MONITOR_NAME) && m.enabled)
            && !self.appear_fail
    }
}

#[cfg(any(test, feature = "test-util"))]
const FAKE_DEVICE: &str = r"\\.\DISPLAY_VD";

#[cfg(any(test, feature = "test-util"))]
impl VDisplayBackend for FakeBackend {
    fn driver_state(&mut self) -> Result<Vec<Monitor>, Error> {
        Ok(self.monitors.clone())
    }

    fn driver_notify(&mut self, monitors: &[Monitor]) -> Result<(), Error> {
        let had = self.our_present();
        self.monitors = monitors.to_vec();
        let has = self.our_present();
        if has && !had {
            self.pending = self.appear_delay;
        }
        if !has {
            self.our_size = None;
        }
        Ok(())
    }

    fn driver_remove(&mut self, ids: &[u32]) -> Result<(), Error> {
        self.monitors.retain(|m| !ids.contains(&m.id));
        if !self.our_present() {
            self.our_size = None;
        }
        Ok(())
    }

    fn displays(&mut self) -> Result<Vec<DisplayInfo>, Error> {
        let mut list = self.base_displays.clone();
        if self.our_present() {
            if self.pending > 0 {
                self.pending -= 1;
            } else {
                let size = self
                    .our_size
                    .or_else(|| {
                        self.monitors
                            .iter()
                            .find(|m| m.name.as_deref() == Some(crate::MONITOR_NAME))
                            .and_then(|m| m.modes.first().map(|x| (x.width, x.height)))
                    })
                    .unwrap_or((0, 0));
                list.push(DisplayInfo {
                    device_name: FAKE_DEVICE.into(),
                    adapter: "Virtual Display Driver (fake)".into(),
                    position: (1920, 0),
                    width: size.0,
                    height: size.1,
                });
            }
        }
        Ok(list)
    }

    fn switch_mode(&mut self, device_name: &str, width: u32, height: u32) -> Result<(), Error> {
        self.switch_calls
            .push((device_name.to_string(), width, height));
        if self.switch_fail {
            return Err(Error::SwitchFailed {
                width,
                height,
                reason: "模拟失败".into(),
            });
        }
        self.our_size = Some((width, height));
        Ok(())
    }

    fn sleep(&mut self, _duration: Duration) {}
}
