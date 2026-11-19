//! Windows 后端：命名管道客户端 + 显示拓扑读取 + CCD 换档。
//!
//! 管道按 master 驱动的 **byte 模式**通信（服务端 `ServerOptions` 未设
//! `pipe_mode`，读写都可截断在任意字节边界），因此用 `std::fs::File` 打开
//! `\\.\pipe\virtualdisplaydriver` 即可，读侧用 `PeekNamedPipe` 轮询可用字节
//! 再读，不需要额外线程，也不会因阻塞读留下无法回收的线程。
//!
//! 换档走 `ChangeDisplaySettingsExW`（实测 228ms、屏不掉线、编号不变），
//! 而不是缩 Notify 档位表（380ms 且先掉后亮）。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsExW, CDS_TYPE, DEVMODEW,
    DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
};
use windows::Win32::System::Pipes::{PeekNamedPipe, WaitNamedPipeW};

use crate::backend::{DisplayInfo, VDisplayBackend};
use crate::protocol::{self, DriverCommand, FrameSplitter, Incoming, Monitor};
use crate::Error;

/// 连管道的总超时（驱动没装/服务没起时快速失败，别把会话拖住）。
pub const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 等驱动应答 `State` 的超时。
pub const STATE_TIMEOUT: Duration = Duration::from_secs(5);
/// 读侧轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// `\\.\pipe\virtualdisplaydriver`
fn pipe_path() -> String {
    format!(r"\\.\pipe\{}", protocol::DEFAULT_PIPE_NAME)
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(b: &[u16]) -> String {
    b.iter()
        .take_while(|c| **c != 0)
        .copied()
        .map(|c| char::from_u32(c as u32).unwrap_or('\u{fffd}'))
        .collect()
}

/// 驱动管道客户端（单连接、单线程内串行使用）。
#[derive(Debug)]
struct PipeClient {
    file: File,
    splitter: FrameSplitter,
    frames: VecDeque<Vec<u8>>,
}

impl PipeClient {
    fn connect() -> Result<Self, Error> {
        let path = pipe_path();
        let wide = to_wide(&path);
        let deadline = Instant::now() + PIPE_CONNECT_TIMEOUT;
        loop {
            // byte 模式 + 默认 4096 缓冲：通用读写法即可
            match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        splitter: FrameSplitter::new(),
                        frames: VecDeque::new(),
                    })
                }
                Err(e) => {
                    let code = e.raw_os_error();
                    let busy = matches!(code, Some(231)); // ERROR_PIPE_BUSY
                    let missing = matches!(code, Some(2)); // ERROR_FILE_NOT_FOUND：驱动服务没起
                    if (!busy && !missing) || Instant::now() >= deadline {
                        return Err(Error::DriverUnavailable(format!("{path}: {e}")));
                    }
                    if busy {
                        // 等这一轮实例被释放；超时/失败都继续重试，由 deadline 兜底
                        unsafe {
                            let _ = WaitNamedPipeW(PCWSTR::from_raw(wide.as_ptr()), 200);
                        }
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.file.as_raw_handle())
    }

    fn write_command(&mut self, cmd: &DriverCommand<'_>) -> Result<(), Error> {
        let bytes =
            protocol::encode_command(cmd).map_err(|e| Error::Io(format!("命令序列化失败: {e}")))?;
        self.file
            .write_all(&bytes)
            .map_err(|e| Error::Io(format!("写驱动管道失败: {e}")))
    }

    /// 读到一条满足 `want` 的入站消息；超时返回 `None`。
    fn next_incoming(
        &mut self,
        timeout: Duration,
        mut want: impl FnMut(&Incoming) -> bool,
    ) -> Result<Option<Incoming>, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = self.frames.pop_front() {
                if let Some(msg) = protocol::parse_incoming(&frame) {
                    if want(&msg) {
                        return Ok(Some(msg));
                    }
                }
                continue;
            }
            let mut available = 0u32;
            let peek =
                unsafe { PeekNamedPipe(self.handle(), None, 0, None, Some(&mut available), None) };
            peek.map_err(|e| Error::Io(format!("PeekNamedPipe 失败: {e}")))?;
            if available > 0 {
                let mut buf = [0u8; 4096];
                let n = self
                    .file
                    .read(&mut buf)
                    .map_err(|e| Error::Io(format!("读驱动管道失败: {e}")))?;
                if n == 0 {
                    return Err(Error::DriverUnavailable("驱动端关闭了管道".into()));
                }
                for frame in self.splitter.push(&buf[..n]) {
                    self.frames.push_back(frame);
                }
                continue;
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn request_state(&mut self) -> Result<Vec<Monitor>, Error> {
        self.file
            .write_all(&protocol::encode_state_request())
            .map_err(|e| Error::Io(format!("写驱动管道失败: {e}")))?;
        match self.next_incoming(STATE_TIMEOUT, |i| matches!(i, Incoming::State(_)))? {
            Some(Incoming::State(monitors)) => Ok(monitors),
            _ => Err(Error::Io(format!(
                "驱动 {STATE_TIMEOUT:?} 内没有应答 State 请求"
            ))),
        }
    }
}

/// Windows 后端：驱动管道 + 本机显示系统。
#[derive(Debug)]
pub struct WindowsBackend {
    pipe: PipeClient,
}

impl WindowsBackend {
    /// 连上驱动管道（驱动未装/服务未起 → [`Error::DriverUnavailable`]）。
    pub fn connect() -> Result<Self, Error> {
        Ok(Self {
            pipe: PipeClient::connect()?,
        })
    }
}

impl VDisplayBackend for WindowsBackend {
    fn driver_state(&mut self) -> Result<Vec<Monitor>, Error> {
        self.pipe.request_state()
    }

    fn driver_notify(&mut self, monitors: &[Monitor]) -> Result<(), Error> {
        self.pipe.write_command(&DriverCommand::Notify(monitors))
    }

    fn driver_remove(&mut self, ids: &[u32]) -> Result<(), Error> {
        self.pipe.write_command(&DriverCommand::Remove(ids))
    }

    fn displays(&mut self) -> Result<Vec<DisplayInfo>, Error> {
        enumerate_displays()
    }

    fn switch_mode(&mut self, device_name: &str, width: u32, height: u32) -> Result<(), Error> {
        switch_display_mode(device_name, width, height)
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// 枚举桌面上已接入的显示器及其当前模式。
fn enumerate_displays() -> Result<Vec<DisplayInfo>, Error> {
    let mut out = Vec::new();
    for index in 0..64u32 {
        let mut device = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let present = unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0) };
        if !present.as_bool() {
            break;
        }
        // 只看接入桌面的（未接入的虚拟屏节点不占桌面，不能当可用屏）
        if !device.StateFlags.contains(DISPLAY_DEVICE_ACTIVE) {
            continue;
        }
        let device_name = from_wide(&device.DeviceName);
        let adapter = from_wide(&device.DeviceString);

        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let got = unsafe {
            EnumDisplaySettingsExW(
                PCWSTR::from_raw(device.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
        };
        if !got.as_bool() {
            continue;
        }
        let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
        out.push(DisplayInfo {
            device_name,
            adapter,
            position: (position.x, position.y),
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
        });
    }
    Ok(out)
}

/// CCD 换档：只改宽高、保留位置/方向，不落注册表（临时会话态）。
fn switch_display_mode(device_name: &str, width: u32, height: u32) -> Result<(), Error> {
    let wide = to_wide(device_name);
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let got = unsafe {
        EnumDisplaySettingsExW(
            PCWSTR::from_raw(wide.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
    };
    if !got.as_bool() {
        return Err(Error::SwitchFailed {
            width,
            height,
            reason: format!("读不到 {device_name} 的当前模式"),
        });
    }
    mode.dmPelsWidth = width;
    mode.dmPelsHeight = height;
    mode.dmFields = mode.dmFields | DM_PELSWIDTH | DM_PELSHEIGHT;
    let code = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR::from_raw(wide.as_ptr()),
            Some(&mode as *const DEVMODEW),
            None,
            CDS_TYPE(0),
            None,
        )
    };
    if code != DISP_CHANGE_SUCCESSFUL {
        return Err(Error::SwitchFailed {
            width,
            height,
            reason: format!("ChangeDisplaySettingsExW 返回 {}", code.0),
        });
    }
    Ok(())
}
