@echo off
title 清晰度自动比对
echo ============================================
echo   清晰度自动比对（需要：控制端正显示测试窗口画面）
echo   步骤：先在控制端按 Ctrl+Shift+S 导出画面，再双击本脚本
echo ============================================
set "DL=%USERPROFILE%\Downloads"
set "SRC=C:\rd-test\source.png"
echo [1/3] 截取本机屏幕作为源图...
powershell -Command Add-Type -AssemblyName System.Windows.Forms,System.Drawing; $b=[System.Windows.Forms.SystemInformation]::VirtualScreen; $bmp=New-Object System.Drawing.Bitmap $b.Width,$b.Height; $g=[System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($b.X,$b.Y,0,0,$b.Size); $bmp.Save('C:d-test\source.png'); $g.Dispose(); $bmp.Dispose()
echo [2/3] 查找控制端导出的画面（下载目录最新 rd-*.png）...
powershell -Command $f = Get-ChildItem '$env:USERPROFILE\Downloadsd-*.png' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1; if ($f) { Copy-Item $f.FullName 'C:d-testeceived.png' -Force; Write-Host found } else { Write-Host NO_EXPORT }
if not exist "C:d-testeceived.png" (
  echo [失败] 未找到导出图：先在控制端按 Ctrl+Shift+S 导出画面，再重跑本脚本
  pause
  exit /b
)
echo [3/3] 逐像素比对中...
cd /d "C:d"
"C:\rd\target\release\clarity-bench.exe" compare "C:\rd-test\received.png" "C:\rd-test\source.png" --json
echo.
echo 判读：text_exact_ratio = 1.0 即文字逐像素一致（满分）
echo 按任意键关闭
pause >nul
