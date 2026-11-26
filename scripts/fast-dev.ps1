param(
    [ValidateSet("menu", "ui", "desktop", "sandbox", "unit", "fast-exe", "selftest")]
    [string]$Mode = "menu",
    [string]$Filter = "Lab"
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 确保 target\debug 下的 FFmpeg DLL 在运行期可直接加载
$debugDir = Join-Path $root "target\debug"
if ($env:PATH -notlike "*$debugDir*") {
    $env:PATH = "$debugDir;$env:PATH"
}

function Test-PortListening([int]$Port) {
    $conn = New-Object System.Net.Sockets.TcpClient
    try {
        $iar = $conn.BeginConnect("127.0.0.1", $Port, $null, $null)
        $ok = $iar.AsyncWaitHandle.WaitOne(250, $false)
        if ($ok -and $conn.Connected) { return $true }
        return $false
    } catch {
        return $false
    } finally {
        $conn.Close()
    }
}

function Ensure-ViteDevServer {
    if (Test-PortListening 1420) {
        Write-Host "[✓] 检测到 Vite 热更新服务已在 http://127.0.0.1:1420 运行（直接复用，0 等待）" -ForegroundColor Green
        return $null
    }
    Write-Host "[…] 正在后台启动前端 Vite 热更新服务 (http://127.0.0.1:1420)…" -ForegroundColor Cyan
    $viteProc = Start-Process -FilePath "cmd.exe" `
        -ArgumentList "/c pnpm -C apps/rd-controller dev" `
        -WorkingDirectory $root `
        -PassThru `
        -WindowStyle Minimized
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 300
        if (Test-PortListening 1420) {
            Write-Host "[✓] Vite 热更新就绪：改任意 .tsx/.css 保存即 0.1 秒无感刷新！" -ForegroundColor Green
            return $viteProc
        }
    }
    throw "Vite 启动超时，请检查 1420 端口"
}

function Stop-ChildSafely($proc) {
    if ($null -ne $proc -and -not $proc.HasExited) {
        try { $proc.Kill() } catch {}
    }
}

# 自动检测并释放本仓库 target\ 目录下仍在运行并锁住 EXE / avcodec-*.dll 的残留进程
# （例如上次关掉黑框残留的 rd-agent / rd-relay，或最小化到托盘的 rd-controller）
function Stop-WorkspaceTargetProcesses([string]$ExcludeSubdir = "") {
    $targetPrefix = (Join-Path $root "target\").ToLowerInvariant()
    $excludePrefix = if ($ExcludeSubdir) { (Join-Path $root $ExcludeSubdir).ToLowerInvariant() } else { "" }
    $procs = Get-Process -ErrorAction SilentlyContinue | Where-Object {
        $_.ProcessName -match "^(rd-controller|rd-agent|rd-relay)$" -and
        $null -ne $_.Path -and
        $_.Path.ToLowerInvariant().StartsWith($targetPrefix) -and
        (-not $excludePrefix -or -not $_.Path.ToLowerInvariant().StartsWith($excludePrefix))
    }
    if ($procs) {
        foreach ($p in $procs) {
            Write-Host "[!] 检测到残留进程占用文件锁: $($p.ProcessName) (PID=$($p.Id))，正在自动关闭以释放 DLL/EXE…" -ForegroundColor Yellow
            try { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } catch {}
        }
        Start-Sleep -Milliseconds 400
    }
}

if ($Mode -eq "menu") {
    Write-Host ""
    Write-Host "==========================================================" -ForegroundColor Cyan
    Write-Host "   RD 极速开发与功能秒测台（告别漫长打包等待）" -ForegroundColor Cyan
    Write-Host "==========================================================" -ForegroundColor Cyan
    Write-Host "  [1] 纯 Web 改界面预览     (0.1秒热更新，纯测 UI 交互最快)"
    Write-Host "  [2] 真桌面端热开发模式    (免打包！前端0.1s热刷 + Rust增量秒编)"
    Write-Host "  [3] 单机全真双机联调沙盒  (自动拉起 本地真中转 + 模拟第2台电脑[码123456+9位ID] + 控制端)"
    Write-Host "  [4] 单功能命令行秒级单测  (1~2秒跑完指定模块测试，连窗口都不用开)"
    Write-Host "  [5] 极速生成免安装裸 EXE  (跳过 LTO 与安装包，跨两台真机拷贝联调用)"
    Write-Host "  [6] 一键全自动真机全检    (无需手点！自动跑通直连+中转9位ID+STUN打洞+桌面144Hz编码)"
    Write-Host "==========================================================" -ForegroundColor Cyan
    $choice = Read-Host "请输入编号 (默认 3)"
    switch ($choice.Trim()) {
        "1" { $Mode = "ui" }
        "2" { $Mode = "desktop" }
        "4" { $Mode = "unit" }
        "5" { $Mode = "fast-exe" }
        "6" { $Mode = "selftest" }
        default { $Mode = "sandbox" }
    }
}

switch ($Mode) {
    "ui" {
        $vite = Ensure-ViteDevServer
        Start-Process "http://127.0.0.1:1420/#/lab"
        Write-Host ""
        Write-Host "已在浏览器打开 http://127.0.0.1:1420/#/lab ，直接改前端代码保存即生效！" -ForegroundColor Green
        if ($null -ne $vite) {
            Read-Host "按回车键停止后台 Vite 服务并退出"
            Stop-ChildSafely $vite
        }
    }

    "desktop" {
        $vite = $null
        try {
            Stop-WorkspaceTargetProcesses -ExcludeSubdir "target\fast-portable"
            $vite = Ensure-ViteDevServer
            Write-Host "[…] 增量编译并启动桌面端 rd-controller (Debug 热开发模式，跳过打包)…" -ForegroundColor Cyan
            cargo run -p rd-controller
        } finally {
            Stop-ChildSafely $vite
        }
    }

    "sandbox" {
        $vite = $null
        $relayProc = $null
        $agentProc = $null
        try {
            Stop-WorkspaceTargetProcesses -ExcludeSubdir "target\fast-portable"
            $vite = Ensure-ViteDevServer

            Write-Host "[1/3] 增量编译 rd-relay / rd-agent / rd-controller (复用缓存，免打包)…" -ForegroundColor Cyan
            cargo build -p rd-relay -p rd-agent -p rd-controller
            if ($LASTEXITCODE -ne 0) { throw "Cargo 增量编译失败" }

            $sandboxDir = Join-Path $root "target\dev-sandbox\host-b"
            $sandboxBin = Join-Path $root "target\dev-sandbox\bin"
            New-Item -ItemType Directory -Force -Path $sandboxDir | Out-Null
            New-Item -ItemType Directory -Force -Path $sandboxBin | Out-Null
            $outLog = Join-Path $sandboxDir "stdout.txt"
            if (Test-Path $outLog) { Remove-Item $outLog -Force }

            # 将后台沙盒进程复制到独立目录运行，彻底避免锁死 target\debug\avcodec-*.dll！
            Copy-Item (Join-Path $debugDir "rd-relay.exe") $sandboxBin -Force
            Copy-Item (Join-Path $debugDir "rd-agent.exe") $sandboxBin -Force
            Get-ChildItem -Path $debugDir -Filter "*.dll" -ErrorAction SilentlyContinue | ForEach-Object {
                Copy-Item $_.FullName $sandboxBin -Force
            }

            Write-Host "[2/3] 启动本地真中转 rd-relay (127.0.0.1:21117) 与模拟第2台电脑 Host-B…" -ForegroundColor Cyan
            $relayExe = Join-Path $sandboxBin "rd-relay.exe"
            $agentExe = Join-Path $sandboxBin "rd-agent.exe"
            $ctrlExe  = Join-Path $debugDir "rd-controller.exe"

            $relayProc = Start-Process -FilePath $relayExe `
                -ArgumentList "127.0.0.1:21117" `
                -WorkingDirectory $sandboxBin `
                -PassThru `
                -WindowStyle Hidden

            Start-Sleep -Milliseconds 300

            $env:RD_RELAY = "127.0.0.1:21117"
            $agentProc = Start-Process -FilePath $agentExe `
                -ArgumentList "run --config-dir `"$sandboxDir`" --listen 127.0.0.1:21128 --no-tray --print-port --test-pair-code 123456" `
                -WorkingDirectory $sandboxBin `
                -RedirectStandardOutput $outLog `
                -PassThru `
                -WindowStyle Hidden
            Remove-Item Env:\RD_RELAY -ErrorAction SilentlyContinue

            $hostNumericId = "读取中..."
            for ($i = 0; $i -lt 25; $i++) {
                Start-Sleep -Milliseconds 200
                if (Test-Path $outLog) {
                    $txt = Get-Content $outLog -Raw -ErrorAction SilentlyContinue
                    if ($txt -match "RD_NUMERIC_ID=([0-9 ]+)") {
                        $hostNumericId = $Matches[1].Trim()
                        break
                    }
                }
            }

            Write-Host ""
            Write-Host "╔══════════════════════════════════════════════════════════════════╗" -ForegroundColor Green
            Write-Host "║  🎉 单机双端全真沙盒已就绪！（前端改代码 0.1s 实时热刷）        ║" -ForegroundColor Green
            Write-Host "╠══════════════════════════════════════════════════════════════════╣" -ForegroundColor Green
            Write-Host "║  ▶ 测【局域网直连】：目标填  127.0.0.1:21128   配对码  123456    ║" -ForegroundColor Yellow
            Write-Host "║  ▶ 测【自建中转9位ID】：                                         ║" -ForegroundColor Yellow
            Write-Host "║      第1步中转地址填：127.0.0.1:21117                            ║" -ForegroundColor Yellow
            Write-Host "║      第3步对方9位ID填：$hostNumericId        配对码  123456    ║" -ForegroundColor Yellow
            Write-Host "║  ▶ 测【P2P 打洞】：左栏点生成打洞码 → 直接粘到右栏点连接即可！   ║" -ForegroundColor Yellow
            Write-Host "╚══════════════════════════════════════════════════════════════════╝" -ForegroundColor Green
            Write-Host ""

            Write-Host "[3/3] 正在打开 rd-controller 桌面窗口（关闭窗口即自动清理后台沙盒进程）…" -ForegroundColor Cyan
            & $ctrlExe
        } finally {
            Stop-ChildSafely $agentProc
            Stop-ChildSafely $relayProc
            Stop-ChildSafely $vite
            Write-Host "[✓] 已清理沙盒后台进程。" -ForegroundColor DarkGray
        }
    }

    "unit" {
        if ($Filter -eq "Lab" -and $args.Count -eq 0) {
            $inp = Read-Host "输入要快测的关键词（如 Lab / Devices / Settings / stun，回车默认 Lab）"
            if ($inp.Trim()) { $Filter = $inp.Trim() }
        }
        if ($Filter -match "^(stun|relay|easytier)$") {
            cargo test -p rd-controller --lib $Filter
        } else {
            pnpm -C apps/rd-controller test -- $Filter
        }
    }

    "fast-exe" {
        Stop-WorkspaceTargetProcesses -ExcludeSubdir "target\fast-portable"
        Write-Host "[…] 正在极速构建内嵌前端的免安装裸 EXE（跳过缓慢 LTO 与 NSIS/MSI 安装包）…" -ForegroundColor Cyan
        pnpm -C apps/rd-controller tauri:fast-exe
        if ($LASTEXITCODE -ne 0) { throw "fast-exe 构建失败" }

        $portableDir = Join-Path $root "target\fast-portable"
        New-Item -ItemType Directory -Force -Path $portableDir | Out-Null
        Copy-Item (Join-Path $debugDir "rd-controller.exe") $portableDir -Force -ErrorAction SilentlyContinue
        if (Test-Path (Join-Path $debugDir "rd-agent.exe")) {
            Copy-Item (Join-Path $debugDir "rd-agent.exe") $portableDir -Force -ErrorAction SilentlyContinue
        }
        if (Test-Path (Join-Path $debugDir "rd-relay.exe")) {
            Copy-Item (Join-Path $debugDir "rd-relay.exe") $portableDir -Force -ErrorAction SilentlyContinue
        }
        Get-ChildItem -Path $debugDir -Filter "*.dll" -ErrorAction SilentlyContinue | ForEach-Object {
            Copy-Item $_.FullName $portableDir -Force -ErrorAction SilentlyContinue
        }
        Write-Host ""
        Write-Host "[✓] 构建完成！已自动整理免安装便携目录（直接整夹拷走或本地双击均不会锁死下次编译）：" -ForegroundColor Green
        Write-Host "    👉 $portableDir\rd-controller.exe" -ForegroundColor Yellow
    }

    "selftest" {
        Stop-WorkspaceTargetProcesses -ExcludeSubdir "target\fast-portable"
        Write-Host "[…] 正在执行【一台电脑测遍】全自动真机集成自检（直连配对 + 完整桌面144Hz硬软编 + 自建中转9位ID + STUN打洞）…" -ForegroundColor Cyan
        cargo test -p rd-agent --test live_single_pc_full -- --nocapture
    }
}
