//! 配置:`<config_dir>\agent.toml`(默认 `%LOCALAPPDATA%\rd\agent.toml`,T-015)。
//!
//! 文件不存在时使用默认值(不自动写回,避免覆盖用户手工修改);
//! 缺失字段逐项回落默认值(`#[serde(default)]`)。

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// 默认监听端口(05_api/协议契约.md §1,标注为可配置)。
pub const DEFAULT_LISTEN_PORT: u16 = 21118;

/// Agent 配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 监听端口(`listen_addrs` 自动发现时使用;显式 `--listen` 地址含端口时不适用)。
    pub listen_port: u16,
    /// 显式监听地址(IP 列表,端口取 `listen_port`);空 = 自动(见 server.rs)。
    pub listen_addrs: Vec<IpAddr>,
    /// 是否额外绑定私网地址(默认 true:同 WiFi 直连是小白场景的主要路径,
    /// 安全由配对码+信任列表兜底,D-013 评审确认;设 false 收窄为仅 Tailscale)。
    pub allow_lan: bool,
    /// 受信任设备接入时在被控端弹确认框。**默认关**(2026-10-23 用户反馈:
    /// 配对输 6 位码时人就在场,已是完整授权,每次连接再问是重复打扰;
    /// 且后台线程 MessageBox 受 Windows 前台锁限制只能闪任务栏不前置)。
    /// 需要强确认的场景在 agent.toml 设 true;信任撤销入口在托盘「信任设备」。
    pub confirm_connection: bool,
    /// 终端功能开关(11_security:信任设备才可开,此处为总开关)。
    pub terminal_enabled: bool,
    /// 语义化精修(T-016 L2 渲染)开关。
    pub refine_enabled: bool,
    /// 默认最大帧率。120（2026-10-23 用户主诉流畅度；WGC 跟随合成器刷新,
    /// 高刷屏可超 60;h264_mf 硬编 1080p 120fps≈8ms/帧可扛;运动带宽×2 由
    /// AIMD 上限 50M 兜底,低带宽自动收敛）。
    pub max_fps: u32,
    /// 初始视频码率(kbps)。
    pub start_kbps: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_port: DEFAULT_LISTEN_PORT,
            listen_addrs: Vec::new(),
            allow_lan: true,
            confirm_connection: false,
            terminal_enabled: true,
            refine_enabled: true,
            max_fps: 120,
            start_kbps: 8000,
        }
    }
}

/// 配置文件路径:`<dir>/agent.toml`。
pub fn config_file(dir: &Path) -> PathBuf {
    dir.join("agent.toml")
}

impl Config {
    /// 从 `dir/agent.toml` 加载;文件不存在时返回默认配置。
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = config_file(dir);
        if !path.exists() {
            tracing::info!(path = %path.display(), "未找到配置文件,使用默认配置");
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取配置失败:{}", path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).with_context(|| format!("解析配置失败:{}", path.display()))?;
        Ok(cfg)
    }
}
