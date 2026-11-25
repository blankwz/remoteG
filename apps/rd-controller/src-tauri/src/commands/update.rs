//! 应用更新（T-038）：updater 插件做检查/下载/验签/落盘，重启由本命令收尾
//! （passive 安装模式装完不自动退出，前端在「安装完成」后显式调这里）。

/// 重启应用（更新安装完成后）。桌面端 `AppHandle::restart` 不返回，进程被替换；移动端为空操作。
#[tauri::command]
pub fn restart_app(_app: tauri::AppHandle) {
    tracing::info!("用户确认重启以完成更新");
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    _app.restart();
}
