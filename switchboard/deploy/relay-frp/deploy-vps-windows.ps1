# rd 一键中继·VPS 部署（Windows 服务器版——【在这台 Windows 服务器上】直接运行）
#
# 做什么：下载 frp 服务端 → 写配置 → 注册成开机自启任务 → 开防火墙 → 验证 → 告诉你下一步。
#
# 用法（在 Windows 服务器的 PowerShell 里，右键"以管理员身份运行"）：
#   powershell -ExecutionPolicy Bypass -File .\deploy-vps-windows.ps1
#   可选参数：-RelayPort 21118 -FrpVersion 0.61.1

param(
    [int]$RelayPort = 21118,
    [string]$FrpVersion = "0.61.1"
)

$ErrorActionPreference = "Stop"
$Root = "C:\rd-relay"
$Mirrors = @(
    "https://github.com",
    "https://ghfast.top/https://github.com",
    "https://mirror.ghproxy.com/https://github.com"
)

function Say($m)  { Write-Host "[OK] $m" -ForegroundColor Green }
function Step($m) { Write-Host "`n[..] $m" -ForegroundColor Cyan }
function Die($m)  { Write-Host "[X] $m" -ForegroundColor Red; exit 1 }

# 管理员检查
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { Die "请右键 PowerShell → 以管理员身份运行，再执行本脚本" }

# 1. 下载 frp
Step "1/4 下载 frp $($FrpVersion)（Windows 64 位）"
$Zip = "$env:TEMP\frp_win.zip"
$Dir = "$Root\frp_$($FrpVersion)_windows_amd64"
if (-not (Test-Path "$Dir\frps.exe")) {
    $ok = $false
    foreach ($m in $Mirrors) {
        $url = "$m/fatedier/frp/releases/download/v$FrpVersion/frp_$($FrpVersion)_windows_amd64.zip"
        Write-Host "  尝试: $url"
        try {
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -Uri $url -OutFile $Zip -UseBasicParsing -TimeoutSec 60
            Expand-Archive -Path $Zip -DestinationPath $Root -Force
            $ok = $true; break
        } catch { Write-Host "  失败，换下一个源…" }
    }
    if (-not $ok) { Die "下载失败。请手动下载 frp_windows_amd64.zip 解压到 $Root 后重跑" }
}
Say "frps.exe 就绪"

# 2. 配置
Step "2/4 写入配置"
$token = -join ((48..57) + (97..122) | Get-Random -Count 32 | ForEach-Object { [char]$_ })
$config = @"
bindAddr = "0.0.0.0"
bindPort = 7000
auth.token = "$token"
"@
New-Item -ItemType Directory -Force -Path $Root | Out-Null
Set-Content -Path "$Root\frps.toml" -Value $config -Encoding utf8
Say "配置写入 $Root\frps.toml（令牌已自动生成）"

# 3. 注册开机自启任务并立即启动
Step "3/4 注册开机自启任务 rd-relay"
$bat = "$Root\run-relay.bat"
Set-Content -Path $bat -Value "@echo off`r`n$Dir\frps.exe -c $Root\frps.toml" -Encoding ascii
schtasks /create /f /tn "rd-relay" /tr "`"$bat`"" /sc onstart /ru SYSTEM /rl highest | Out-Null
schtasks /run /tn "rd-relay" | Out-Null
Start-Sleep -Seconds 2
Say "任务注册完成并已启动"

# 4. 防火墙 + 验证
Step "4/4 防火墙放行并验证"
New-NetFirewallRule -DisplayName "rd-relay-quic" -Direction Inbound -Protocol UDP -LocalPort $RelayPort -Action Allow -ErrorAction SilentlyContinue | Out-Null
New-NetFirewallRule -DisplayName "rd-relay-frp"  -Direction Inbound -Protocol TCP -LocalPort 7000       -Action Allow -ErrorAction SilentlyContinue | Out-Null
$conn = Get-NetUDPEndpoint -LocalPort $RelayPort -ErrorAction SilentlyContinue
if (-not $conn) { Die "frps 没起来或端口没监听。看日志：任务计划程序 → rd-relay；或手动运行 $bat 看报错" }
Say "frps 运行中，端口 $RelayPort/udp 正在监听"

$ip = (Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -notlike "169.254.*" } |
    Select-Object -First 1).IPAddress

Write-Host @"

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
VPS 部署完成！记下这两个值（被控端脚本 connect-home.ps1 会用到）：
  VPS 地址 : $ip   （如是云服务器，请用控制台显示的【公网 IP】）
  中继令牌 : $token
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
"@ -ForegroundColor Yellow
