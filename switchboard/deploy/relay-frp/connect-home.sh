#!/usr/bin/env bash
# rd 一键中继·被控端接线（在被控的 Linux 电脑/Mac 上运行；Windows 用 connect-home.ps1）
#
# 用法：
#   ./connect-home.sh <VPS的IP> <中继令牌>
#   （deploy-vps.sh 生成的 .relay-info 在本脚本同目录时会自动读取，两个参数都可省）
set -euo pipefail

RELAY_PORT=21118
LOCAL_PORT=21118
FRP_VERSION="${FRP_VERSION:-0.61.1}"
HERE="$(cd "$(dirname "$0")" && pwd)"

say()  { printf '\033[1;32m[✓]\033[0m %s\n' "$*"; }
step() { printf '\n\033[1;36m[…]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[✗] %s\033[0m\n' "$*" >&2; exit 1; }

# 自动读取 .relay-info
if [ -z "${1:-}" ] && [ -f "$HERE/.relay-info" ]; then
  # shellcheck disable=SC1091
  . "$HERE/.relay-info"
  say "已从 .relay-info 读取 VPS=$VPS"
fi
SERVER="${1:-${VPS:-}}"
TOKEN="${2:-${TOKEN:-}}"
[ -n "$SERVER" ] && [ -n "$TOKEN" ] || die "用法: $0 <VPS的IP> <中继令牌>
（Linux VPS 部署后，.relay-info 在本脚本旁边可自动读取）"

# 0. 本机 rd 在跑吗
step "0/3 检查本机 rd 被控端（端口 $LOCAL_PORT）"
if ss -lun 2>/dev/null | grep -q ":$LOCAL_PORT " || netstat -lun 2>/dev/null | grep -q ":$LOCAL_PORT "; then
  say "rd 被控端在跑"
else
  die "本机 $LOCAL_PORT 端口没监听——先启动 rd 被控端再跑本脚本"
fi

# 1. 下载 frpc
step "1/3 下载 frp 客户端（$FRP_VERSION）"
ARCH=$(uname -m); case "$ARCH" in x86_64) FARCH=amd64;; aarch64) FARCH=arm64;; *) die "不支持的架构: $ARCH";; esac
INSTALL_ROOT="/opt/rd-relay-home"; [ "$(id -u)" != "0" ] && INSTALL_ROOT="$HOME/.rd-relay-home"
mkdir -p "$INSTALL_ROOT" && cd "$INSTALL_ROOT"
if [ ! -x "frp_${FRP_VERSION}_linux_${FARCH}/frpc" ]; then
  TARBALL="frp_${FRP_VERSION}_linux_${FARCH}.tar.gz"
  OK=0
  for M in "https://github.com" "https://ghfast.top/https://github.com" "https://mirror.ghproxy.com/https://github.com"; do
    curl -fsSL --connect-timeout 8 -o "$TARBALL" "$M/fatedier/frp/releases/download/v${FRP_VERSION}/${TARBALL}" && OK=1 && break || true
  done
  [ "$OK" = "1" ] || die "下载失败。手动下载 $TARBALL 解压到 $INSTALL_ROOT 后重跑"
  tar xzf "$TARBALL"
fi
say "frpc 就绪"

# 2. 配置 + 自启（有 systemd 才注册服务）
step "2/3 写配置并设开机自启"
cat > "$INSTALL_ROOT/frpc.toml" <<EOF
serverAddr = "$SERVER"
serverPort = 7000
auth.token = "$TOKEN"

[[proxies]]
name = "rd-quic"
type = "udp"
localIP = "127.0.0.1"
localPort = $LOCAL_PORT
remotePort = $RELAY_PORT
EOF
FPC="$INSTALL_ROOT/frp_${FRP_VERSION}_linux_${FARCH}/frpc"
if command -v systemctl >/dev/null 2>&1 && [ "$(id -u)" = "0" ]; then
  cat > /etc/systemd/system/rd-relay-home.service <<EOF
[Unit]
Description=rd relay home (frpc)
After=network.target
[Service]
ExecStart=$FPC -c $INSTALL_ROOT/frpc.toml
Restart=always
RestartSec=3
[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload && systemctl enable --now rd-relay-home >/dev/null 2>&1
  say "开机自启已注册（rd-relay-home）"
else
  (nohup "$FPC" -c "$INSTALL_ROOT/frpc.toml" >/dev/null 2>&1 &) && say "frpc 已后台启动（无 root/systemd，开机自启需自行处理）"
fi

# 3. 验证
step "3/3 验证与 VPS 的连接"
sleep 2
"$FPC" verify -c "$INSTALL_ROOT/frpc.toml" >/dev/null || die "frpc 配置校验失败"
if timeout 5 bash -c "cat < /dev/null > /dev/tcp/$SERVER/7000" 2>/dev/null; then
  say "能连上 VPS——中继链路已打通"
else
  die "连不上 VPS 的 7000 端口。检查云安全组：TCP 7000 + UDP $RELAY_PORT"
fi

cat <<EOF

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
全部完成！最后一步（在控制端电脑上）：
  控制端 → 设备 → 添加设备 → 地址填：${SERVER}:${RELAY_PORT}
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
EOF
