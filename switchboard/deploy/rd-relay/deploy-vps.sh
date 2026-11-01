#!/usr/bin/env bash
# rd 原生中继（rd-relay）· VPS 部署（Linux，在【你自己的电脑】上运行）
#
# 与路线 A（frp）相比：不需要被控端装任何东西——被控端只改一行配置；
# rd 代码原生支持，端到端加密不变。这是三条路线里部署完之后最省事的。
#
# 用法：
#   ./deploy-vps.sh root@1.2.3.4            # 按提示输密码（或已配密钥免密）
#   ./deploy-vps.sh root@1.2.3.4 2222       # SSH 非默认端口
#
# rd-relay 二进制的来源（按优先级）：
#   1. 本仓库已编译的 target/release/rd-relay（Windows 上没有 Linux 版则跳过）
#   2. GitHub Release（RD_RELAY_DOWNLOAD_URL 指定的 tar.gz，内含 rd-relay）
#   3. 都没有 → 在 VPS 上用 cargo 编译（自动装 rustup，约 5-10 分钟）
#
# 环境变量（可选）：
#   RD_RELAY_PORT=21118     中继监听端口
#   RD_RELAY_BIN=/path/rd-relay  直接指定本地已编译的 Linux 二进制

set -euo pipefail

FRP_FALLBACK_NOTE="提示：如果不想起 VPS 编译，可以用路线 A（../relay-frp/，纯 frp，3 分钟）"

TARGET="${1:-}"
SSH_PORT="${2:-22}"
RELAY_PORT="${RD_RELAY_PORT:-21118}"
REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

say()  { printf '\033[1;32m[✓]\033[0m %s\n' "$*"; }
step() { printf '\n\033[1;36m[…]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[✗] %s\033[0m\n' "$*" >&2; exit 1; }

[ -n "$TARGET" ] || die "用法: $0 用户@VPS的IP [SSH端口，默认22]"
command -v ssh >/dev/null || die "本机没有 ssh。Windows 用户请用 Git Bash / WSL。"

# ── SSH 通道（与 relay-frp/deploy-vps.sh 同策略：密钥优先，密码用 sshpass）──
step "检查本地 SSH 环境"
USE_SSHPASS=0
if ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$SSH_PORT" "$TARGET" true 2>/dev/null; then
  say "密钥登录可用"
elif command -v sshpass >/dev/null 2>&1; then
  USE_SSHPASS=1; say "使用密码登录（已装 sshpass）"
else
  echo "  尝试自动安装 sshpass…"
  if command -v apt-get >/dev/null 2>&1; then sudo apt-get install -y sshpass >/dev/null 2>&1 || true
  elif command -v dnf >/dev/null 2>&1; then sudo dnf install -y sshpass >/dev/null 2>&1 || true
  elif command -v yum >/dev/null 2>&1; then sudo yum install -y sshpass >/dev/null 2>&1 || true
  elif command -v brew >/dev/null 2>&1; then brew install hudochenkov/sshpass/sshpass >/dev/null 2>&1 || true
  fi
  command -v sshpass >/dev/null 2>&1 || die "sshpass 安装失败。改用密钥：ssh-copy-id $TARGET 后重跑；或用路线 A（frp）"
  USE_SSHPASS=1; say "sshpass 就绪"
fi
if [ "$USE_SSHPASS" = "1" ]; then
  printf '请输入 %s 的 SSH 密码: ' "$TARGET"; read -rs SSHPASS_INPUT; echo
  export SSHPASS="$SSHPASS_INPUT"
fi
rsh() { if [ "$USE_SSHPASS" = "1" ]; then sshpass -e ssh -o StrictHostKeyChecking=accept-new -p "$SSH_PORT" "$TARGET" "$@"; else ssh -o StrictHostKeyChecking=accept-new -p "$SSH_PORT" "$TARGET" "$@"; fi; }
rscp() { if [ "$USE_SSHPASS" = "1" ]; then sshpass -e scp -o StrictHostKeyChecking=accept-new -P "$SSH_PORT" "$@"; else scp -o StrictHostKeyChecking=accept-new -P "$SSH_PORT" "$@"; fi; }

step "1/5 测试 SSH"
rsh -o ConnectTimeout=10 "echo ok" >/dev/null 2>&1 || die "SSH 连不上。检查 IP/端口/密码与 VPS 防火墙。"
say "SSH 连接成功"

step "2/5 上传或获取 rd-relay 二进制"
rsh "mkdir -p /opt/rd-relay"
LOCAL_BIN=""
for cand in "$REPO_ROOT/target/x86_64-unknown-linux-gnu/release/rd-relay" \
            "$RD_RELAY_BIN" ; do
    [ -n "$cand" ] && [ -f "$cand" ] && LOCAL_BIN="$cand" && break
done
if [ -n "$LOCAL_BIN" ]; then
  echo "  上传本地二进制：$LOCAL_BIN"
  rscp "$LOCAL_BIN" "$TARGET:/opt/rd-relay/rd-relay"
elif [ -n "${RD_RELAY_DOWNLOAD_URL:-}" ]; then
  echo "  从 Release 下载：$RD_RELAY_DOWNLOAD_URL"
  rsh "curl -fsSL --connect-timeout 10 -o /tmp/rd-relay.tar.gz '$RD_RELAY_DOWNLOAD_URL' \
       && tar xzf /tmp/rd-relay.tar.gz -C /opt/rd-relay --strip-components=0"
else
  echo "  本地无 Linux 二进制 → 在 VPS 上用 cargo 编译（首次约 5-10 分钟，$FRP_FALLBACK_NOTE）"
  rsh "command -v cargo >/dev/null 2>&1 || (curl -fsSL https://sh.rustup.rs | sh -s -- -y >/dev/null 2>&1 || true)
       export PATH=\"\$HOME/.cargo/bin:\$PATH\"
       command -v cargo >/dev/null 2>&1 || { echo 'rustup 安装失败'; exit 1; }
       mkdir -p /opt/rd-relay && cd /opt/rd-relay
       if [ ! -f Cargo.toml ]; then
         # 只取仓库里 crates/rd-relay 的最小源码集（通过 git 或上传；这里用 git）
         REPO_URL=\"\${RD_RELAY_GIT_REPO:-}\"
         [ -n \"\$REPO_URL\" ] || { echo '需要 RD_RELAY_GIT_REPO 环境变量指向本仓库（VPS 可访问）'; exit 1; }
         git clone --depth 1 \"\$REPO_URL\" repo
         cd repo/crates/rd-relay
       fi
       cargo build --release
       cp target/release/rd-relay /opt/rd-relay/rd-relay 2>/dev/null || cp ../../target/release/rd-relay /opt/rd-relay/rd-relay"
fi
say "二进制就绪"

step "3/5 安装 systemd 服务（开机自启）"
rsh "cat > /etc/systemd/system/rd-relay.service <<'UNIT'
[Unit]
Description=rd native relay (E2E encrypted UDP forwarder)
After=network.target
[Service]
ExecStart=/opt/rd-relay/rd-relay 0.0.0.0:${RELAY_PORT}
Restart=always
RestartSec=3
[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload && systemctl enable --now rd-relay >/dev/null 2>&1"
say "服务已启动并设为开机自启"

step "4/5 防火墙放行 UDP ${RELAY_PORT}"
rsh "if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q active; then ufw allow ${RELAY_PORT}/udp; echo 'ufw 已放行';
     elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then firewall-cmd --permanent --add-port=${RELAY_PORT}/udp && firewall-cmd --reload; echo 'firewalld 已放行';
     else echo '未检测到系统防火墙，跳过'; fi"
echo "  ⚠ 云控制台【安全组】记得放行 UDP ${RELAY_PORT}（就这一个端口，不用 TCP）"

step "5/5 验证"
sleep 1
rsh "systemctl is-active rd-relay" >/dev/null || die "服务没起来：journalctl -u rd-relay -n 20"
rsh "ss -lun 2>/dev/null | grep -q ':${RELAY_PORT} '" && say "rd-relay 运行中，端口 ${RELAY_PORT}/udp 监听中" \
  || die "端口未监听：journalctl -u rd-relay -n 20"

VPS_IP="${TARGET#*@}"; VPS_IP="${VPS_IP%%:*}"
cat <<EOF

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
部署完成！剩下两步（都是一行配置）：

① 被控端（要被远控的电脑）二选一：
   a) 编辑 %LOCALAPPDATA%\\rd\\agent.toml，加一行：
        relay_addr = "${VPS_IP}:${RELAY_PORT}"
      （保存后重启 rd）
   b) 或启动前设环境变量 RD_RELAY=${VPS_IP}:${RELAY_PORT}

② 控制端（你手里的电脑）：
   设备 → 编辑该设备 → 「原生中继」填 ${VPS_IP}:${RELAY_PORT} → 保存
   连接时直连失败会自动走中继（同网段时仍走直连，中继只是兜底）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
EOF
say "全部完成"
