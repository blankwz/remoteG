@echo off
title 测试窗口（时钟）
echo ============================================
echo   测试窗口启动中（右上角有毫秒时间码）
echo   用途：延迟拍照 / 清晰度比对源
echo   测完直接关掉弹出的窗口即可
echo ============================================
start "" "C:\rd\target\release\clarity-bench.exe" testwin --mode clock --size 960x600
timeout /t 2 >nul
echo 窗口已弹出，本窗口可以关闭。
pause
