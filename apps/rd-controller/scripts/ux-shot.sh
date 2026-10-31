#!/bin/bash
# UX 评审截图脚本：截取 rd-controller 四个关键界面 → screenshots/<前缀>/
# 用法: bash scripts/ux-shot.sh <输出目录名>
set -u
CHROME="/c/Program Files/Google/Chrome/Application/chrome.exe"
PORT=1420
OUT="${1:?用法: ux-shot.sh <输出目录名>}"
BASE="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$BASE/screenshots/$OUT"
mkdir -p "$DIR"

shots=(
  "01-devices:#/devices"
  "02-windows:#/windows/dev-studio"
  "03-session-empty:#/session/dev-studio"
  "04-session-window:#/session/dev-studio?open=3301"
  "05-session-term:#/session/dev-studio?term=pwsh"
)

for entry in "${shots[@]}"; do
  name="${entry%%:*}"
  hash="${entry#*:}"
  echo "shot: $name"
  "/c/Program Files/Google/Chrome/Application/chrome.exe" \
    --headless=new --disable-gpu --hide-scrollbars \
    --window-size=1440,900 --virtual-time-budget=5000 \
    --run-all-compositor-stages-before-draw \
    --screenshot="$(cygpath -w "$DIR/$name.png")" \
    "http://localhost:$PORT/$hash" 2>/dev/null
done
echo "done -> $DIR"
