//! 会话保活（D-013 包2）：有受信任连接期间阻止系统进入屏保/息屏/睡眠——
//! 否则控制中的会话会因系统节能中途失控（E5 评审：会话边界失控源之首）。
//!
//! `SetThreadExecutionState` 的 `ES_CONTINUOUS` 标志是 per-thread 的，且随
//! 线程退出失效；tokio worker 线程会被调度到不同连接，故用专用驻留线程
//! 持有标志，经 mpsc 通道聚合 acquire/release 计数。

#[cfg(windows)]
pub use imp::{acquire, release};

#[cfg(windows)]
mod imp {
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::OnceLock;

    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    };

    /// 保活命令通道；`true` = 一个连接开始，`false` = 一个连接结束。
    static TX: OnceLock<Sender<bool>> = OnceLock::new();

    /// 登记一个活跃连接（受信任会话建立时调用）。
    pub fn acquire() {
        let _ = TX.get_or_init(spawn).send(true);
    }

    /// 注销一个活跃连接（断开清理时调用）。
    pub fn release() {
        if let Some(tx) = TX.get() {
            let _ = tx.send(false);
        }
    }

    /// 启动驻留线程：聚合计数，>0 时持 ES_*_REQUIRED，归零即释放。
    fn spawn() -> Sender<bool> {
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let builder = std::thread::Builder::new().name("rd-keepalive".into());
        let worker = builder.spawn(move || pump(rx));
        if worker.is_err() {
            tracing::warn!("保活线程启动失败，会话期间不阻止系统睡眠");
        }
        tx
    }

    fn pump(rx: Receiver<bool>) {
        let mut holders: usize = 0;
        for msg in rx {
            holders = if msg {
                holders.saturating_add(1)
            } else {
                holders.saturating_sub(1)
            };
            // SAFETY:无指针参数;返回值为实际生效标志(设置失败时不含所需位)
            let applied = unsafe {
                SetThreadExecutionState(if holders > 0 {
                    ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED
                } else {
                    ES_CONTINUOUS
                })
            };
            if holders > 0 && !applied.contains(ES_SYSTEM_REQUIRED) {
                tracing::debug!(holders, "SetThreadExecutionState 设置失败");
            }
            tracing::debug!(holders, "会话保活状态已更新");
        }
    }
}

#[cfg(not(windows))]
pub fn acquire() {}

#[cfg(not(windows))]
pub fn release() {}
