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
    /// 原生中继地址(T-032,如 "1.2.3.4:21118");配置后额外经中继接受连接,
    /// 直连路径不受影响。环境变量 RD_RELAY 优先。跨网打洞失败的兜底通道。
    pub relay_addr: Option<String>,
    /// EasyTier 组网(可选,默认关闭):agent 托管 easytier-core 进程,
    /// 加入 mesh 后本机虚拟 IP 作为直连候选上报(随 REG → DIAL ACK 竞速)。
    /// 失败仅降级告警,不影响直连/中继路径。
    #[serde(default)]
    pub easytier: EasyTierAgentConfig,
    /// 虚拟屏(一期):会话期间按用户手机规格建一块虚拟显示器,远控窗口落到
    /// 这块屏上,物理屏保持可用。驱动未装时建屏失败只上报,不影响远控会话。
    #[serde(default)]
    pub virtual_display: VirtualDisplayConfig,
}

/// agent.toml `[virtual_display]` 节(全部字段有默认,旧配置文件零改动兼容)。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VirtualDisplayConfig {
    /// 总开关(默认开;环境变量 `RD_VDISPLAY=0` 可强制关,测试/排障用)。
    pub enabled: bool,
    /// 用户手机物理像素宽(0 = 用标准机 1320×2868)。
    pub phone_width: u32,
    /// 用户手机物理像素高。
    pub phone_height: u32,
    /// 手机逻辑宽度(点);0 = 未知,档位数学按 400 估。
    pub points: u32,
}

impl Default for VirtualDisplayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            phone_width: 0,
            phone_height: 0,
            points: 0,
        }
    }
}

/// agent.toml `[easytier]` 节(全部字段有默认,旧配置文件零改动兼容)。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EasyTierAgentConfig {
    /// 总开关(false = 完全不碰 EasyTier)。
    pub enabled: bool,
    /// easytier-core.exe 路径(rd 不捆绑分发,由用户安装后指向)。
    pub binary_path: String,
    /// easytier-cli 路径(虚拟 IP 自查用);留空 = binary_path 同目录的
    /// easytier-cli(.exe)——发行包两者同目录。
    pub cli_path: String,
    /// 网络名(同网互见;空 = 未配置,enabled 无效)。
    pub network_name: String,
    /// 网络密钥。
    pub network_secret: String,
    /// 引导 peer(官方公共节点或自建中继,如 "tcp://1.2.3.4:11010")。
    pub peer_urls: Vec<String>,
    /// 静态虚拟 IP(如 "10.126.126.2/24");留空 = DHCP。
    pub virtual_ip: String,
    /// 不建虚拟网卡(免管理员,默认 true);TUN 模式需管理员但控制端可直拨本机虚拟 IP。
    pub no_tun: bool,
    /// no_tun 模式的 UDP 转发规则(格式 "对端虚拟IP:端口@本地端口",如
    /// "10.126.126.3:21118@21130"):本机节点在 127.0.0.1:21130 开 UDP 入口,
    /// 经 mesh 转发到对端 rd。被控端通常不配;控制端(融合版)配置后连接
    /// 自动把 127.0.0.1:21130 作为候选。
    pub forwards: Vec<String>,
}

// 手写 Default:no_tun 语义默认必须是 true(免管理员);
// derive(Default) 会给 bool 填 false,而缺 [easytier] 节时正是走 Default 路径。
impl Default for EasyTierAgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            binary_path: String::new(),
            cli_path: String::new(),
            network_name: String::new(),
            network_secret: String::new(),
            peer_urls: Vec::new(),
            virtual_ip: String::new(),
            no_tun: true,
            forwards: Vec::new(),
        }
    }
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
            relay_addr: None,
            easytier: EasyTierAgentConfig::default(),
            virtual_display: VirtualDisplayConfig::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧版 agent.toml（无 [easytier] 节）零改动兼容：默认关闭。
    #[test]
    fn 旧配置_无easytier节_默认关闭() {
        let cfg: Config =
            toml::from_str("listen_port = 21118\nrelay_addr = \"1.2.3.4:21118\"\n").unwrap();
        assert!(!cfg.easytier.enabled);
        assert_eq!(cfg.easytier.network_name, "");
        assert!(cfg.easytier.no_tun); // 默认 no_tun（免管理员）
    }

    /// 旧版 agent.toml（无 [virtual_display] 节）零改动兼容：默认开启。
    #[test]
    fn 旧配置_无virtual_display节_默认开启() {
        let cfg: Config = toml::from_str("listen_port = 21118\n").unwrap();
        assert!(cfg.virtual_display.enabled);
        assert_eq!(cfg.virtual_display.phone_width, 0);
        assert_eq!(cfg.virtual_display.points, 0);
    }

    /// [virtual_display] 节完整解析。
    #[test]
    fn virtual_display节_完整解析() {
        let raw = r#"
[virtual_display]
enabled = false
phone_width = 1080
phone_height = 2400
points = 400
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.virtual_display.enabled);
        assert_eq!(cfg.virtual_display.phone_width, 1080);
        assert_eq!(cfg.virtual_display.phone_height, 2400);
        assert_eq!(cfg.virtual_display.points, 400);
    }

    /// [easytier] 节完整解析。
    #[test]
    fn easytier节_完整解析() {
        let raw = r#"
[easytier]
enabled = true
binary_path = "C:\\tools\\easytier-core.exe"
network_name = "rd-mesh"
network_secret = "s3cret"
peer_urls = ["tcp://1.2.3.4:11010"]
virtual_ip = "10.126.126.2/24"
no_tun = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.easytier.enabled);
        assert_eq!(cfg.easytier.network_name, "rd-mesh");
        assert_eq!(cfg.easytier.network_secret, "s3cret");
        assert_eq!(cfg.easytier.peer_urls, vec!["tcp://1.2.3.4:11010"]);
        assert_eq!(cfg.easytier.virtual_ip, "10.126.126.2/24");
        assert!(!cfg.easytier.no_tun);
    }
}
