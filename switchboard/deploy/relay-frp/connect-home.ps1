# rd 一键中继·被控端接线（在被控的那台 Windows 电脑上运行——就是家人用的那台）
#
# 做什么：下载 frp 客户端 → 把"VPS 的 21118 端口"接到本机 rd 的 21118 端口 →
#         注册开机自启 → 验证连上 VPS → 告诉你在控制端填什么地址。
#
# 用法（PowerShell，普通权限即可；推荐右键"以管理员身份运行"以注册自启）：
#   powershell -ExecutionPolicy Bypass -File .\connect-home.ps1 -Server 1.2.3.4 -Token 上一步的令牌
#
# 上一步（deploy-vps.sh / deploy-vps-windows.ps1）生成的 .relay-info 里有这两个值；
# 脚本与本文件同目录时会自动读取，不用手抄。

param(
    [string]$Server = "",
    [string]$Token = "",
    [int]$RelayPort = 21118,
    [int]$LocalPort = 21118,
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

# 自动读取同目录 .relay-info（deploy-vps.sh 生成的连接信息）
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if ((-not $Server -or -not $Token) -and (Test-Path "$here\.relay-info")) {
    Get-Content "$here\.relay-info" | ForEach-Object {
        if ($_ -match "^VPS=(.+)$")     { if (-not $Server) { $Server = $Matches[1] } }
        if ($_ -match "^TOKEN=(.+)$")   { if (-not $Token)  { $Token = $Matches[1] } }
        if ($_ -match "^PORT=(.+)$")    { $RelayPort = [int]$Matches[1] }
    }
    Say "已从 .relay-info 自动读取连接信息"
}
if (-not $Server -or -not $Token) {
    Die "缺参数。用法: .\connect-home.ps1 -Server VPS的IP -Token 部署时显示的令牌
（VPS 是 Linux 部署时，.relay-info 文件就在本脚本旁边，会自动读取）"
}

# 1. 前置检查：rd 被控端在跑吗
Step "0/3 检查本机 rd 被控端（端口 $LocalPort）"
$rdListen = Get-NetUDPEndpoint -LocalPort $LocalPort -ErrorAction SilentlyContinue
if (-not $rdListen) {
    Write-Host "  ⚠ 本机 $LocalPort 端口没在监听——rd 被控端好像没在跑。"
    Write-Host "    请先启动 rd（托盘有 rd 图标），或本脚本所在仓库 target\release\rd-agent.exe"
    $answer = Read-Host "  已启动好了就输入 y 继续，其他输入退出"
    if ($answer -ne "y") { exit 1 }
} else {
    Say "rd 被控端在跑"
}

# 2. 下载 frpc
Step "1/3 下载 frp 客户端"
$Zip = "$env:TEMP\frp_win.zip"
$Dir = "$Root\frp_$($FrpVersion)_windows_amd64"
if (-not (Test-Path "$Dir\frpc.exe")) {
    $ok = $false
    foreach ($m in $Mirrors) {
        $url = "$m/fatedier/frp/releases/download/v$FrpVersion/frp_$($FrpVersion)_windows_amd64.zip"
        try {
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -Uri $url -OutFile $Zip -UseBasicParsing -TimeoutSec 60
            Expand-Archive -Path $Zip -DestinationPath $Root -Force
            $ok = $true; break
        } catch { }
    }
    if (-not $ok) { Die "下载失败。请手动下载 frp_windows_amd64.zip 解压到 $Root 后重跑" }
}
Say "frpc.exe 就绪"

# 3. 配置 + 自启
Step "2/3 写配置并注册开机自启"
$config = @"
serverAddr = "$Server"
serverPort = 7000
auth.token = "$Token"

[[proxies]]
name = "rd-quic"
type = "udp"
localIP = "127.0.0.1"
localPort = $LocalPort
remotePort = $RelayPort
"@
Set-Content -Path "$Root\frpc.toml" -Value $config -Encoding utf8
$bat = "$Root\run-home.bat"
Set-Content -Path $bat -Value "@echo off`r`n$Dir\frpc.exe -c $Root\frpc.toml" -Encoding ascii
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($isAdmin) {
    schtasks /create /f /tn "rd-relay-home" /tr "`"$bat`"" /sc onstart /ru SYSTEM /rl highest | Out-Null
    Say "开机自启已注册（任务名 rd-relay-home）"
} else {
    Write-Host "  ⚠ 非管理员：跳过开机自启。想开机自动接通，右键管理员重跑一次本脚本"
}
Start-Process -FilePath $bat -WindowStyle Hidden

# 4. 验证
Step "3/3 验证与 VPS 的连接"
Start-Sleep -Seconds 3
$log = & "$Dir\frpc.exe" verify -c "$Root\frpc.toml" 2>&1
if ($LASTEXITCODE -ne 0) { Die "frpc 配置校验失败：$log" }
$probe = Test-NetConnection -ComputerName $Server -Port 7000 -WarningAction SilentlyContinue
if ($probe.TcpTestSucceeded) {
    Say "能连上 VPS（7000 端口通）——中继链路已打通"
} else {
    Die "连不上 VPS 的 7000 端口。检查：VPS 安全组放行 TCP 7000 / UDP $RelayPort 了吗？IP 对吗？"
}

Write-Host @"

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
全部完成！最后一步（在【控制端电脑】上）：
  控制端 → 设备 → 添加设备 → 地址填：

      ${Server}:${RelayPort}

  （配对码照常在被控端托盘生成；连接会走中继，
   画面上会多几十毫秒延迟，属正常）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
"@ -ForegroundColor Yellow
