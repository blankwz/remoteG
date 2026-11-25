//! macOS 权限检测：屏幕录制（Screen Recording）与辅助功能（Accessibility）。

/// 屏幕录制权限是否已授予（非 macOS 恒返 true）。
pub fn has_screen_recording() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        crate::ffi::CGPreflightScreenCaptureAccess()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// 辅助功能（键鼠注入）权限是否已授予（非 macOS 恒返 true）。
pub fn has_accessibility() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        crate::ffi::AXIsProcessTrusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// 启动期一次性日志提示缺失权限。
pub fn log_permission_status() {
    #[cfg(target_os = "macos")]
    {
        if !has_screen_recording() {
            tracing::warn!(
                "macOS 尚未授予「屏幕录制」权限：请前往 系统设置 → 隐私与安全性 → 屏幕录制 勾选本应用后重启"
            );
        }
        if !has_accessibility() {
            tracing::warn!(
                "macOS 尚未授予「辅助功能」权限：请前往 系统设置 → 隐私与安全性 → 辅助功能 勾选本应用，否则远程键鼠注入将无效"
            );
        }
    }
}
