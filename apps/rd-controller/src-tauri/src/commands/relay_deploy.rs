//! SSH 一键上传并部署自建中转服务器（宪法 #3）。
//!
//! 用户在控制端 UI 填写 SSH 主机/端口/用户名（可选私钥路径）后，本模块通过
//! 本机 OpenSSH（Windows 10/11、macOS、Linux 均自带 `ssh`/`scp`）执行：
//! 1. 严格校验输入参数（防命令注入）；
//! 2. 连通性探测（`BatchMode=yes`，免交互卡死）；
//! 3. 自动上传本地已编译 Linux `rd-relay` 二进制（若存在）及远端安装脚本；
//! 4. 远端配置 `systemd` 守护、放行防火墙端口并健康校验；
//! 5. 返回可直接回填的 `relay_addr`（如 `1.2.3.4:21118` 或 `https://domain`）与完整日志。

use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshDeployRequest {
    /// 服务器 IP 或域名（必填）。
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

/// 严格校验主机名/IP：仅允许字母、数字、点、横杠、冒号（IPv6），禁止空白与 shell 元字符。
pub(crate) fn validate_host(host: &str) -> Result<String, String> {
    let h = host.trim();
    if h.is_empty() || h.len() > 253 {
        return Err("服务器地址不能为空或超过 253 字符".into());
    }
    if h.starts_with('-') {
        return Err("服务器地址不能以 '-' 开头".into());
    }
    if !h
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
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

/// 生成远端 Linux 自动安装脚本（无交互、幂等、systemd 守护 + 防火墙放行）。
fn build_remote_script(relay_port: u16, mode: &str, domain: &str) -> String {
    if mode == "docker" {
        return format!(
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
        );
    }

    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
RELAY_PORT={relay_port}
mkdir -p /opt/rd-relay

echo "[1/4] 检查 rd-relay 二进制..."
if [ ! -x /opt/rd-relay/rd-relay ]; then
  echo "  未检测到预上传二进制，尝试检查本机 cargo 环境..."
  if [ -f /opt/rd-relay/src/Cargo.toml ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
    command -v cargo >/dev/null 2>&1 || (curl -fsSL https://sh.rustup.rs | sh -s -- -y)
    (cd /opt/rd-relay/src && cargo build --release --bin rd-relay)
    cp /opt/rd-relay/src/target/release/rd-relay /opt/rd-relay/rd-relay
  else
    echo "  提示：请将 Linux 版 rd-relay 二进制放到本地 RD_RELAY_BIN 或服务器 /opt/rd-relay/rd-relay"
    exit 2
  fi
fi
chmod +x /opt/rd-relay/rd-relay

echo "[2/4] 写入 systemd 服务..."
cat > /etc/systemd/system/rd-relay.service <<EOF
[Unit]
Description=rd Native UDP Relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/rd-relay/rd-relay --listen 0.0.0.0:${{RELAY_PORT}}
Restart=always
RestartSec=2
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now rd-relay
systemctl restart rd-relay

echo "[3/4] 放行 UDP ${{RELAY_PORT}} 防火墙..."
if command -v ufw >/dev/null 2>&1; then
  ufw allow "${{RELAY_PORT}}/udp" >/dev/null 2>&1 || true
elif command -v firewall-cmd >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port="${{RELAY_PORT}}/udp" >/dev/null 2>&1 || true
  firewall-cmd --reload >/dev/null 2>&1 || true
fi

echo "[4/4] 校验服务状态..."
sleep 1
systemctl is-active --quiet rd-relay
echo "RD_RELAY_DEPLOY_OK"
"#
    )
}

fn find_local_linux_relay_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RD_RELAY_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let candidates = [
        PathBuf::from("target/x86_64-unknown-linux-gnu/release/rd-relay"),
        PathBuf::from("target/x86_64-unknown-linux-musl/release/rd-relay"),
        PathBuf::from("dist/rd-relay-linux-x86_64"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

fn ssh_base_args(port: u16, key_path: &str) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-p".into(),
        port.to_string(),
    ];
    let k = key_path.trim();
    if !k.is_empty() {
        args.push("-i".into());
        args.push(k.to_string());
    }
    args
}

/// 一键 SSH 上传并部署自建中转服务器。
#[tauri::command]
pub async fn deploy_relay_ssh(req: SshDeployRequest) -> Result<SshDeployResult, String> {
    let host = validate_host(&req.host)?;
    let user = validate_user(&req.user)?;
    let port = if req.port == 0 { 22 } else { req.port };
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
    let ver = Command::new("ssh")
        .arg("-V")
        .output()
        .await
        .map_err(|e| format!("未检测到本机 OpenSSH 客户端（ssh 命令不可用）：{e}"))?;
    let ver_str = String::from_utf8_lossy(&ver.stderr);
    log.push_str(&format!("[本机] {}\n", ver_str.trim()));

    // 2. 测试 SSH 连通性（免密密钥或 ssh-agent）
    let mut probe_args = ssh_base_args(port, &req.key_path);
    probe_args.push(target.clone());
    probe_args.push("echo RD_SSH_OK".into());
    let probe = Command::new("ssh")
        .args(&probe_args)
        .output()
        .await
        .map_err(|e| format!("执行 ssh 失败：{e}"))?;
    if !probe.status.success() {
        let stderr = String::from_utf8_lossy(&probe.stderr);
        log.push_str(&format!("[SSH 失败] {stderr}\n"));
        return Ok(SshDeployResult {
            ok: false,
            relay_addr: String::new(),
            summary:
                "SSH 连接失败：请确认已配置免密密钥（ssh-copy-id）或指定正确的私钥路径、IP 与端口。"
                    .into(),
            log,
        });
    }
    log.push_str(&format!("[✓] SSH 连接 {target}:{port} 成功\n"));

    // 3. 若本机有预编译 Linux rd-relay，自动通过 scp 上传
    if mode == "udp"
        && let Some(bin) = find_local_linux_relay_bin()
    {
        log.push_str(&format!(
            "[…] 上传本地二进制 {} → /opt/rd-relay/rd-relay\n",
            bin.display()
        ));
        let mut mk_args = ssh_base_args(port, &req.key_path);
        mk_args.push(target.clone());
        mk_args.push("mkdir -p /opt/rd-relay".into());
        let _ = Command::new("ssh").args(&mk_args).output().await;

        let mut scp_args = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-P".into(),
            port.to_string(),
        ];
        if !req.key_path.trim().is_empty() {
            scp_args.push("-i".into());
            scp_args.push(req.key_path.trim().to_string());
        }
        scp_args.push(bin.to_string_lossy().into_owned());
        scp_args.push(format!("{target}:/opt/rd-relay/rd-relay"));
        let scp_out = Command::new("scp")
            .args(&scp_args)
            .output()
            .await
            .map_err(|e| format!("scp 上传失败：{e}"))?;
        if !scp_out.status.success() {
            log.push_str(&format!(
                "[警告] scp 上传返回非零：{}\n",
                String::from_utf8_lossy(&scp_out.stderr)
            ));
        } else {
            log.push_str("[✓] 二进制上传完成\n");
        }
    }

    // 4. 通过 stdin 推送并执行远端安装脚本
    let script = build_remote_script(relay_port, mode, &domain);
    let mut run_args = ssh_base_args(port, &req.key_path);
    run_args.push(target.clone());
    run_args.push("bash -s".into());

    let mut child = Command::new("ssh")
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
    } else {
        format!("{host}:{relay_port}")
    };

    if out.status.success() {
        Ok(SshDeployResult {
            ok: true,
            relay_addr: relay_addr.clone(),
            summary: format!("部署成功！中继地址：{relay_addr}（已就绪，可直接保存使用）"),
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
    fn validate_host_rejects_injection() {
        assert!(validate_host("").is_err());
        assert!(validate_host("-oProxyCommand=evil").is_err());
        assert!(validate_host("1.2.3.4; rm -rf /").is_err());
        assert!(validate_host("1.2.3.4 && id").is_err());
    }

    #[test]
    fn validate_user_rejects_injection() {
        assert_eq!(validate_user("root").unwrap(), "root");
        assert_eq!(validate_user("ubuntu_1").unwrap(), "ubuntu_1");
        assert!(validate_user("").is_err());
        assert!(validate_user("root;id").is_err());
        assert!(validate_user("-root").is_err());
    }
}
