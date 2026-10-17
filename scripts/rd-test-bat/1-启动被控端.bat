@echo off
title rd 被控端
echo ============================================
echo   rd 被控端 Agent 启动中...
echo   托盘出现图标即成功。本窗口可关闭。
echo ============================================
start "" "C:\rd\target\release\rd-agent.exe" run --listen 127.0.0.1:21118
timeout /t 3 >nul
tasklist | findstr /i "rd-agent" >nul && (
  echo [成功] Agent 已运行，托盘有图标。直接进行下一步即可。
) || (
  echo [失败] Agent 未运行，请截图本窗口报错
)
echo 按任意键关闭本窗口（Agent 继续在后台运行）
pause >nul
