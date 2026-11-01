#!/usr/bin/env bash
# rd 一键中继·VPS 部署（Linux 服务器，在【你自己的电脑】上运行）
#
# 做什么：SSH 连上你的 VPS → 装 frp 服务端 → 配成开机自启服务 → 开防火墙 →
#         验证端口在听 → 告诉你下一步填什么。全程自动，失败会用人话告诉你卡在哪。
#
# 用法（二选一）：
#   ./deploy-vps.sh root@1.2.3.4            # 会提示输密码（只输一次）
#   ./deploy-vps.sh root@1.2.3.4 2222       # SSH 端口不是 22 时，第二个参数指定
#
# 前提：本地有 ssh；密码登录时本地需有 sshpass（脚本会尝试自动安装）。
#       用密钥登录则不需要密码，直接就能跑。

set -euo pipefail

# ── 可按需调整的默认值（一般不用动）────────────────────────────────
FRP_VERSION="${FRP_VERSION:-0.61.1}"   # frp 版本（钉住，避免上游变动踩坑）
RELAY_PORT=21118                        # rd 的 QUIC 端口（与被控端一致，别改）
FRP_TOKEN="${FRP_TOKEN:-$(openssl rand -hex 16 2>/dev/null || head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
GH_MIRRORS=(
  "https://github.com"
  "https://ghfast.top/https://github.com"
  "https://mirror.ghproxy.com/https://github.com"
)

# ── 人话输出 ──────────────────────────────────────────────────────
say()  { printf '\033[1;32m[✓]\033[0m %s\n' "$*"; }
step() { printf '\n\033[1;36m[…]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[✗] %s\033[0m\n' "$*" >&2; exit 1; }

# ── 参数检查 ──────────────────────────────────────────────────────
TARGET="${1:-}"
SSH_PORT="${2:-22}"
[ -n "$TARGET" ] || die "用法: $0 用户@VPS的IP [SSH端口，默认22]
例子: $0 root@123.45.67.89"
command -v ssh >/dev/null || die "本机没有 ssh 命令。Windows 用户请用 Git Bash / WSL 运行本脚本。"

# ── 密码登录准备：优先密钥，密码则需要 sshpass ────────────────────
USE_SSHPASS=0
step "检查本地 SSH 环境"
if ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$SSH_PORT" "$TARGET" true 2>/dev/null; then
  say "密钥登录可用（无需密码）"
elif command -v sshpass >/dev/null 2>&1; then
  USE_SSHPASS=1
  say "使用密码登录（已装 sshpass）"
else
  echo "  未检测到免密登录，尝试自动安装 sshpass（密码登录辅助工具）…"
  if command -v apt-get >/dev/null 2>&1; then sudo apt-get install -y sshpass >/dev/null 2>&1 || true
  elif command -v dnf >/dev/null 2>&1; then sudo dnf install -y sshpass >/dev/null 2>&1 || true
  elif command -v yum >/dev/null 2>&1; then sudo yum install -y sshpass >/dev/null 2>&1 || true
  elif command -v pacman >/dev/null 2>&1; then sudo pacman -S --noconfirm sshpass >/dev/null 2>&1 || true
  elif command -v brew >/dev/null 2>&1; then brew install sshpass >/dev/null 2>&1 || true
  fi
  if command -v sshpass >/dev/null 2>&1; then USE_SSHPASS=1; say "sshpass 装好了"
  else die "自动安装 sshpass 失败。两个办法任选：
  1) 手动装：apt install sshpass（或 brew install hudochenkov/sshpass/sshpass）
  2) 改用密钥登录：ssh-copy-id $TARGET 后重跑本脚本"; fi
fi

if [ "$USE_SSHPASS" = "1" ]; then
  printf '请输入 %s 的 SSH 密码: ' "$TARGET"
  read -rs SSHPASS_INPUT
  echo
  export SSHPASS="$SSHPASS_INPUT"
  SH="sshpass -e ssh -o StrictHostKeyChecking=accept-new -p $SSH_PORT $TARGET"
  SC="sshpass -e scp -o StrictHostKeyChecking=accept-new -P $SSH_PORT"
else
  SH="ssh -o StrictHostKeyChecking=accept-new -p $SSH_PORT $TARGET"
  SC="scp -o StrictHostKeyChecking=accept-new -P $SSH_PORT"
fi
# r()：统一执行入口。$SH/$SC 已内嵌 sshpass（密码在 SSHPASS 环境变量），
# 这里只负责原样执行——别再包一层 sshpass（会变成 sshpass -e sshpass -e ssh）
r() { "$@"; }

# ── 1. 连通性 ─────────────────────────────────────────────────────
step "1/5 测试 SSH 能否连上 VPS"
r $SH -o ConnectTimeout=10 "echo ok" >/dev/null 2>&1 \
  || die "SSH 连不上。检查：IP/端口/密码是否正确；VPS 防火墙是否放行了 SSH 端口。"
say "SSH 连接成功"

# ── 2. 下载并安装 frp ─────────────────────────────────────────────
step "2/5 在 VPS 上下载安装 frp（服务端 frps）"
r $SH bash -s -- "$FRP_VERSION" "${RELAY_PORT}" "$FRP_TOKEN" <<'REMOTE'
set -euo pipefail
FRP_VERSION="$1"; RELAY_PORT="$2"; FRP_TOKEN="$3"
ARCH=$(uname -m); case "$ARCH" in x86_64) FARCH=amd64;; aarch64) FARCH=arm64;; *) echo "不支持的架构: $ARCH"; exit 1;; esac
mkdir -p /opt/rd-relay && cd /opt/rd-relay
if [ ! -x "frp_${FRP_VERSION}_linux_${FARCH}/frps" ]; then
  TARBALL="frp_${FRP_VERSION}_linux_${FARCH}.tar.gz"
  OK=0
  for M in "https://github.com" "https://ghfast.top/https://github.com" "https://mirror.ghproxy.com/https://github.com"; do
    echo "  尝试下载: $M/releases/download/v${FRP_VERSION}/${TARBALL}"
    curl -fsSL --connect-timeout 8 -o "$TARBALL" "$M/fatedier/frp/releases/download/v${FRP_VERSION}/${TARBALL}" && OK=1 && break
  done
  [ "$OK" = "1" ] || { echo "下载失败（GitHub 可能被墙）。给 VPS 换源或手动上传 $TARBALL 到 /opt/rd-relay 后重跑。"; exit 1; }
  tar xzf "$TARBALL"
fi
cat > /opt/rd-relay/frps.toml <<EOF
bindAddr = "0.0.0.0"
bindPort = 7000
auth.token = "${FRP_TOKEN}"
EOF
cat > /etc/systemd/system/rd-relay.service <<EOF
[Unit]
Description=rd relay (frps)
After=network.target
[Service]
ExecStart=/opt/rd-relay/frp_${FRP_VERSION}_linux_${FARCH}/frps -c /opt/rd-relay/frps.toml
Restart=always
RestartSec=3
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload && systemctl enable --now rd-relay >/dev/null 2>&1
echo "frps 已安装并设为开机自启"
REMOTE
say "frp 服务端安装完成"

# ── 3. 防火墙放行 ─────────────────────────────────────────────────
step "3/5 放行防火墙端口（UDP ${RELAY_PORT} + TCP 7000）"
r $SH bash -s -- "${RELAY_PORT}" <<'REMOTE'
set -euo pipefail
RELAY_PORT="$1"
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
  ufw allow ${RELAY_PORT}/udp >/dev/null 2>&1 || true; ufw allow 7000/tcp >/dev/null 2>&1 || true; echo "ufw 已放行"
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port=${RELAY_PORT}/udp >/dev/null 2>&1 || true
  firewall-cmd --permanent --add-port=7000/tcp >/dev/null 2>&1 || true
  firewall-cmd --reload >/dev/null 2>&1 || true; echo "firewalld 已放行"
else
  echo "没有检测到启用中的防火墙（云服务器也可能没装），跳过"
fi
REMOTE
echo "  ⚠ 别忘了云控制台的【安全组】也要放行：UDP ${RELAY_PORT} 和 TCP 7000"
say "防火墙处理完成"

# ── 4. 验证 ───────────────────────────────────────────────────────
step "4/5 验证服务在运行、端口在监听"
sleep 1
r $SH "systemctl is-active rd-relay" >/dev/null \
  || die "rd-relay 服务没起来。到 VPS 上跑 journalctl -u rd-relay -n 20 看原因。"
r $SH "ss -lun 2>/dev/null | grep -q ':${RELAY_PORT} ' || netstat -lun 2>/dev/null | grep -q ':${RELAY_PORT} '" \
  && say "frps 运行中，端口 ${RELAY_PORT}/udp 正在监听" \
  || die "服务在跑但端口没监听（正常现象也可能是还没客户端接入，可继续第 5 步，连接不通再回来查）"

# ── 5. 收尾报告 ───────────────────────────────────────────────────
VPS_IP="${TARGET#*@}"; VPS_IP="${VPS_IP%%:*}"
say "VPS 部署全部完成！"
cat <<EOF

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
下一步（在被控端电脑上）：
  双击运行 connect-home.ps1（或看 README 第 2 步），
  它会问你要 VPS IP，把中继打通。

记下这两个值（被控端脚本会用到）：
  VPS 地址 : ${VPS_IP}
  中继令牌 : ${FRP_TOKEN}
  （已保存在本目录 .relay-info，connect-home.ps1 会自动读取）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
EOF
printf 'VPS=%s\nTOKEN=%s\nPORT=%s\n' "$VPS_IP" "$FRP_TOKEN" "$RELAY_PORT" > "$(dirname "$0")/.relay-info"
say "连接信息已写入 $(dirname "$0")/.relay-info"
