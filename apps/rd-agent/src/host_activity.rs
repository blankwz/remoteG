//! 被控机「本机优先」输入活动检测（D-013 包5，用户手测反馈 2）。
//!
//! 低级钩子（WH_MOUSE_LL/WH_KEYBOARD_LL）记录**真实**（非注入）键鼠输入的
//! 最后时刻：窗口期内（2s）判定「本机有人正在使用」，控制端注入挂起——
//! 避免两边抢鼠标打架（向日葵式本机优先；SendInput 注入的输入带
//! INJECTED 标志，不会自我触发）。
//!
//! 钩子必须由装钩线程持续泵消息；本模块线程常驻到进程退出。

#[cfg(windows)]
pub use imp::{busy, init};

#[cfg(not(windows))]
pub fn init() {}

#[cfg(not(windows))]
pub fn busy() -> bool {
    false
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
        HOOKPROC, KBDLLHOOKSTRUCT, KBDLLHOOKSTRUCT_FLAGS, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
        MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    /// 本机优先窗口期：最后真实输入 2s 内视为「有人在用」。
    const BUSY_WINDOW_MS: u64 = 2000;

    /// 最后一次真实（非注入）键鼠输入的 Unix 毫秒。
    static LAST_REAL_INPUT_MS: AtomicU64 = AtomicU64::new(0);

    /// 启动钩子线程（进程启动调用一次；失败仅记录，功能降级为「永不挂起」）。
    pub fn init() {
        let builder = std::thread::Builder::new().name("rd-host-activity".into());
        if builder.spawn(pump).is_err() {
            tracing::warn!("本机输入活动钩子线程启动失败,本机优先功能停用");
        }
    }

    /// 现在是否处于「本机有人正在使用」窗口期。
    pub fn busy() -> bool {
        let last = LAST_REAL_INPUT_MS.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now.saturating_sub(last) < BUSY_WINDOW_MS
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// 装钩 + 消息泵（常驻）。
    fn pump() {
        unsafe {
            let hmod = GetModuleHandleW(None).unwrap_or_default();
            let hook_proc: HOOKPROC = Some(hook_proc);
            let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, hook_proc, Some(hmod.into()), 0);
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, hook_proc, Some(hmod.into()), 0);
            if keyboard.is_err() || mouse.is_err() {
                tracing::warn!("低级钩子安装失败(错误略),本机优先功能停用");
                return;
            }
            tracing::info!("本机输入活动钩子就绪(本机优先窗口 {BUSY_WINDOW_MS}ms)");
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// 低级钩子回调：非注入输入刷新时间戳；永远放行（不吞事件）。
    unsafe extern "system" fn hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
        unsafe {
            if n_code >= 0 {
                // 键盘消息 0x100..0x105 < 鼠标 0x200..;w_param 即消息 ID
                let injected = if w_param.0 as u32 <= 0x105 {
                    let kb = &*(l_param.0 as *const KBDLLHOOKSTRUCT);
                    (kb.flags & KBDLLHOOKSTRUCT_FLAGS(LLKHF_INJECTED.0))
                        == KBDLLHOOKSTRUCT_FLAGS(LLKHF_INJECTED.0)
                } else {
                    let ms = &*(l_param.0 as *const MSLLHOOKSTRUCT);
                    (ms.flags & LLMHF_INJECTED) != 0
                };
                if !injected {
                    LAST_REAL_INPUT_MS.store(now_ms(), Ordering::Relaxed);
                }
            }
            CallNextHookEx(None, n_code, w_param, l_param)
        }
    }
}
