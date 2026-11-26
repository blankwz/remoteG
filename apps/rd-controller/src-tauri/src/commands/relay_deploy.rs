//! SSH 一键上传并部署自建中转服务器（宪法 #3）。
//!
//! 用户在控制端 UI 填写 SSH 主机/端口/用户名（可选私钥路径）后，本模块通过
//! 本机 OpenSSH（Windows 10/11、macOS、Linux 均自带 `ssh`）执行：
//! 1. 智能归一化输入（支持 `host:port`、`user@host:port`、`[ipv6]:port`、带引号私钥路径）并严格防注入；
//! 2. 自动处理 Windows 私钥文件 NTFS 继承权限过宽（`bad permissions`）问题，生成受限临时副本并在结束后即刻销毁；
//! 3. 连通性探测（`BatchMode=yes`），若遇到本机 TUN/代理虚拟网卡拦截 22 端口导致握手被重置，自动枚举物理网卡 IPv4 直连重试；
//! 4. 优先通过 SSH 流式原子上传本地预编译 Linux `rd-relay` 二进制（避免 `scp` 缺 sftp 或覆盖运行中进程报 `Text file busy`），无预编译产物时自动上传内嵌 `rd-relay` 源码在远端编译；
//! 5. 远端配置 `systemd` 守护、放行系统防火墙端口、校验服务状态，并从本机实测 UDP 中继端口连通性（提示云安全组放行）。

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const EMBEDDED_RELAY_MAIN_RS: &str = include_str!("../../../../../crates/rd-relay/src/main.rs");
const EMBEDDED_RELAY_LIB_RS: &str = include_str!("../../../../../crates/rd-relay/src/lib.rs");
const EMBEDDED_RELAY_CARGO_TOML: &str = r#"[package]
name = "rd-relay"
version = "0.4.6"
edition = "2021"

[dependencies]
anyhow = "1"
ed25519-dalek = { version = "2", features = ["pkcs8"] }
sha2 = "0.10"
tokio = { version = "1", features = ["net", "rt-multi-thread", "macros", "time"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
"#;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshDeployRequest {
    /// 服务器 IP 或域名（支持 `1.2.3.4`、`1.2.3.4:22`、`root@1.2.3.4:22`）。
    pub host: String,
    /// SSH 端口（默认 22）。
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// SSH 用户名（默认 root）。
    #[serde(default = "default_ssh_user")]
    pub user: String,
    /// 可选私钥文件路径（留空使用本机默认 ~/.ssh 密钥或 ssh-agent）。
    #[serde(default)]
    pub key_path: String,
    /// 中继监听端口（默认 21118）。
    #[serde(default = "default_relay_port")]
    pub relay_port: u16,
    /// 部署模式："udp"（原生最低延迟）或 "docker"（Docker + Caddy 自动 HTTPS）。
    #[serde(default = "default_mode")]
    pub mode: String,
    /// docker 模式下的域名（仅 mode="docker" 时使用）。
    #[serde(default)]
    pub domain: String,
}

fn default_ssh_port() -> u16 {
    22
}
fn default_ssh_user() -> String {
    "root".into()
}
fn default_relay_port() -> u16 {
    21118
}
fn default_mode() -> String {
    "udp".into()
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshDeployResult {
    pub ok: bool,
    pub relay_addr: String,
    pub summary: String,
    pub log: String,
}

/// Windows GUI 模式下隐藏子进程控制台黑框闪烁（宪法 #4：按 `cfg(windows)` 隔离）。
fn configure_hidden_cmd(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

/// 严格校验纯主机名/IP（不含端口）：仅允许字母、数字、点、横杠，或合法 IPv6 地址。
pub(crate) fn validate_host(host: &str) -> Result<String, String> {
    let h = host.trim();
    if h.is_empty() || h.len() > 253 {
        return Err("服务器地址不能为空或超过 253 字符".into());
    }
    if h.starts_with('-') {
        return Err("服务器地址不能以 '-' 开头".into());
    }
    if h.contains(':') {
        // 含冒号时必须是合法 IPv6 地址（`host:port` 应先经 `parse_ssh_endpoint` 拆分）
        let parsed: IpAddr = h.parse().map_err(|_| {
            "服务器地址格式错误（如含端口请使用 IP:端口，如 43.135.129.15:22）".to_string()
        })?;
        return Ok(parsed.to_string());
    }
    if !h
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return Err("服务器地址包含非法字符（仅允许 IP 或域名）".into());
    }
    Ok(h.to_string())
}

/// 严格校验用户名：仅允许字母、数字、点、下划线、横杠。
pub(crate) fn validate_user(user: &str) -> Result<String, String> {
    let u = user.trim();
    if u.is_empty() || u.len() > 64 {
        return Err("SSH 用户名不能为空或超过 64 字符".into());
    }
    if u.starts_with('-') {
        return Err("SSH 用户名不能以 '-' 开头".into());
    }
    if !u
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("SSH 用户名包含非法字符".into());
    }
    Ok(u.to_string())
}

/// 智能拆分用户在主机框输入的 `[user@]host[:port]` 或 `[ipv6]:port`。
pub(crate) fn parse_ssh_endpoint(
    raw_host: &str,
    fallback_port: u16,
    fallback_user: &str,
) -> Result<(String, u16, String), String> {
    let mut rest = raw_host.trim();
    let mut user_str = if fallback_user.trim().is_empty() {
        "root"
    } else {
        fallback_user.trim()
    };
    let mut port = if fallback_port == 0 {
        22
    } else {
        fallback_port
    };

    if let Some((u, h)) = rest.rsplit_once('@') {
        if !u.trim().is_empty() {
            user_str = u.trim();
        }
        rest = h.trim();
    }

    let host_part = if let Some(after_bracket) = rest.strip_prefix('[') {
        let (ipv6, tail) = after_bracket
            .split_once(']')
            .ok_or_else(|| "IPv6 地址缺少右括号 ']'".to_string())?;
        if let Some(port_str) = tail.strip_prefix(':') {
            port = port_str
                .parse::<u16>()
                .ok()
                .filter(|&p| p > 0)
                .ok_or_else(|| format!("SSH 端口无效：{port_str}"))?;
        } else if !tail.is_empty() {
            return Err("IPv6 地址后包含多余字符".into());
        }
        ipv6
    } else if rest.matches(':').count() == 1 {
        let (h, p) = rest.split_once(':').expect("single colon");
        port = p
            .parse::<u16>()
            .ok()
            .filter(|&v| v > 0)
            .ok_or_else(|| format!("SSH 端口无效：{p}"))?;
        h
    } else {
        rest
    };

    let valid_host = validate_host(host_part)?;
    let valid_user = validate_user(user_str)?;
    Ok((valid_host, port, valid_user))
}

/// 归一化私钥路径：去首尾引号、展开 `~/`。
fn normalize_key_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rel) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        if let Some(h) = home {
            return Some(h.join(rel));
        }
    }
    Some(PathBuf::from(trimmed))
}

/// 临时受限私钥副本（解决 Windows 非 `~/.ssh` 路径继承 `Users`/`Authenticated Users` 权限被 OpenSSH 拒用问题）。
struct PreparedSshKey {
    effective_path: Option<PathBuf>,
    temp_copy: Option<PathBuf>,
}

impl Drop for PreparedSshKey {
    fn drop(&mut self) {
        if let Some(tmp) = self.temp_copy.take() {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

async fn prepare_ssh_key(raw_key_path: &str, log: &mut String) -> Result<PreparedSshKey, String> {
    let Some(orig_path) = normalize_key_path(raw_key_path) else {
        return Ok(PreparedSshKey {
            effective_path: None,
            temp_copy: None,
        });
    };

    if !orig_path.is_file() {
        return Err(format!(
            "未找到 SSH 私钥文件：{}（请确认路径拼写正确且为私钥文件）",
            orig_path.display()
        ));
    }

    let raw_bytes = std::fs::read(&orig_path)
        .map_err(|e| format!("读取私钥文件 {} 失败：{e}", orig_path.display()))?;
    // 若为 PEM/OpenSSH 文本私钥且被记事本存成 CRLF，统一归一化为 LF，二进制文件保持原样
    let normalized_bytes = if raw_bytes.starts_with(b"-----BEGIN ") {
        let s = String::from_utf8_lossy(&raw_bytes).replace("\r\n", "\n");
        s.into_bytes()
    } else {
        raw_bytes
    };

    let tmp_path = std::env::temp_dir().join(format!("rd-ssh-key-{}.pem", uuid::Uuid::new_v4()));
    std::fs::write(&tmp_path, &normalized_bytes)
        .map_err(|e| format!("创建受限临时私钥副本失败：{e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    }

    #[cfg(windows)]
    {
        let user_grant = std::env::var("USERNAME")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .map(|u| format!("{u}:(R)"));
        let mut cmd = Command::new("icacls");
        configure_hidden_cmd(&mut cmd);
        cmd.arg(&tmp_path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg("*S-1-5-18:(F)")
            .arg("/grant:r")
            .arg("*S-1-5-32-544:(F)");
        if let Some(ug) = &user_grant {
            cmd.arg("/grant:r").arg(ug);
        }
        match cmd.output().await {
            Ok(out) if out.status.success() => {
                log.push_str(&format!(
                    "[✓] 已就绪私钥文件 {}（已自动隔离 Windows ACL 权限检查）\n",
                    orig_path.display()
                ));
            }
            Ok(out) => {
                log.push_str(&format!(
                    "[提示] icacls 收紧临时私钥权限返回非零：{}，回退使用原路径\n",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                let _ = std::fs::remove_file(&tmp_path);
                return Ok(PreparedSshKey {
                    effective_path: Some(orig_path),
                    temp_copy: None,
                });
            }
            Err(_) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Ok(PreparedSshKey {
                    effective_path: Some(orig_path),
                    temp_copy: None,
                });
            }
        }
    }

    Ok(PreparedSshKey {
        effective_path: Some(tmp_path.clone()),
        temp_copy: Some(tmp_path),
    })
}

/// 枚举本机物理网卡 IPv4 地址（排除回环、Clash/Mihomo TUN `198.18.0.0/15`、链路本地、WSL/虚拟网卡）。
/// 当本机开启 TUN 全局代理且机场节点封禁 22 端口导致 `kex_exchange_identification: Connection closed` 时，
/// 通过 `-o BindAddress=<物理网卡IP>` 让 SSH 走物理网卡直连云服务器。
fn candidate_physical_ipv4s(target_host: &str) -> Vec<String> {
    // 若目标本身是回环或 Tailscale 100.x，不强绑物理局域网口
    if let Ok(IpAddr::V4(dst)) = target_host.parse::<IpAddr>() {
        let oct = dst.octets();
        if dst.is_loopback() || (oct[0] == 100 && (64..=127).contains(&oct[1])) {
            return Vec::new();
        }
    }
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };

    let mut primary = Vec::new();
    let mut secondary = Vec::new();
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        let IpAddr::V4(ip) = iface.ip() else {
            continue;
        };
        let oct = ip.octets();
        // 排除链路本地 169.254.0.0/16、Clash/Mihomo TUN 198.18.0.0/15、CGNAT/Tailscale 100.64.0.0/10
        if (oct[0] == 169 && oct[1] == 254)
            || (oct[0] == 198 && (oct[1] == 18 || oct[1] == 19))
            || (oct[0] == 100 && (64..=127).contains(&oct[1]))
        {
            continue;
        }
        let name_lc = iface.name.to_ascii_lowercase();
        let is_virtual = [
            "tun",
            "tap",
            "meta",
            "ninja",
            "clash",
            "tailscale",
            "wsl",
            "vethernet",
            "hyper-v",
            "vmware",
            "vbox",
            "virtual",
            "docker",
            "loopback",
        ]
        .iter()
        .any(|kw| name_lc.contains(kw));
        if is_virtual {
            continue;
        }
        let s = ip.to_string();
        // 常见家庭/办公路由网段 192.168.x.x / 10.x.x.x 优先
        if oct[0] == 192 && oct[1] == 168 || oct[0] == 10 {
            if !primary.contains(&s) {
                primary.push(s);
            }
        } else if !secondary.contains(&s) {
            secondary.push(s);
        }
    }
    primary.extend(secondary);
    primary
}

fn ssh_base_args(port: u16, key_path: Option<&Path>, bind_addr: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "PubkeyAcceptedAlgorithms=+ssh-rsa".into(),
        "-p".into(),
        port.to_string(),
    ];
    if let Some(bind) = bind_addr {
        args.push("-o".into());
        args.push(format!("BindAddress={bind}"));
    }
    if let Some(k) = key_path {
        args.push("-i".into());
        args.push(k.to_string_lossy().into_owned());
    }
    args
}

fn diagnose_ssh_error(stderr: &str) -> String {
    let lc = stderr.to_ascii_lowercase();
    if lc.contains("bad permissions") || lc.contains("unprotected private key file") {
        "SSH 私钥文件权限过宽被系统 OpenSSH 拒绝，请检查私钥文件访问权限。".into()
    } else if lc.contains("permission denied") {
        "SSH 认证被服务器拒绝（Permission denied）：请确认用户名正确，且该私钥对应的公钥已写入服务器 ~/.ssh/authorized_keys。".into()
    } else if lc.contains("kex_exchange_identification") || lc.contains("connection reset") {
        "SSH 握手阶段连接被中断（常见于本机代理/TUN 拦截了 22 端口或服务器 sshd 限制连接频率）。"
            .into()
    } else if lc.contains("timed out") || lc.contains("connection refused") {
        "无法连通服务器 SSH 端口：请确认服务器 IP、SSH 端口（默认 22）及云安全组已放行。".into()
    } else if lc.contains("invalid format") || lc.contains("error in libcrypto") {
        "SSH 私钥格式无法识别：请确认使用的是标准 PEM / OpenSSH 私钥（而非 PuTTY .ppk 格式）。"
            .into()
    } else {
        "SSH 连接失败：请确认已配置免密密钥（ssh-copy-id）或指定正确的私钥路径、IP 与端口。".into()
    }
}

/// 生成远端 Linux 自动安装脚本（无交互、幂等、systemd 守护 + 防火墙放行；自动编译兜底）。
fn build_remote_script(relay_port: u16, mode: &str, domain: &str, embed_source: bool) -> String {
    let script = if mode == "docker" {
        format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
echo "[1/4] 检查 Docker 环境..."
if ! command -v docker >/dev/null 2>&1; then
  curl -fsSL https://get.docker.com | sh
  systemctl enable --now docker || true
fi
mkdir -p /opt/rd-relay-docker
cd /opt/rd-relay-docker
cat > Caddyfile <<'EOF'
{domain} {{
    reverse_proxy relay:8080
}}
EOF
cat > docker-compose.yml <<'EOF'
services:
  relay:
    image: node:20-alpine
    restart: unless-stopped
    working_dir: /app
    command: ["node", "-e", "const http=require('http');http.createServer((q,s)=>{{if(q.url==='/healthz'){{s.end('ok');return;}}s.statusCode=426;s.end('rd-relay');}}).listen(8080)"]
  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
volumes:
  caddy_data:
EOF
echo "[2/4] 启动容器..."
docker compose up -d
echo "[3/4] 放行 80/443 端口..."
if command -v ufw >/dev/null 2>&1; then ufw allow 80/tcp >/dev/null 2>&1 || true; ufw allow 443/tcp >/dev/null 2>&1 || true; fi
echo "[4/4] 部署完成: https://{domain}"
"#
        )
    } else {
        let source_bootstrap = if embed_source {
            let cargo_toml = EMBEDDED_RELAY_CARGO_TOML.replace("\r\n", "\n");
            let main_rs = EMBEDDED_RELAY_MAIN_RS.replace("\r\n", "\n");
            let lib_rs = EMBEDDED_RELAY_LIB_RS.replace("\r\n", "\n");
            format!(
                r#"mkdir -p /opt/rd-relay/src/src
cat > /opt/rd-relay/src/Cargo.toml <<'RD_CARGO_TOML_EOF'
{cargo_toml}
RD_CARGO_TOML_EOF
cat > /opt/rd-relay/src/src/main.rs <<'RD_MAIN_RS_EOF'
{main_rs}
RD_MAIN_RS_EOF
cat > /opt/rd-relay/src/src/lib.rs <<'RD_LIB_RS_EOF'
{lib_rs}
RD_LIB_RS_EOF
"#
            )
        } else {
            String::new()
        };

        format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
RELAY_PORT={relay_port}
mkdir -p /opt/rd-relay

echo "[1/4] 准备 rd-relay 服务端程序..."
if [ ! -x /opt/rd-relay/rd-relay ]; then
{source_bootstrap}
  if [ -f /opt/rd-relay/src/Cargo.toml ]; then
    echo "  本机未附带预编译 Linux 二进制，正在远端自动编译 rd-relay（首次约需 1~3 分钟）..."
    if ! command -v cc >/dev/null 2>&1; then
      if command -v apt-get >/dev/null 2>&1; then apt-get update -y && apt-get install -y gcc libc6-dev;
      elif command -v dnf >/dev/null 2>&1; then dnf install -y gcc glibc-devel;
      elif command -v yum >/dev/null 2>&1; then yum install -y gcc glibc-devel;
      fi
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v cargo >/dev/null 2>&1; then
      if ! curl -fsSL --connect-timeout 10 https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable; then
        echo "  官方 rustup 连接较慢，自动切换国内镜像安装..."
        export RUSTUP_DIST_SERVER="https://rsproxy.cn"
        export RUSTUP_UPDATE_ROOT="https://rsproxy.cn/rustup"
        curl -fsSL https://rsproxy.cn/rustup-init.sh | sh -s -- -y --profile minimal --default-toolchain stable
      fi
      export PATH="$HOME/.cargo/bin:$PATH"
    fi
    (cd /opt/rd-relay/src && cargo build --release --bin rd-relay)
    cp -f /opt/rd-relay/src/target/release/rd-relay /opt/rd-relay/rd-relay.new
    chmod +x /opt/rd-relay/rd-relay.new
    mv -f /opt/rd-relay/rd-relay.new /opt/rd-relay/rd-relay
  else
    echo "  未找到 rd-relay 二进制或源码"
    exit 2
  fi
fi
chmod +x /opt/rd-relay/rd-relay

echo "[2/4] 写入并启动 systemd 守护服务..."
cat > /etc/systemd/system/rd-relay.service <<EOF
[Unit]
Description=rd Native UDP Relay (E2E Encrypted Datagram Forwarder)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/rd-relay/rd-relay 0.0.0.0:${{RELAY_PORT}}
Restart=always
RestartSec=2
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now rd-relay
systemctl restart rd-relay

echo "[3/4] 放行系统防火墙 UDP ${{RELAY_PORT}}..."
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q active; then
  ufw allow "${{RELAY_PORT}}/udp" >/dev/null 2>&1 || true
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port="${{RELAY_PORT}}/udp" >/dev/null 2>&1 || true
  firewall-cmd --reload >/dev/null 2>&1 || true
fi

echo "[4/4] 校验服务运行与端口监听状态..."
sleep 1
if ! systemctl is-active --quiet rd-relay; then
  journalctl -u rd-relay -n 25 --no-pager || true
  exit 3
fi
echo "RD_RELAY_DEPLOY_OK"
"#
        )
    };
    script.replace("\r\n", "\n")
}

fn find_local_linux_relay_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RD_RELAY_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let mut candidates = vec![
        PathBuf::from("dist/rd-relay-linux-x86_64"),
        PathBuf::from("target/x86_64-unknown-linux-gnu/release/rd-relay"),
        PathBuf::from("target/x86_64-unknown-linux-musl/release/rd-relay"),
        PathBuf::from("../../dist/rd-relay-linux-x86_64"),
        PathBuf::from("../../../dist/rd-relay-linux-x86_64"),
    ];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("rd-relay-linux-x86_64"));
        candidates.push(dir.join("../../dist/rd-relay-linux-x86_64"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 从本机向刚部署好的 `host:relay_port` 发送一次轻量 `LOOKUP_ID` 控制报（13 字节），验证云安全组 UDP 是否已放通。
async fn probe_remote_udp_relay(
    host: &str,
    relay_port: u16,
    preferred_bind_ip: Option<&str>,
) -> bool {
    let Ok(mut addrs) = tokio::net::lookup_host((host, relay_port)).await else {
        return false;
    };
    let Some(remote) = addrs.next() else {
        return false;
    };
    let mut pkt = Vec::with_capacity(13);
    pkt.extend_from_slice(&0u64.to_le_bytes());
    pkt.push(7u8); // TYPE_LOOKUP_ID
    pkt.extend_from_slice(&100_000_000u32.to_le_bytes());

    let mut local_binds: Vec<SocketAddr> = Vec::new();
    if remote.is_ipv6() {
        local_binds.push("[::]:0".parse().expect("ipv6 any"));
    } else {
        local_binds.push("0.0.0.0:0".parse().expect("ipv4 any"));
        if let Some(ip_str) = preferred_bind_ip
            && let Ok(addr) = format!("{ip_str}:0").parse::<SocketAddr>()
        {
            local_binds.push(addr);
        }
    }

    for bind_local in local_binds {
        let Ok(sock) = tokio::net::UdpSocket::bind(bind_local).await else {
            continue;
        };
        for _ in 0..2 {
            let _ = sock.send_to(&pkt, remote).await;
            let mut buf = [0u8; 64];
            if let Ok(Ok((n, _))) =
                tokio::time::timeout(Duration::from_millis(900), sock.recv_from(&mut buf)).await
                && n >= 10
                && buf[..8] == [0u8; 8]
                && buf[8] == 8
            {
                return true;
            }
        }
    }
    false
}

/// 一键 SSH 上传并部署自建中转服务器。
#[tauri::command]
pub async fn deploy_relay_ssh(req: SshDeployRequest) -> Result<SshDeployResult, String> {
    let (host, port, user) = parse_ssh_endpoint(&req.host, req.port, &req.user)?;
    let relay_port = if req.relay_port == 0 {
        21118
    } else {
        req.relay_port
    };
    let mode = if req.mode == "docker" {
        "docker"
    } else {
        "udp"
    };
    let domain = if mode == "docker" {
        validate_host(&req.domain)?
    } else {
        String::new()
    };

    let target = format!("{user}@{host}");
    let mut log = String::new();

    // 1. 探测本机 ssh 可用性
    let mut ver_cmd = Command::new("ssh");
    configure_hidden_cmd(&mut ver_cmd);
    let ver = ver_cmd
        .arg("-V")
        .output()
        .await
        .map_err(|e| format!("未检测到本机 OpenSSH 客户端（ssh 命令不可用）：{e}"))?;
    let ver_str = String::from_utf8_lossy(&ver.stderr);
    let ver_out = String::from_utf8_lossy(&ver.stdout);
    let banner = if ver_str.trim().is_empty() {
        ver_out.trim()
    } else {
        ver_str.trim()
    };
    log.push_str(&format!("[本机] {banner}\n"));

    // 2. 准备受限私钥副本（规避 Windows ACL 继承导致 OpenSSH 报 bad permissions）
    let prepared_key = match prepare_ssh_key(&req.key_path, &mut log).await {
        Ok(k) => k,
        Err(msg) => {
            log.push_str(&format!("[✗] {msg}\n"));
            return Ok(SshDeployResult {
                ok: false,
                relay_addr: String::new(),
                summary: msg,
                log,
            });
        }
    };
    let key_ref = prepared_key.effective_path.as_deref();

    // 3. 测试 SSH 连通性（默认路由优先；若被 TUN/代理重置握手，自动枚举物理网卡 IPv4 直连重试）
    let probe_remote_cmd = "echo RD_SSH_OK && uname -m";
    let mut chosen_bind: Option<String> = None;
    let mut probe_ok = false;
    let mut remote_arch = String::new();
    let mut last_stderr = String::new();

    let mut default_probe = Command::new("ssh");
    configure_hidden_cmd(&mut default_probe);
    let mut probe_args = ssh_base_args(port, key_ref, None);
    probe_args.push(target.clone());
    probe_args.push(probe_remote_cmd.into());
    let probe = default_probe
        .args(&probe_args)
        .output()
        .await
        .map_err(|e| format!("执行 ssh 失败：{e}"))?;

    if probe.status.success() {
        probe_ok = true;
        remote_arch = String::from_utf8_lossy(&probe.stdout)
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_string();
    } else {
        last_stderr = String::from_utf8_lossy(&probe.stderr).to_string();
        let stderr_lc = last_stderr.to_ascii_lowercase();
        let worth_retrying_direct_nic = stderr_lc.contains("kex_exchange_identification")
            || stderr_lc.contains("connection closed")
            || stderr_lc.contains("connection reset")
            || stderr_lc.contains("timed out")
            || stderr_lc.contains("broken pipe");

        if worth_retrying_direct_nic {
            for nic_ip in candidate_physical_ipv4s(&host) {
                log.push_str(&format!(
                    "[…] 默认路由 SSH 握手被重置，尝试绑定本机物理网卡 {nic_ip} 直连 {target}:{port}...\n"
                ));
                let mut retry_cmd = Command::new("ssh");
                configure_hidden_cmd(&mut retry_cmd);
                let mut retry_args = ssh_base_args(port, key_ref, Some(&nic_ip));
                retry_args.push(target.clone());
                retry_args.push(probe_remote_cmd.into());
                if let Ok(out) = retry_cmd.args(&retry_args).output().await {
                    if out.status.success() {
                        probe_ok = true;
                        chosen_bind = Some(nic_ip.clone());
                        remote_arch = String::from_utf8_lossy(&out.stdout)
                            .lines()
                            .last()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        log.push_str(&format!(
                            "[✓] 已绕过虚拟网卡/代理干扰，通过物理网卡 {nic_ip} 直连成功\n"
                        ));
                        break;
                    } else {
                        last_stderr = String::from_utf8_lossy(&out.stderr).to_string();
                    }
                }
            }
        }
    }

    if !probe_ok {
        log.push_str(&format!("[SSH 失败] {}\n", last_stderr.trim()));
        return Ok(SshDeployResult {
            ok: false,
            relay_addr: String::new(),
            summary: diagnose_ssh_error(&last_stderr),
            log,
        });
    }
    log.push_str(&format!(
        "[✓] SSH 连接 {target}:{port} 成功（远端架构：{remote_arch}）\n"
    ));

    // 4. 若本机有匹配 x86_64 的预编译 Linux rd-relay，通过 SSH 流式原子上传（无需 sftp，且避免覆盖运行中二进制报 Text file busy）
    let mut uploaded_prebuilt = false;
    if mode == "udp"
        && remote_arch == "x86_64"
        && let Some(bin) = find_local_linux_relay_bin()
        && let Ok(bin_bytes) = std::fs::read(&bin)
    {
        log.push_str(&format!(
            "[…] 上传本地预编译二进制 {} ({} KB) → /opt/rd-relay/rd-relay\n",
            bin.display(),
            bin_bytes.len() / 1024
        ));
        let mut up_args = ssh_base_args(port, key_ref, chosen_bind.as_deref());
        up_args.push(target.clone());
        up_args.push(
            "mkdir -p /opt/rd-relay && cat > /opt/rd-relay/rd-relay.new && chmod +x /opt/rd-relay/rd-relay.new && mv -f /opt/rd-relay/rd-relay.new /opt/rd-relay/rd-relay".into(),
        );
        let mut up_cmd = Command::new("ssh");
        configure_hidden_cmd(&mut up_cmd);
        if let Ok(mut up_child) = up_cmd
            .args(&up_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = up_child.stdin.take() {
                let _ = stdin.write_all(&bin_bytes).await;
            }
            if let Ok(up_out) = up_child.wait_with_output().await {
                if up_out.status.success() {
                    uploaded_prebuilt = true;
                    log.push_str("[✓] 预编译二进制上传完成\n");
                } else {
                    log.push_str(&format!(
                        "[警告] 预编译二进制流式上传返回非零：{}，将回退远端编译\n",
                        String::from_utf8_lossy(&up_out.stderr).trim()
                    ));
                }
            }
        }
    }

    // 5. 通过 stdin 推送并执行远端安装脚本（无预编译产物时自动携带内嵌 rd-relay 源码编译）
    let script = build_remote_script(relay_port, mode, &domain, !uploaded_prebuilt);
    let mut run_args = ssh_base_args(port, key_ref, chosen_bind.as_deref());
    run_args.push(target.clone());
    run_args.push("bash -s".into());

    let mut run_cmd = Command::new("ssh");
    configure_hidden_cmd(&mut run_cmd);
    let mut child = run_cmd
        .args(&run_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动远端部署脚本失败：{e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(script.as_bytes())
            .await
            .map_err(|e| format!("写入远端脚本失败：{e}"))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("等待远端部署结束失败：{e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    log.push_str(&stdout);
    if !stderr.trim().is_empty() {
        log.push_str("\n[stderr]\n");
        log.push_str(&stderr);
    }

    let relay_addr = if mode == "docker" {
        format!("https://{domain}")
    } else if host.contains(':') {
        format!("[{host}]:{relay_port}")
    } else {
        format!("{host}:{relay_port}")
    };

    if out.status.success() {
        let summary = if mode == "udp" {
            if probe_remote_udp_relay(&host, relay_port, chosen_bind.as_deref()).await {
                log.push_str(&format!(
                    "[✓] 本机 → {relay_addr} UDP 控制报往返实测通过！\n"
                ));
                format!("部署成功且 UDP 实测互通！中继地址：{relay_addr}（已自动回填生效）")
            } else {
                log.push_str(&format!(
                    "[提示] 远端 rd-relay 已启动，但本机探测 UDP {relay_port} 未收到回包：请确认云服务器控制台【防火墙/安全组】已放行 UDP {relay_port}（注意是 UDP 而非仅 TCP）。\n"
                ));
                format!(
                    "部署成功！中继地址：{relay_addr}（请确认云控制台安全组已放行 UDP {relay_port} 端口）"
                )
            }
        } else {
            format!("部署成功！中继地址：{relay_addr}（已就绪，可直接保存使用）")
        };

        Ok(SshDeployResult {
            ok: true,
            relay_addr,
            summary,
            log,
        })
    } else {
        Ok(SshDeployResult {
            ok: false,
            relay_addr: String::new(),
            summary: format!(
                "远端部署未完成（退出码 {:?}），请查看下方日志排查。",
                out.status.code()
            ),
            log,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_host_accepts_ip_and_domain() {
        assert_eq!(validate_host("1.2.3.4").unwrap(), "1.2.3.4");
        assert_eq!(
            validate_host("relay.example.com").unwrap(),
            "relay.example.com"
        );
        assert_eq!(validate_host("2001:db8::1").unwrap(), "2001:db8::1");
    }

    #[test]
    fn validate_host_rejects_injection_and_unsplit_port() {
        assert!(validate_host("").is_err());
        assert!(validate_host("-oProxyCommand=evil").is_err());
        assert!(validate_host("1.2.3.4; rm -rf /").is_err());
        assert!(validate_host("1.2.3.4 && id").is_err());
        assert!(validate_host("43.135.129.15:22").is_err());
    }

    #[test]
    fn parse_ssh_endpoint_handles_host_port_and_user() {
        assert_eq!(
            parse_ssh_endpoint("43.135.129.15:22", 22, "root").unwrap(),
            ("43.135.129.15".into(), 22, "root".into())
        );
        assert_eq!(
            parse_ssh_endpoint("ubuntu@43.135.129.15:2222", 22, "root").unwrap(),
            ("43.135.129.15".into(), 2222, "ubuntu".into())
        );
        assert_eq!(
            parse_ssh_endpoint("[2001:db8::1]:2200", 22, "admin").unwrap(),
            ("2001:db8::1".into(), 2200, "admin".into())
        );
    }

    #[test]
    fn normalize_key_path_strips_quotes() {
        assert_eq!(
            normalize_key_path(r#" "G:\work.pem" "#),
            Some(PathBuf::from(r"G:\work.pem"))
        );
        assert_eq!(normalize_key_path("   "), None);
    }

    #[test]
    fn validate_user_rejects_injection() {
        assert_eq!(validate_user("root").unwrap(), "root");
        assert_eq!(validate_user("ubuntu_1").unwrap(), "ubuntu_1");
        assert!(validate_user("").is_err());
        assert!(validate_user("root;id").is_err());
        assert!(validate_user("-root").is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn env_live_ssh_deploy() {
        let host = std::env::var("RD_TEST_SSH_HOST").expect("set RD_TEST_SSH_HOST");
        let user = std::env::var("RD_TEST_SSH_USER").unwrap_or_else(|_| "root".into());
        let key_path = std::env::var("RD_TEST_SSH_KEY").unwrap_or_default();
        let res = deploy_relay_ssh(SshDeployRequest {
            host,
            port: 22,
            user,
            key_path,
            relay_port: 21118,
            mode: "udp".into(),
            domain: String::new(),
        })
        .await
        .expect("deploy_relay_ssh call");
        println!("=== SUMMARY ===\n{}\n=== LOG ===\n{}", res.summary, res.log);
        assert!(res.ok, "SSH deploy failed: {}", res.summary);
    }
}
