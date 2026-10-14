# package-agent.ps1 - Package Windows agent portable zip (T-026)
# Usage: powershell -ExecutionPolicy Bypass -File scripts/package-agent.ps1
# Prereq: FFMPEG_DIR points to a BtbN LGPL shared build (bin\*.dll + LICENSE); Rust stable + MSVC.
# Output: dist/rd-agent-<version>-win64.zip (exe + LGPL DLLs + LICENSE, portable)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot | Split-Path
Set-Location $root

if (-not $env:FFMPEG_DIR) { throw "FFMPEG_DIR is not set (BtbN LGPL shared build dir containing bin\*.dll)" }
$ffmpegBin = Join-Path $env:FFMPEG_DIR "bin"
if (-not (Test-Path $ffmpegBin)) { throw "No bin directory under FFMPEG_DIR: $ffmpegBin" }

# Version from workspace Cargo.toml
$cargoToml = Get-Content "$root\Cargo.toml" -Raw
$version = [regex]::Match($cargoToml, '(?m)^\s*version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $version) { throw "Cannot parse version from Cargo.toml" }

Write-Host "== cargo build -p rd-agent --release =="
cargo build -p rd-agent --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = "$root\dist\rd-agent-$version-win64"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item $stage -ItemType Directory | Out-Null

Copy-Item "$root\target\release\rd-agent.exe" $stage

# Ship ffmpeg LGPL DLLs with the package (10_devops: LICENSE attached, no libx264)
Copy-Item "$ffmpegBin\*.dll" $stage
$license = Get-ChildItem "$env:FFMPEG_DIR" -Filter "LICENSE*" -File | Select-Object -First 1
if ($license) { Copy-Item $license.FullName $stage } else { Write-Warning "ffmpeg LICENSE file not found" }

$zip = "$root\dist\rd-agent-$version-win64.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $stage -DestinationPath $zip
Write-Host "== Output: $zip =="
Get-Item $zip | Select-Object Name, Length
