@echo off
set "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223"
title rd 控制端（不要关闭本窗口）
echo ============================================
echo   rd 控制端启动中（首次约 1~3 分钟编译，请等待）
echo   完成后自动弹出 rd-controller 窗口
echo   本窗口不要关闭（关了控制端就退出）
echo ============================================
cd /d "C:\rd\apps\rd-controller"
"F:\Program Files\nodejs\node.exe" "C:\Users\blank\AppData\Local\node\corepack\v1\pnpm\12.11.2\bin\pnpm.mjs" tauri dev
pause
