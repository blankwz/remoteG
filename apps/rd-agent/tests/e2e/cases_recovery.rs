//! t12/t13/t14（手测清单 §5.5/§5.6/§5.7 自动化）：隐私屏重启恢复、挂死窗口
//! 探针、窗口账本崩溃还原。
//!
//! 三个用例都自带独立进程（agent / testwin，tempdir config-dir），不注入
//! 物理鼠标键盘输入，可在任意时段运行。视觉影响：t12 的黑罩窗口全屏置顶
//! 数秒（用例结束杀进程即消）；t14 的独立 testwin 会被挪动位置。

use std::mem;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use rd_proto::v0::envelope::Msg;
use rd_proto::v0::{FitMode, OpenWindow, RenderLevel, SetPrivacy};
use tempfile::TempDir;

use crate::common::{self, Client};

// ---------------------------------------------------------------------------
// t12 隐私屏重启恢复（§5.5，改造项 E3）
// ---------------------------------------------------------------------------

/// 隐私屏黑罩窗口类名（privacy::shield_thread 注册）。
const SHIELD_CLASS: &str = "rd-privacy-shield";
/// 隐私屏状态文件名（privacy::persist_state）。
const PRIVACY_STATE_FILE: &str = "rd-privacy-state.json";

/// A：会话内开隐私屏（状态落盘）→ 强杀；B：重启 → 黑窗自动恢复 → 关闭
/// （OFF 落盘）；C：OFF 对照重启 → 无黑窗。
#[tokio::test(flavor = "multi_thread")]
async fn t12_privacy_screen_restart_recover() -> Result<()> {
    let dir = TempDir::new().context("临时目录创建失败")?;
    let cfg = dir.path().join("cfg");
    std::fs::create_dir(&cfg).context("创建 config-dir 失败")?;
    common::write_pro_license(&cfg)?;
    let _cleanup = common::AgentCleanup(cfg.clone());

    // ── A：开隐私屏 ──
    let (mut a, addr_a) = spawn_silent_notify_agent(&cfg)?;
    let pid_a = a.child.id();
    let mut c = Client::connect_to(addr_a).await?;
    let reply = c
        .call(Msg::SetPrivacy(SetPrivacy {
            window_id: 0,
            on: true,
        }))
        .await?;
    let Some(Msg::SetPrivacy(echo)) = reply.msg else {
        bail!("期望 SetPrivacy echo，实际 {:?}", reply.msg);
    };
    ensure!(echo.on, "SetPrivacy(on=true) 的 echo 应为 on=true");
    // 黑窗异步生效（shield 线程 200ms 收请求）：轮询可见（≤5s）
    ensure!(
        wait_shield_visible(pid_a, Duration::from_secs(5)).await,
        "开启隐私屏后 5s 内黑罩窗口未变为可见"
    );
    // 状态落盘（受理即记）
    let state_path = cfg.join(PRIVACY_STATE_FILE);
    let body = wait_for_file(&state_path, Duration::from_secs(3))
        .await
        .with_context(|| format!("状态文件未落盘: {}", state_path.display()))?;
    ensure!(
        body == b"{\"on\":true}",
        "状态文件应为 on:true，实际 {body:?}"
    );
    println!("[t12] A: 隐私屏已开启且落盘，强杀进程（模拟崩溃）");
    // 强杀（模拟崩溃，非 graceful）
    let _ = a.child.kill();
    a.child.wait()?;
    drop(c);

    // ── B：重启 → 黑窗自动恢复（E3 回看恢复）→ 关闭（OFF 落盘）──
    let (mut b, addr_b) = spawn_silent_notify_agent(&cfg)?;
    let pid_b = b.child.id();
    ensure!(
        wait_shield_visible(pid_b, Duration::from_secs(5)).await,
        "重启后 5s 内黑罩窗口未自动恢复（E3 回看恢复失效）"
    );
    println!("[t12] B: 重启后黑窗已恢复，关闭隐私屏");
    let mut c2 = Client::connect_to(addr_b).await?;
    let reply = c2
        .call(Msg::SetPrivacy(SetPrivacy {
            window_id: 0,
            on: false,
        }))
        .await?;
    let Some(Msg::SetPrivacy(echo)) = reply.msg else {
        bail!("期望 SetPrivacy echo，实际 {:?}", reply.msg);
    };
    ensure!(!echo.on, "SetPrivacy(on=false) 的 echo 应为 on=false");
    ensure!(
        wait_shield_hidden(pid_b, Duration::from_secs(5)).await,
        "关闭隐私屏后 5s 内黑罩窗口未隐藏"
    );
    let body = wait_for_stable_state(&cfg, b"{\"on\":false}").await?;
    ensure!(
        body == b"{\"on\":false}",
        "关闭后状态文件应为 on:false，实际 {body:?}"
    );
    // 强杀（与 A 同为崩溃语义，但状态已是 OFF）
    let _ = b.child.kill();
    b.child.wait()?;
    drop(c2);

    // ── C：OFF 对照：重启后不出现黑窗 ──
    let (mut cc, _addr_c) = spawn_silent_notify_agent(&cfg)?;
    let pid_c = cc.child.id();
    // recover_state 读 OFF → 不恢复；给足启动+恢复窗口，黑窗须始终隐藏
    ensure!(
        !wait_shield_visible(pid_c, Duration::from_secs(3)).await,
        "OFF 状态重启后黑罩窗口不应出现"
    );
    // 黑罩线程已起（窗口创建即存在），只是隐藏态——更有信息量的对照
    ensure!(
        !find_shield_windows(pid_c).is_empty(),
        "黑罩窗口应已创建（隐藏态常驻）——shield 线程未起？"
    );
    let _ = cc.child.kill();
    cc.child.wait()?;

    println!("t12 ok: ON 崩溃重启恢复 ✓ / OFF 崩溃重启不恢复 ✓（关闭状态同样持久化）");
    Ok(())
}

/// 隐私屏用例的独立 Agent：知情提示直通（`RD_TEST_SILENT_NOTIFY=1`，debug
/// 钩子）——无人值守环境弹窗会挂在无人点击的桌面上。
fn spawn_silent_notify_agent(cfg: &Path) -> Result<(common::AgentProc, std::net::SocketAddr)> {
    let (agent, addr, _rx) = common::spawn_agent_custom(
        Path::new(env!("CARGO_BIN_EXE_rd-agent")),
        cfg,
        &[("RD_TEST_SILENT_NOTIFY", "1")],
        &["--no-tray", "--listen", "127.0.0.1:0", "--print-port"],
    )?;
    Ok((agent, addr))
}

/// 枚举顶层窗口，返回指定 pid 下类名为 [`SHIELD_CLASS`] 的 (hwnd, 可见)。
/// 共享 env 或其他用例的 Agent 黑窗按 pid 天然隔离。
fn find_shield_windows(pid: u32) -> Vec<(isize, bool)> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsWindowVisible,
    };

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<(isize, bool)>) };
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        out.push((hwnd.0 as isize, visible));
        true.into()
    }

    let mut all: Vec<(isize, bool)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    let mut hits = Vec::new();
    for (h, visible) in all {
        let h = HWND(h as *mut _);
        let mut wpid = 0u32;
        unsafe {
            GetWindowThreadProcessId(h, Some(&mut wpid));
        }
        if wpid != pid {
            continue;
        }
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(h, &mut buf) } as usize;
        let class = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        if class == SHIELD_CLASS {
            hits.push((h.0 as isize, visible));
        }
    }
    hits
}

/// 轮询直到黑罩窗口可见（E3 恢复生效 / On 生效）。
async fn wait_shield_visible(pid: u32, timeout: Duration) -> bool {
    common::wait_for(timeout, Duration::from_millis(150), || {
        find_shield_windows(pid).iter().any(|(_, v)| *v)
    })
    .await
}

/// 轮询直到黑罩窗口全部不可见（Off 生效）。
async fn wait_shield_hidden(pid: u32, timeout: Duration) -> bool {
    common::wait_for(timeout, Duration::from_millis(150), || {
        find_shield_windows(pid).iter().all(|(_, v)| !v)
    })
    .await
}

/// 轮询读文件直到出现（返回内容）；超时返回 Err。
async fn wait_for_file(path: &Path, timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(body) = std::fs::read(path) {
            return Ok(body);
        }
        ensure!(Instant::now() < deadline, "等待 {} 超时", path.display());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 轮询直到状态文件内容等于期望（两次写之间有异步窗口）。
async fn wait_for_stable_state(cfg: &Path, want: &[u8]) -> Result<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(body) = std::fs::read(cfg.join(PRIVACY_STATE_FILE)) {
            if body == want {
                return Ok(body);
            }
        }
        ensure!(Instant::now() < deadline, "状态文件未变为 {want:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// t13 挂死窗口探针（§5.6，改造项 E4）
// ---------------------------------------------------------------------------

/// testwin 进程被 `NtSuspendProcess` 挂起（消息循环冻结）后，`fit_window`
/// 必须在响应性探针超时窗口（RESPONSIVE_TIMEOUT_MS=1000）内返回「窗口无
/// 响应」错误而非阻塞——挂死窗口的 SetWindowPos 同步阻塞会拖死整条会话。
/// 本用例不注入物理输入，可在任意时段跑。
#[tokio::test(flavor = "multi_thread")]
async fn t13_hung_window_probe() -> Result<()> {
    // 独立 testwin（不碰共享 env 的那份；挂起不能殃及其他用例）
    let mut tw = common::spawn_testwin_proc()?;
    let pid = tw.child.id();
    let hwnd = common::wait_testwin_window(pid)?;

    // 对照：健康窗口 fit 正常返回（探针不误伤）
    let hwnd_health = hwnd;
    let fit_ok = tokio::task::spawn_blocking(move || {
        rd_window_win::resize::fit_window(hwnd_health as isize, 400, 300)
    })
    .await
    .context("spawn_blocking 失败")?
    .context("健康窗口 fit_window 不应失败")?;
    println!("[t13] 健康窗口 fit: {fit_ok:?}");

    // 挂起 testwin（ntdll NtSuspendProcess）
    suspend_resume_process(pid, true).context("NtSuspendProcess 失败")?;

    // fit 必须在探针超时窗口内返回「窗口无响应」
    let hwnd_hung = hwnd;
    let started = Instant::now();
    let res = tokio::task::spawn_blocking(move || {
        rd_window_win::resize::fit_window(hwnd_hung as isize, 500, 400)
    })
    .await
    .context("spawn_blocking 失败")?;
    let elapsed = started.elapsed();
    let Err(err) = res else {
        bail!("挂死窗口的 fit_window 竟然成功（响应性探针未拦截）");
    };
    let msg = format!("{err:#}");
    ensure!(msg.contains("窗口无响应"), "错误应含「窗口无响应」: {msg}");
    ensure!(
        elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(5),
        "fit_window 应在探针超时（1s）附近返回而非阻塞/秒拒，实际 {elapsed:?}"
    );

    // 环境还原（进程随后被杀；resume 失败无碍）
    let _ = suspend_resume_process(pid, false);
    let _ = tw.child.kill();
    tw.child.wait()?;
    println!("t13 ok: 挂死窗口 fit_window 在 {elapsed:?} 返回「窗口无响应」（会话不被拖死）");
    Ok(())
}

/// ntdll `NtSuspendProcess` / `NtResumeProcess`（动态解析——不进 windows
/// crate 公开面）。挂起后目标进程全部线程冻结、消息循环停摆。
fn suspend_resume_process(pid: u32, suspend: bool) -> Result<()> {
    use windows::core::PCSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME};

    let module = unsafe { GetModuleHandleW(windows::core::w!("ntdll.dll")) }
        .context("取 ntdll 模块句柄失败")?;
    let name: &[u8] = if suspend {
        b"NtSuspendProcess\0"
    } else {
        b"NtResumeProcess\0"
    };
    let addr = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }
        .with_context(|| format!("GetProcAddress({}) 失败", String::from_utf8_lossy(name)))?;
    let f: unsafe extern "system" fn(HANDLE) -> i32 = unsafe { mem::transmute(addr) };
    let h = unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, false, pid) }
        .with_context(|| format!("OpenProcess(PROCESS_SUSPEND_RESUME, pid {pid}) 失败"))?;
    let rc = unsafe { f(h) };
    unsafe {
        let _ = CloseHandle(h);
    }
    ensure!(
        rc == 0,
        "NtSuspend/ResumeProcess 返回 {rc:#x}（非 STATUS_SUCCESS）"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// t14 窗口布局崩溃还原（§5.7，改造项 E1）
// ---------------------------------------------------------------------------

/// 窗口账本文件名（rd-window-win::ledger::OperationLedger）。
const LEDGER_FILE: &str = "rd-window-ledger.json";

/// A：会话打开窗口（记账 = 会话前位置）→ 挪位 → 强杀 → B 重启 → 窗口回记账
/// 位置（账本清空）；C：正常断开（销账 + 恢复）→ 挪位 → 强杀 → E 重启 →
/// 窗口不动（已销账，无事可还原）。
#[tokio::test(flavor = "multi_thread")]
async fn t14_ledger_crash_restore() -> Result<()> {
    // 对照路径需要完整管线建立（断开 → restore + clear），WGC 不可用时跳过
    if crate::skip_if_no_wgc() {
        return Ok(());
    }
    let dir = TempDir::new().context("临时目录创建失败")?;
    let cfg = dir.path().join("cfg");
    std::fs::create_dir(&cfg).context("创建 config-dir 失败")?;
    common::write_pro_license(&cfg)?;
    let _cleanup = common::AgentCleanup(cfg.clone());

    let tw = common::spawn_testwin_proc()?;
    let hwnd = common::wait_testwin_window(tw.child.id())?;
    let h = hwnd as isize;

    // 摆到已知位置（普通态外框）
    move_window(h, 300, 220)?;
    let want = placement_ltrb(h)?;

    // ── A：会话打开窗口 → 触发记账 ──
    let (mut a, addr_a) = spawn_plain_agent(&cfg)?;
    let mut c = Client::connect_to(addr_a).await?;
    open_window_quiet(&mut c, hwnd).await?;
    let entry = wait_ledger_entry(&cfg, hwnd).await?;
    ensure!(
        entry.normal == want,
        "账本应记会话前位置 {want:?}，实际 {:?}",
        entry.normal
    );
    println!("[t14] A: 已记账 {:?}，会话中挪位 (60,40)", entry.normal);

    // 会话中挪位（等效远程挪动；SetWindowPos 由测试进程直调，窗口状态全局）
    move_window(h, 60, 40)?;

    // 强杀（无断开 → 无销账、无 restore）
    let _ = a.child.kill();
    a.child.wait()?;
    drop(c);

    // ── B：重启 → 启动恢复 → 窗口回记账位置 ──
    let (mut b, _addr_b) = spawn_plain_agent(&cfg)?;
    wait_placement(h, &want).await?;
    wait_ledger_empty(&cfg).await?;
    println!("[t14] B: 崩溃重启后窗口已回记账位置 {want:?}，账本已清空");
    let _ = b.child.kill();
    b.child.wait()?;

    // ── C：正常路径对照（销账后崩溃重启不还原）──
    move_window(h, 520, 300)?;
    let p2 = placement_ltrb(h)?;
    let (mut d, addr_d) = spawn_plain_agent(&cfg)?;
    let mut c2 = Client::connect_to(addr_d).await?;
    open_window_quiet(&mut c2, hwnd).await?;
    let entry2 = wait_ledger_entry(&cfg, hwnd).await?;
    ensure!(
        entry2.normal == p2,
        "第二轮账本应记 {:?}，实际 {:?}",
        p2,
        entry2.normal
    );
    // 正常断开（不杀进程）→ on_disconnect → restore + clear
    c2.close().await;
    wait_ledger_empty(&cfg).await?;
    println!("[t14] C: 正常断开已销账，挪位后强杀");

    // 用户自己挪走（会话外操作，无记账）→ 崩溃
    move_window(h, 700, 500)?;
    let p3 = placement_ltrb(h)?;
    ensure!(
        p3 != p2 && p3 != want,
        "挪位应产生与账本不同的位置（测试前提）"
    );
    let _ = d.child.kill();
    d.child.wait()?;

    // ── E：重启 → 不还原（已销账，无事可做）──
    let (mut e, _addr_e) = spawn_plain_agent(&cfg)?;
    // 给启动恢复一个稳定窗口：位置必须保持 p3
    tokio::time::sleep(Duration::from_secs(3)).await;
    let now = placement_ltrb(h)?;
    ensure!(
        now == p3,
        "已销账窗口不得被启动恢复挪动：应为 {p3:?}，实际 {now:?}"
    );
    ensure!(ledger_entries(&cfg)?.is_empty(), "E 重启后账本应为空");
    let _ = e.child.kill();
    e.child.wait()?;

    println!("t14 ok: 崩溃还原 ✓（B 回记账位置）/ 正常销账不动 ✓（E 保持挪后位置）");
    Ok(())
}

/// t14 的独立 Agent（无环境变量钩子）。
fn spawn_plain_agent(cfg: &Path) -> Result<(common::AgentProc, std::net::SocketAddr)> {
    let (agent, addr, _rx) = common::spawn_agent_custom(
        Path::new(env!("CARGO_BIN_EXE_rd-agent")),
        cfg,
        &[],
        &["--no-tray", "--listen", "127.0.0.1:0", "--print-port"],
    )?;
    Ok((agent, addr))
}

/// 最小 OpenWindow（低帧率减轻无人消费视频流的负担）；ack/Error 都可——
/// 记账发生在 open_window 的 save_placement（早于任何捕获失败路径）。
///
/// 独立 Agent 进程的**首个** WGC FramePool 冷启动实测约 4.0s，与 t15 同源，
/// 故用 [`common::OPEN_TIMEOUT`] 而非 5s 默认值。
async fn open_window_quiet(c: &mut Client, hwnd: u64) -> Result<()> {
    let reply = c
        .call_timeout(
            Msg::OpenWindow(OpenWindow {
                window_id: hwnd,
                fit: FitMode::Fit as i32,
                max_level: RenderLevel::L0Pixel as i32,
                max_fps: 1,
                view_only: false,
            }),
            common::OPEN_TIMEOUT,
        )
        .await?;
    println!("[t14] OpenWindow 回包: {:?}", reply.msg);
    Ok(())
}

/// 移动窗口外框位置（保持尺寸）。
fn move_window(hwnd: isize, x: i32, y: i32) -> Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    let h = HWND(hwnd as *mut _);
    unsafe {
        SetWindowPos(
            h,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .context("SetWindowPos 失败")?;
    Ok(())
}

/// `WINDOWPLACEMENT.rcNormalPosition`（普通态外框 [l, t, r, b]；账本投影字段）。
fn placement_ltrb(hwnd: isize) -> Result<[i32; 4]> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, WINDOWPLACEMENT};
    let h = HWND(hwnd as *mut _);
    let mut wp = WINDOWPLACEMENT {
        length: mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    unsafe { GetWindowPlacement(h, &mut wp) }.context("GetWindowPlacement 失败")?;
    let rc = wp.rcNormalPosition;
    Ok([rc.left, rc.top, rc.right, rc.bottom])
}

/// 轮询直到窗口 `rcNormalPosition` 等于 `want`（启动恢复生效；B 进程在打印
/// RD_LISTEN 前已完成 recover，此处为防御性等待）。
async fn wait_placement(hwnd: isize, want: &[i32; 4]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if &placement_ltrb(hwnd)? == want {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "8s 内窗口未回到记账位置 {want:?}（当前 {:?}）",
            placement_ltrb(hwnd)?
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 读账本全部条目（`Vec<LedgerEntry>`，明文 JSON）。
fn ledger_entries(cfg: &Path) -> Result<Vec<rd_window_win::ledger::LedgerEntry>> {
    let path = cfg.join(LEDGER_FILE);
    let bytes = std::fs::read(&path)
        .map_err(|err| anyhow::anyhow!("读账本失败（{}）: {err}", path.display()))?;
    serde_json::from_slice(&bytes).context("账本 JSON 解析失败")
}

/// 轮询直到账本记录了 `hwnd`（open_window 的 save_placement → record）。
async fn wait_ledger_entry(cfg: &Path, hwnd: u64) -> Result<rd_window_win::ledger::LedgerEntry> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(entries) = ledger_entries(cfg) {
            if let Some(e) = entries.iter().find(|e| e.hwnd == hwnd as isize) {
                return Ok(e.clone());
            }
        }
        ensure!(
            Instant::now() < deadline,
            "5s 内账本未记录 hwnd {hwnd:#x}（open_window 记账路径未生效）"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 轮询直到账本清空（restore 成功 → clear；或启动 recover 全清）。
async fn wait_ledger_empty(cfg: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match ledger_entries(cfg) {
            Ok(entries) if entries.is_empty() => return Ok(()),
            _ => {}
        }
        ensure!(Instant::now() < deadline, "8s 内账本未清空");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
