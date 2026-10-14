//! 托盘:图标菜单(显示配对码/作废/在线设备/退出)——T-018 实现。
//!
//! 本任务(T-015)仅提供启动桩:主线程在此驻留(Ctrl+C 由 OS 默认处理终止
//! 进程)。`tray-icon` 依赖已在 Cargo.toml 就位,T-018 填充消息泵与菜单回调
//! (回调经由 `AgentState`:配对码 = `state.pairing.lock().new_code()`,
//! 作废 = `expire_code()`,在线设备 = `state.peers`,暂停输入 = `state.paused`)。
//! 本桩不打印、不落日志配对码。

use std::sync::Arc;
use std::time::Duration;

use crate::session::AgentState;

/// 托盘主循环(T-018 实现;当前桩永不返回)。
pub fn run(_state: Arc<AgentState>) -> ! {
    tracing::info!("托盘未实现(T-018),进程驻留待命;Ctrl+C 退出");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
