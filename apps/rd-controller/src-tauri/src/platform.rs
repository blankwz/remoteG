//! 跨平台路径与配置解析（宪法 #4：Windows / macOS / iOS / Android 解耦）。
//!
//! - Windows 融合版路径与历史 `rd_agent::default_config_dir()/app` 100% 一致，零迁移；
//! - macOS / iOS / Android / Linux 脱离 `rd-agent` 依赖，使用各自平台标准配置目录。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::OnceLock;

static CUSTOM_CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 在 Tauri `setup()` 中注入平台沙盒配置目录（Android / iOS 必须走系统沙盒路径）。
#[cfg_attr(not(any(target_os = "android", target_os = "ios")), allow(dead_code))]
pub fn init_config_dir(dir: PathBuf) {
    let _ = CUSTOM_CONFIG_DIR.set(dir);
}

/// 应用融合配置目录（Windows 为 `%LOCALAPPDATA%\rd\app` 或 `%APPDATA%\rd\app`，与 `rd-agent` 保持一致）。
pub fn app_config_dir() -> PathBuf {
    if let Some(dir) = CUSTOM_CONFIG_DIR.get() {
        return dir.clone();
    }
    #[cfg(windows)]
    {
        rd_agent::default_config_dir().join("app")
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("rd")
                .join("app");
        }
        std::env::temp_dir().join("rd-app")
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("rd").join("app");
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return PathBuf::from(home).join(".config").join("rd").join("app");
        }
        std::env::temp_dir().join("rd-app")
    }
}

/// 日志目录（`<app_config_dir>/logs`）。
pub fn app_log_dir() -> PathBuf {
    app_config_dir().join("logs")
}

/// 解析 EasyTier 端口转发规则 `"对端虚拟IP:端口@本地端口"`（例如 `"10.126.126.3:21118@21130"`）。
/// 独立于 `rd-agent` 的纯逻辑实现，确保 macOS / iOS / Android 控制端也能校验并消费转发候选。
pub fn parse_forward_rule(rule: &str) -> Option<(SocketAddr, u16)> {
    let trimmed = rule.trim();
    let (remote_str, local_port_str) = trimmed.split_once('@')?;
    let remote: SocketAddr = remote_str.trim().parse().ok()?;
    let local_port: u16 = local_port_str.trim().parse().ok()?;
    if local_port == 0 {
        return None;
    }
    Some((remote, local_port))
}

/// 将转发规则列表转为本机回环入口地址列表（`127.0.0.1:<local_port>`）。
pub fn forward_bind_addrs(forwards: &[String]) -> Vec<SocketAddr> {
    forwards
        .iter()
        .filter_map(|f| parse_forward_rule(f))
        .map(|(_, local_port)| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port))
        .collect()
}

/// macOS 隐私与安全性权限状态 DTO（非 macOS 恒为 `isMac: false`）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MacPermissionsDto {
    pub is_mac: bool,
    pub screen_recording: bool,
    pub accessibility: bool,
}

/// 查询当前系统的 macOS 权限状态（屏幕录制 + 辅助功能）。
#[tauri::command]
pub fn get_mac_permissions() -> MacPermissionsDto {
    #[cfg(target_os = "macos")]
    {
        MacPermissionsDto {
            is_mac: true,
            screen_recording: rd_mac::permissions::has_screen_recording(),
            accessibility: rd_mac::permissions::has_accessibility(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        MacPermissionsDto {
            is_mac: false,
            screen_recording: true,
            accessibility: true,
        }
    }
}

/// 一键打开 macOS《系统设置 → 隐私与安全性》对应面板（"screen" / "accessibility" / "security"）。
#[tauri::command]
pub fn open_mac_privacy_settings(pane: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let url = match pane.as_str() {
            "screen" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            "accessibility" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            _ => "x-apple.systempreferences:com.apple.preference.security?Privacy",
        };
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统设置失败：{e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pane;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_rule_valid_and_invalid() {
        let parsed = parse_forward_rule("10.126.126.3:21118@21130").unwrap();
        assert_eq!(parsed.1, 21130);
        assert!(parse_forward_rule("坏规则").is_none());
        assert!(parse_forward_rule("10.126.126.3:21118@0").is_none());
    }

    #[test]
    fn forward_addrs_collects_localhost_ports() {
        let addrs = forward_bind_addrs(&[
            "10.126.126.3:21118@21130".into(),
            "bad".into(),
            "10.126.126.4:21118@21131".into(),
        ]);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], "127.0.0.1:21130".parse::<SocketAddr>().unwrap());
        assert_eq!(addrs[1], "127.0.0.1:21131".parse::<SocketAddr>().unwrap());
    }
}
