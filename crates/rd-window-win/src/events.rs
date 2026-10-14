//! 窗口事件：`SetWinEventHook`（WINEVENT_OUTOFCONTEXT）+ 独立消息循环线程。
//!
//! 监听 `EVENT_OBJECT_CREATE/DESTROY/NAMECHANGE/LOCATIONCHANGE` 与
//! `EVENT_SYSTEM_MENUPOPUPSTART/END`；只处理 `idObject == OBJID_WINDOW`
//! （菜单事件兼容 `OBJID_CLIENT`，弹出菜单以该对象标识上报）。`LOCATIONCHANGE`
//! 按窗口节流 50 ms，避免移动/动画期间刷屏。`Stop` 通过 `WM_QUIT` 结束线程。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PostThreadMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_MENUPOPUPEND,
    EVENT_SYSTEM_MENUPOPUPSTART, MSG, OBJID_CLIENT, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WM_QUIT,
};

/// 同一窗口相邻两次 `Moved` 事件的最小间隔。
const MOVE_THROTTLE: Duration = Duration::from_millis(50);
/// 节流表的清理阈值（防止长期运行时按历史 hwnd 无界增长）。
const THROTTLE_MAP_PRUNE: usize = 512;

/// 原始窗口事件（hwnd 为事件目标顶层/子窗口句柄，销毁事件到达时窗口可能已无效）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowEventRaw {
    /// 窗口创建。
    Created(isize),
    /// 窗口销毁。
    Destroyed(isize),
    /// 窗口移动/尺寸变化（50 ms 节流）。
    Moved(isize),
    /// 标题变化。
    Title(isize),
    /// 弹出菜单出现（hwnd 为 `#32768` 菜单窗口）。
    MenuStart(isize),
    /// 弹出菜单消失。
    MenuEnd(isize),
}

/// 事件监听句柄；Drop 时向监听线程投递 `WM_QUIT` 并等待其退出（反装 hook）。
pub struct WatcherHandle {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 启动事件监听线程，返回句柄与事件接收端。
///
/// 句柄被 Drop 前事件持续推送；接收端不再消费时通道按需缓存（unbounded），
/// 由上层（rd-agent）控制消费节奏。
pub fn spawn_event_watcher() -> Result<(WatcherHandle, Receiver<WindowEventRaw>)> {
    let (tx, rx) = unbounded::<WindowEventRaw>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();

    let thread = std::thread::Builder::new()
        .name("rd-winevent".into())
        .spawn(move || unsafe { event_thread(tx, ready_tx) })
        .context("启动事件监听线程失败")?;

    // 等监听线程装好 hook 并回报线程 id；失败（hook 安装失败）透传错误。
    let thread_id = ready_rx
        .recv()
        .context("事件监听线程提前退出")?
        .context("SetWinEventHook 失败")?;

    Ok((
        WatcherHandle {
            thread_id,
            thread: Some(thread),
        },
        rx,
    ))
}

/// 监听线程主体：装 hook → 消息循环（事件经 GetMessage 派发到本线程回调）→
/// 收到 `WM_QUIT` 后反装 hook 退出。
unsafe fn event_thread(tx: Sender<WindowEventRaw>, ready: mpsc::Sender<Result<u32>>) {
    // 回调通过 thread_local 取 sender；先装 sender 再装 hook，保证首事件不丢
    // （事件经消息循环派发，GetMessage 前不会有回调）。
    EVENT_TX.with(|cell| *cell.borrow_mut() = Some(tx));

    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_MENUPOPUPSTART,
            EVENT_OBJECT_NAMECHANGE,
            None,
            Some(winevent_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.is_invalid() {
        EVENT_TX.with(|cell| *cell.borrow_mut() = None);
        let _ = ready.send(Err(anyhow::anyhow!(
            "SetWinEventHook 返回空句柄（范围 {EVENT_SYSTEM_MENUPOPUPSTART:#x}..{EVENT_OBJECT_NAMECHANGE:#x}）"
        )));
        return;
    }
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    let mut msg = MSG::default();
    loop {
        // 返回 0 = WM_QUIT；-1 = 出错。两者都结束循环。
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
    }

    let unhooked = unsafe { UnhookWinEvent(hook) };
    if !unhooked.as_bool() {
        tracing::debug!("UnhookWinEvent 失败（线程即将退出，句柄随线程回收）");
    }
    EVENT_TX.with(|cell| *cell.borrow_mut() = None);
}

thread_local! {
    /// 当前监听线程的输出通道（回调与监听线程同线程，OUTOFCONTEXT 保证）。
    static EVENT_TX: RefCell<Option<Sender<WindowEventRaw>>> = const { RefCell::new(None) };
    /// LOCATIONCHANGE 按窗口节流的时间表。
    static MOVE_THROTTLE_MAP: RefCell<HashMap<isize, Instant>> = RefCell::new(HashMap::new());
}

/// WinEvent 回调：按事件类型映射为 `WindowEventRaw` 并投递。
unsafe extern "system" fn winevent_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time_ms: u32,
) {
    if hwnd.0.is_null() {
        return;
    }
    let hwnd = hwnd.0 as isize;

    let is_window_object = id_object == OBJID_WINDOW.0;
    // 弹出菜单（#32768）以 OBJID_CLIENT 上报；同时兼容 OBJID_WINDOW。
    let is_menu_object = event == EVENT_SYSTEM_MENUPOPUPSTART || event == EVENT_SYSTEM_MENUPOPUPEND;
    let object_ok = if is_menu_object {
        is_window_object || id_object == OBJID_CLIENT.0
    } else {
        is_window_object
    };
    if !object_ok {
        return;
    }

    EVENT_TX.with(|cell| {
        let slot = cell.borrow();
        let Some(tx) = slot.as_ref() else {
            return;
        };
        let ev = match event {
            EVENT_OBJECT_CREATE => WindowEventRaw::Created(hwnd),
            EVENT_OBJECT_DESTROY => WindowEventRaw::Destroyed(hwnd),
            EVENT_OBJECT_NAMECHANGE => WindowEventRaw::Title(hwnd),
            EVENT_OBJECT_LOCATIONCHANGE => {
                if !throttle_pass(hwnd) {
                    return;
                }
                WindowEventRaw::Moved(hwnd)
            }
            EVENT_SYSTEM_MENUPOPUPSTART => WindowEventRaw::MenuStart(hwnd),
            EVENT_SYSTEM_MENUPOPUPEND => WindowEventRaw::MenuEnd(hwnd),
            _ => return,
        };
        let _ = tx.send(ev);
    });
}

/// `LOCATIONCHANGE` 节流：同窗口 50 ms 内的后续事件丢弃。
fn throttle_pass(hwnd: isize) -> bool {
    MOVE_THROTTLE_MAP.with(|cell| {
        let mut map = cell.borrow_mut();
        let now = Instant::now();
        if map.len() > THROTTLE_MAP_PRUNE {
            map.retain(|_, t| now.duration_since(*t) < MOVE_THROTTLE);
        }
        match map.get(&hwnd) {
            Some(t) if now.duration_since(*t) < MOVE_THROTTLE => false,
            _ => {
                map.insert(hwnd, now);
                true
            }
        }
    })
}
