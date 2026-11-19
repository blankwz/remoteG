# rd-mirror.ps1 — 国内渠道手动补传（改造项 B）。
#
# 从 GitHub Releases 拉产物 → 内容寻址校验 → 上传 COS；latest 指针防回滚。
# 真正的逻辑在 rd-pack-cli mirror 子命令（rd-update-pack crate，feature
# mirror），本脚本只做密钥交接与调用包装。
#
# 用法：
#   powershell -File scripts/rd-mirror.ps1 -Tag v0.3.0
#     （需 RD_COS_CONFIG / RD_COS_KEYS 环境变量，或 -CosConfigPath/-CosKeysPath）
#   powershell -File scripts/rd-mirror.ps1 -Tag v0.3.0 -CosConfigPath cfg.json -CosKeysPath keys.json
#
# 密钥纪律（对齐 T-036）：secretId/secretKey 绝不进仓库、不进命令行参数
# （进程列表可见）；只经环境变量或 @文件 交接，脚本退出即删临时文件。

param(
    [Parameter(Mandatory = $true)]
    [string]$Tag,

    # COS 桶配置 JSON（{"host":"bucket-appid.cos.region.myqcloud.com","prefix":"rd-update"}）
    [string]$CosConfigPath = "",

    # COS 密钥 JSON（{"secret_id":"...","secret_key":"..."}）——建议凭据管理释放的临时文件
    [string]$CosKeysPath = ""
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo 不在 PATH；请先安装 Rust 工具链"
}

# 密钥交接：优先显式文件；否则环境变量落临时文件（退出即删）
$tmpDir = Join-Path $env:TEMP ("rd-cos-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmpDir | Out-Null
try {
    if (-not $CosConfigPath) {
        if (-not $env:RD_COS_CONFIG) { Write-Error "缺 COS 配置：传 -CosConfigPath 或设 RD_COS_CONFIG 环境变量" }
        $CosConfigPath = Join-Path $tmpDir "cos-config.json"
        Set-Content -Path $CosConfigPath -Value $env:RD_COS_CONFIG -NoNewline
    }
    if (-not $CosKeysPath) {
        if (-not $env:RD_COS_KEYS) { Write-Error "缺 COS 密钥：传 -CosKeysPath 或设 RD_COS_KEYS 环境变量（建议凭据管理释放临时文件）" }
        $CosKeysPath = Join-Path $tmpDir "cos-keys.json"
        Set-Content -Path $CosKeysPath -Value $env:RD_COS_KEYS -NoNewline
    }

    cargo run -q -p rd-update-pack --features mirror --bin rd-pack-cli -- mirror `
        --tag $Tag `
        --cos "@$CosConfigPath" --cos-keys "@$CosKeysPath"
}
finally {
    Remove-Item $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
