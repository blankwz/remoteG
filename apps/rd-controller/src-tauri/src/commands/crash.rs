//! 崩溃报告（T-040）：设置页「崩溃报告」节的数据源——列出/清空本地 dump、
//! 组装预填 Issue 链接。dump 由两端 panic hook 写入（rd-crash crate），
//! 只含元数据；上报走用户确认（前端 openUrl 打开，提交与否在用户手里）。

use crate::conn::ConnManager;
use tauri::State;

/// dump 目录（与 lib.rs panic hook 写入处同源：融合日志目录下 crash/）。
fn crash_dir() -> std::path::PathBuf {
    rd_crash::crash_dir(&rd_agent::default_config_dir().join("app").join("logs"))
}

/// 列出崩溃记录（时间倒序；只含元数据）。
#[tauri::command]
pub fn crash_dumps(_state: State<'_, ConnManager>) -> Vec<rd_crash::DumpEntry> {
    rd_crash::list_dumps(&crash_dir())
}

/// 清空崩溃记录，返回删除条数。
#[tauri::command]
pub fn crash_clear(_state: State<'_, ConnManager>) -> usize {
    let n = rd_crash::clear_dumps(&crash_dir());
    tracing::info!(n, "用户清空崩溃记录");
    n
}

/// 组装「在浏览器上报」的预填 Issue 链接（用户可在浏览器里检查后再提交）。
/// body 上限 1500 字符（中文 urlencode 膨胀 ~9 倍后仍在浏览器安全范围）。
#[tauri::command]
pub fn crash_report_url(_state: State<'_, ConnManager>) -> String {
    rd_crash::issue_new_url(&rd_crash::list_dumps(&crash_dir()), 1500)
}
