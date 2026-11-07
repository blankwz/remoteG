## license-ledger.ps1 — T-041 半自动发码台账（量 <100/月 不做全自动，路线图既定）
##
## 用法（发一单）：
##   .\scripts\license-ledger.ps1 -Order "TD20261107001" -Email "buyer@example.com" `
##       -Plan pro -Devices 99 -ExpiresDays 365
##   （-ExpiresDays 省略 = 永久码）
##
## 种子来源（按序）：
##   1) 环境变量 RD_LICENSE_SEED（正式签发，推荐）
##   2) -SeedFile 指定文件（首行 64 位 hex）
##   缺两者则报错退出（**绝不使用仓库内开发种子发正式码**）。
##
## 台账 CSV 列：date, order, email, name, plan, devices, expires, code
##   默认 %USERPROFILE%\.rd-secrets\licenses.csv（敏感业务数据，**不进仓库**；
##   可用 -Ledger 覆盖路径）。Set-Clipboard 把授权码放剪贴板，回邮件直接粘贴。
##
## 退出码：0 成功；2 参数缺失；3 签发/自验失败。

param(
  [Parameter(Mandatory = $true)][string]$Order,
  [Parameter(Mandatory = $true)][string]$Email,
  [string]$Name = "",
  [ValidateSet("pro", "free")][string]$Plan = "pro",
  [int]$Devices = 99,
  [int]$ExpiresDays = 0,
  [string]$SeedFile = "",
  [string]$Ledger = ""
)
$ErrorActionPreference = "Stop"

# ---- 种子解析：env 优先，其次 SeedFile；都没有 = 拒绝发码 -----------------
$seed = $env:RD_LICENSE_SEED
if (-not $seed) {
  if (-not $SeedFile) {
    # 兼容：默认找本机密钥目录的授权种子文件
    $SeedFile = Join-Path $env:USERPROFILE ".rd-secrets\license-seed.txt"
  }
  if (-not (Test-Path $SeedFile)) {
    Write-Error "缺少签名种子：设 RD_LICENSE_SEED 或提供 -SeedFile（绝不用开发种子发正式码）"
    exit 2
  }
  $seed = (Get-Content $SeedFile -TotalCount 1).Trim()
}
if ($seed -notmatch '^[0-9a-fA-F]{64}$') {
  Write-Error "种子格式不对（需 64 位 hex）"
  exit 2
}

# ---- 签发（issue_license 自带「签发即验」：种子与内嵌公钥不配对会当场失败）--
$repo = Split-Path -Parent $PSScriptRoot
$env:RD_LICENSE_SEED = $seed
$issueArgs = @(
  "run", "-q", "-p", "rd-security", "--example", "issue_license", "--",
  "--name", $Name, "--plan", $Plan, "--devices", "$Devices"
)
if ($ExpiresDays -gt 0) {
  $issueArgs += @("--expires-days", "$ExpiresDays")
}
Write-Host "签发中…（issue_license 内置签发后自验）"
# PS 5.1 下 stderr 重定向（2>&1）+ Stop 偏好会把 cargo 的 stderr 输出
# （✓ 自验行）误判为终止错误——此处临时放宽，靠 RD1./plan= 匹配判定成败
$ErrorActionPreference = "Continue"
$output = & cargo @issueArgs 2>&1
$ErrorActionPreference = "Stop"
$code = ($output | Where-Object { $_ -like "RD1.*" } | Select-Object -First 1)
$okLine = ($output | Where-Object { $_ -like "*plan=*" } | Select-Object -First 1)
if (-not $code -or -not $okLine) {
  Write-Error "签发或自验失败，未写台账。输出：$output"
  exit 3
}
Write-Host $okLine

# ---- 台账：追加一行（敏感数据默认仓库外；文本字段加引号防逗号破坏列）------
if (-not $Ledger) {
  $Ledger = Join-Path $env:USERPROFILE ".rd-secrets\licenses.csv"
}
$expiresText = if ($ExpiresDays -gt 0) { (Get-Date).AddDays($ExpiresDays).ToString("yyyy-MM-dd") } else { "permanent" }
$row = '"{0}","{1}","{2}","{3}","{4}",{5},"{6}","{7}"' -f `
  (Get-Date -Format "yyyy-MM-dd HH:mm"), $Order, $Email, $Name, $Plan, $Devices, $expiresText, $code
Add-Content -Path $Ledger -Value $row -Encoding utf8

# ---- 回邮草稿 + 剪贴板 -----------------------------------------------------
Set-Clipboard -Value $code
$draft = @"
您好，

您购买的 rd 专业版授权码如下：

$code

激活方法：把上面整串文本保存为文件 license.key（UTF-8 纯文本），
放到「被控电脑」的 %LOCALAPPDATA%\rd\ 目录下，重启 rd 即生效。
有效期：$expiresText

如有问题请直接回复本邮件。
"@
Write-Host "`n=== 回邮草稿（授权码已复制到剪贴板）===" -ForegroundColor Green
Write-Host $draft
Write-Host "=== 台账已追加：$Ledger ===" -ForegroundColor Green
