#![cfg(windows)]

//! T-025 无头端到端测试（任务卡步骤 2，8 用例）。
//!
//! 共享一次进程启动（tests/common）：testwin（800×600 静止画面）+ rd-agent
//! （随机端口 + 测试配对码），每个用例独立建立连接与窗口管线，用例名前缀
//! 编号保证串行语义下的执行顺序（libtest 按名称排序）。
//!
//! 运行：`cargo test -p rd-agent --test e2e -- --test-threads=1 --nocapture`
//!
//! WGC 不可用时相关用例 skip 并打印原因（不得静默通过）；窗口列表与终端
//! 用例不依赖 WGC，始终执行。

mod common;

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rd_proto::v0::envelope::Msg;
use rd_proto::v0::window_event::Kind;
use rd_proto::v0::{KeyEvent, ListWindows, MouseButton, OpenTerminal, ViewportInfo};
use rd_transport::{DataFrame, StreamKind};

use common::Client;

/// WGC 不可用时跳过依赖捕获的用例（打印原因，不静默通过）。
fn skip_if_no_wgc() -> bool {
    if rd_capture_win::is_supported() {
        return false;
    }
    println!(
        "SKIP: Windows.Graphics.Capture 不可用（需 Windows 10 1903+ 且未被策略禁用），\
         跳过本用例"
    );
    true
}

// ---------------------------------------------------------------------------
// t01 窗口列表
// ---------------------------------------------------------------------------

/// WindowList 中有标题含 `clarity-bench` 的窗口，且 id 与本地枚举一致。
#[tokio::test(flavor = "multi_thread")]
async fn t01_list_contains_testwin() -> Result<()> {
    let _guard = common::serial_lock().await;
    let env = common::env();
    let mut c = Client::connect().await?;
    let reply = c.call(Msg::ListWindows(ListWindows {})).await?;
    let Some(Msg::WindowList(list)) = reply.msg else {
        bail!("期望 WindowList，实际 {:?}", reply.msg);
    };
    // 桌面上可能存在历史遗留的同名窗口（例如上次中断运行留下的 testwin），
    // 断言以"本用例启动的 hwnd 出现在列表中"为准。
    let hit = list
        .windows
        .iter()
        .find(|w| w.id == env.hwnd && w.title.contains("clarity-bench"))
        .with_context(|| {
            format!(
                "WindowList（{} 项）中没有本用例的 testwin（hwnd {:#x}）",
                list.windows.len(),
                env.hwnd
            )
        })?;
    anyhow::ensure!(
        hit.w == common::CLIENT_W && hit.h == common::CLIENT_H,
        "testwin 客户区应为 {}x{}，实际 {}x{}",
        common::CLIENT_W,
        common::CLIENT_H,
        hit.w,
        hit.h
    );
    println!(
        "t01 ok: {} 个窗口，命中 id={:#x} \"{}\" {}x{}",
        list.windows.len(),
        hit.id,
        hit.title,
        hit.w,
        hit.h
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// t02 视频首帧
// ---------------------------------------------------------------------------

/// OpenWindow 后收到 keyframe 视频帧，解码成功且尺寸等于客户区。
#[tokio::test(flavor = "multi_thread")]
async fn t02_open_window_keyframe() -> Result<()> {
    let _guard = common::serial_lock().await;
    let env = common::env();
    if skip_if_no_wgc() {
        return Ok(());
    }
    let mut c = Client::connect().await?;
    let mut s = common::open_window(&mut c, env.hwnd).await?;
    anyhow::ensure!(
        s.ack.codec == rd_proto::v0::Codec::H264420 as i32,
        "ack.codec 应为 H264_420，实际 {}",
        s.ack.codec
    );

    let started = Instant::now();
    let (header, au) =
        common::recv_video_frame(&mut s.video, env.hwnd, Duration::from_secs(5)).await?;
    let elapsed = started.elapsed();
    anyhow::ensure!(
        header.keyframe,
        "首个视频帧必须是 keyframe（IDR），实际 seq={}",
        header.seq
    );
    let (w, h) = common::decode_h264_size(&au)?;
    anyhow::ensure!(
        w == common::CLIENT_W && h == common::CLIENT_H,
        "解码尺寸应为 {}x{}，实际 {}x{}",
        common::CLIENT_W,
        common::CLIENT_H,
        w,
        h
    );
    if elapsed > Duration::from_secs(2) {
        println!("WARN: 首帧耗时 {:.1?}（任务卡期望 <2s）", elapsed);
    }
    println!(
        "t02 ok: keyframe {} 字节，解码 {}x{}，首帧耗时 {:.1?}",
        au.len(),
        w,
        h,
        elapsed
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// t03 无损瓦片逐像素正确
// ---------------------------------------------------------------------------

/// 静止后收集瓦片拼图，与 rd-capture-win 直接抓取的源图在已覆盖区域逐像素一致。
#[tokio::test(flavor = "multi_thread")]
async fn t03_tiles_pixel_exact() -> Result<()> {
    let _guard = common::serial_lock().await;
    let env = common::env();
    if skip_if_no_wgc() {
        return Ok(());
    }
    let mut c = Client::connect().await?;
    let mut s = common::open_window(&mut c, env.hwnd).await?;

    // 并发丢弃视频流：不消费会让发送任务积压（QUIC 流控阻塞 → 在途字节记账
    // 只增不减），会话码率自适应随即把码率砍到下限，瓦片字节预算低于单个
    // 64px 块的编码尺寸，瓦片将永远无法下发。
    let drain = tokio::spawn(async move { while let Ok(Some(_)) = s.video.recv().await {} });

    // 瓦片产出时序：首帧创建编码器/精修器（实测约 150 ms）→ 静止 ≥300 ms
    // 成熟 → tick（50 ms 周期、字节预算分批）下发，首批瓦片约在 500 ms 处。
    // 先等精修完成再开收集——此前的瓦片已排在 QUIC 流缓冲中，collect 启动
    // 即读到；同时避开 collect 的 400 ms 静默提前退出与成熟期的重叠误判。
    tokio::time::sleep(Duration::from_millis(800)).await;
    let hwnd = env.hwnd;
    let collect_handle = tokio::spawn(async move {
        common::collect_tiles(&mut s.tile, hwnd, Duration::from_secs(6))
            .await
            .expect("瓦片收集失败")
    });
    // 源图与瓦片同为静止画面（Agent 与本地抓取时刻内容相同），再等 700 ms
    // 保持约 1.5 s 的原始节奏。
    tokio::time::sleep(Duration::from_millis(700)).await;

    // 源图：testwin 静止画面，Agent 抓取时刻与本地抓取时刻内容相同。
    let src = common::capture_source_frame(env.hwnd)?;
    anyhow::ensure!(
        src.width == common::CLIENT_W && src.height == common::CLIENT_H,
        "源图尺寸应为 {}x{}，实际 {}x{}",
        common::CLIENT_W,
        common::CLIENT_H,
        src.width,
        src.height
    );

    let (canvas, mask) = collect_handle.await?;
    let total = (common::CLIENT_W as usize) * (common::CLIENT_H as usize);
    let covered = mask.iter().filter(|c| **c).count();
    let ratio = covered as f64 / total as f64;

    // 诊断 A：同一 TileRefiner 实现 + 同一预算公式在测试进程内复算——
    // 区分"预算装不进瓦片（参数问题）"与"传输丢失"。25 000 B =
    // 8000 kbps × 50% × 0.05 s；1 562 B = RateController 下限 500kbps 同式。
    let local_probe = {
        let raw = rd_encode::frame::RawFrame {
            width: src.width,
            height: src.height,
            stride: src.stride as i32,
            pts_us: 0,
            data: src.data.clone(),
        };
        let mut refiner = rd_encode::tiles::TileRefiner::new(raw.width, raw.height, 300);
        let t0 = Instant::now();
        let _ = refiner.on_frame(&raw, None, t0);
        let tiles = refiner.tick(&raw, t0 + Duration::from_millis(301), 25_000, None);
        let max_bytes = tiles.iter().map(|t| t.data.len()).max().unwrap_or(0);
        let _ = refiner.on_frame(&raw, None, t0);
        let tiles_low = refiner.tick(&raw, t0 + Duration::from_millis(602), 1_562, None);
        (tiles.len(), max_bytes, tiles_low.len())
    };
    println!(
        "[e2e] 诊断·本地复算同帧内容：预算 25000B → {} 张（最大 {} B）；下限 1562B → {} 张",
        local_probe.0, local_probe.1, local_probe.2
    );

    // 诊断 B：读 1 条 StatsReport。视频静止后 kbps ≈ 瓦片发送速率。
    let mut agent_kbps = None;
    if let Ok(stats_env) = c
        .recv_until(Duration::from_secs(2), |e: &rd_proto::v0::Envelope| {
            matches!(e.msg, Some(Msg::Stats(_)))
        })
        .await
    {
        if let Some(Msg::Stats(s)) = stats_env.msg {
            agent_kbps = Some(s.kbps);
            println!(
                "[e2e] 诊断·Agent Stats: fps={} kbps={} rtt={}ms",
                s.fps, s.kbps, s.rtt_ms
            );
        }
    }

    anyhow::ensure!(
        ratio >= 0.15,
        "瓦片覆盖率过低: {:.1}%（{} / {total} px；本地复算同预算 {} 张/最大 {} B，\
         Agent 瓦片 kbps={agent_kbps:?}）",
        ratio * 100.0,
        covered,
        local_probe.0,
        local_probe.1
    );

    // 已覆盖区域逐像素比对（BGRA 4 通道；源图行距为 stride）。
    let stride = src.stride as usize;
    let mut mismatches = 0usize;
    let mut first_bad: Option<(i32, i32, [u8; 4], [u8; 4])> = None;
    for y in 0..common::CLIENT_H as usize {
        for x in 0..common::CLIENT_W as usize {
            let i = y * common::CLIENT_W as usize + x;
            if !mask[i] {
                continue;
            }
            let got: [u8; 4] = canvas[i * 4..i * 4 + 4].try_into().expect("4 字节");
            let sb = x * 4 + y * stride;
            let want: [u8; 4] = src.data[sb..sb + 4].try_into().expect("4 字节");
            if got != want {
                mismatches += 1;
                if first_bad.is_none() {
                    first_bad = Some((x as i32, y as i32, got, want));
                }
            }
        }
    }
    anyhow::ensure!(
        mismatches == 0,
        "已覆盖区域存在 {mismatches} 处像素不一致，首个 {:?}（收到的 BGRA, 源 BGRA）",
        first_bad
    );
    drain.abort();
    println!(
        "t03 ok: 覆盖率 {:.1}%（{covered}/{total} px），已覆盖区域逐像素一致",
        ratio * 100.0
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// t04 视口适配（Fit）
// ---------------------------------------------------------------------------

/// Viewport(1000×640, dpr 1) → 下一 keyframe 尺寸 1000×640；回 800×600 同样成立。
///
/// 先发一次非默认尺寸再回到任务卡的 800×600：初始客户区即 800×600 时
/// （静止画面无新帧），直接发 800×600 不会产生"下一个 keyframe"。
#[tokio::test(flavor = "multi_thread")]
async fn t04_fit_resizes_window() -> Result<()> {
    let _guard = common::serial_lock().await;
    let env = common::env();
    if skip_if_no_wgc() {
        return Ok(());
    }
    let mut c = Client::connect().await?;
    let mut s = common::open_window(&mut c, env.hwnd).await?;

    // 基线：初始 keyframe（800×600），此后 resize 才会有新 IDR。
    let (header, au) =
        common::recv_video_frame(&mut s.video, env.hwnd, Duration::from_secs(5)).await?;
    anyhow::ensure!(header.keyframe, "初始帧应为 IDR");
    let base = common::decode_h264_size(&au)?;
    anyhow::ensure!(
        base == (common::CLIENT_W, common::CLIENT_H),
        "基线尺寸异常: {base:?}"
    );

    send_viewport(&mut c, env.hwnd, 1000, 640).await?;
    let got =
        wait_keyframe_size(&mut s.video, env.hwnd, Duration::from_secs(6), (1000, 640)).await?;
    anyhow::ensure!(got == (1000, 640), "Fit 1000×640 未生效，实际 {got:?}");

    send_viewport(&mut c, env.hwnd, common::CLIENT_W, common::CLIENT_H).await?;
    let got = wait_keyframe_size(
        &mut s.video,
        env.hwnd,
        Duration::from_secs(6),
        (common::CLIENT_W, common::CLIENT_H),
    )
    .await?;
    anyhow::ensure!(
        got == (common::CLIENT_W, common::CLIENT_H),
        "回 800×600 未生效，实际 {got:?}"
    );
    println!("t04 ok: Viewport → keyframe 1000x640 → 800x600 均按 1:1 适配");
    Ok(())
}

async fn send_viewport(c: &mut Client, hwnd: u64, css_w: u32, css_h: u32) -> Result<()> {
    c.send(Msg::Viewport(ViewportInfo {
        window_id: hwnd,
        css_w,
        css_h,
        dpr: 1.0,
    }))
    .await?;
    Ok(())
}

/// 等到解码尺寸为 `want` 的 keyframe（跳过其他 keyframe 与 P 帧）。
async fn wait_keyframe_size(
    rx: &mut rd_transport::DataRx,
    hwnd: u64,
    timeout: Duration,
    want: (u32, u32),
) -> Result<(u32, u32)> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!left.is_zero(), "等待 {want:?} keyframe 超时");
        let (header, au) = common::recv_video_frame(rx, hwnd, left).await?;
        if !header.keyframe {
            continue;
        }
        let size = common::decode_h264_size(&au)?;
        if size == want {
            return Ok(size);
        }
        println!("[t04] keyframe 尺寸 {size:?} ≠ 期望 {want:?}，继续等待");
    }
}

// ---------------------------------------------------------------------------
// t05 点击注入
// ---------------------------------------------------------------------------

/// 对已打开窗口的客户区 (100,100) 左键按下抬起 → 标题 `clicks=1 last=100,100`。
#[tokio::test(flavor = "multi_thread")]
async fn t05_click_injection() -> Result<()> {
    let _guard = common::serial_lock().await;
    let env = common::env();
    if skip_if_no_wgc() {
        return Ok(());
    }
    let mut c = Client::connect().await?;
    // ack 到达即管线注册完成（is_open 门放行本连接的注入）。
    let _s = common::open_window(&mut c, env.hwnd).await?;

    // 预置前 + 失败重试：Agent 端置前偶发失败会直接丢弃该次注入（FIX-1），
    // 统一走 preset_foreground 消除该路径，确认改轮询而非固定时长。
    common::preset_foreground(env.hwnd);

    let deadline = Instant::now() + Duration::from_secs(9);
    let mut last = String::new();
    loop {
        send_click(&mut c, env.hwnd, 100, 100, 0).await?;
        // 每轮给 3s：点击生效（置前 + SendInput + 标题更新）的常规耗时 <1s。
        let matched = common::wait_for(Duration::from_secs(3), Duration::from_millis(100), || {
            if let Ok(title) = common::testwin_title(env.hwnd) {
                last = title;
            }
            last.contains("clicks=1 last=100,100")
        })
        .await;
        if matched {
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "9s 内标题未变为 clicks=1 last=100,100（最后: {last:?}）"
        );
        println!("[t05] 点击未生效（可能置前失败被丢弃），预置前后重试");
        common::preset_foreground(env.hwnd);
    }
    println!("t05 ok: 标题 {last:?}");
    Ok(())
}

async fn send_click(c: &mut Client, hwnd: u64, x: i32, y: i32, button: u32) -> Result<()> {
    c.send(Msg::MouseBtn(MouseButton {
        window_id: hwnd,
        x,
        y,
        button,
        down: true,
    }))
    .await?;
    c.send(Msg::MouseBtn(MouseButton {
        window_id: hwnd,
        x,
        y,
        button,
        down: false,
    }))
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// t06 右键菜单 → CHILD_ADDED
// ---------------------------------------------------------------------------

/// 对 testwin 客户区右键 → 3s 内收到 CHILD_ADDED（root_id=testwin）。
/// 随后点击菜单项关闭菜单（"菜单可交互感知"），确认组恢复为仅根窗口。
///
/// 弹出菜单（#32768）无法被 WGC 捕获（CreateForWindow E_INVALIDARG，Windows
/// 平台硬限制）：CHILD_ADDED 必须与子管线建立解耦——事件照发、无视频流降级
/// （D-011 缺陷 2 修复语义）；点击作用于菜单依赖 is_open 的组成员放行。
#[tokio::test(flavor = "multi_thread")]
async fn t06_menu_child_added() -> Result<()> {
    let _guard = common::serial_lock().await;
    let env = common::env();
    if skip_if_no_wgc() {
        return Ok(());
    }
    let mut c = Client::connect().await?;
    let _s = common::open_window(&mut c, env.hwnd).await?;

    // 弹菜单：预置前 + 右键注入 + 等 CHILD_ADDED（FIX-1 加固）。置前失败的
    // 右键会被 Agent 丢弃（无事件、本地窗口组也不见菜单）——预置前后重试
    // 一次；若菜单已弹出而事件未到，属 D-011 已裁决的解耦语义被破坏（事件
    // 照发），重试无济于事，直接失败，不得掩盖。
    const MENU_EVENT_TIMEOUT: Duration = Duration::from_secs(3);
    let mut child_added: Option<rd_proto::v0::Envelope> = None;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=2 {
        common::preset_foreground(env.hwnd);
        // 右键按下 + 抬起（button=1）；WM_RBUTTONUP 经 DefWindowProc 产生
        // WM_CONTEXTMENU。
        send_click(&mut c, env.hwnd, 400, 300, 1).await?;
        match c
            .recv_until(MENU_EVENT_TIMEOUT, |e: &rd_proto::v0::Envelope| {
                matches!(
                    &e.msg,
                    Some(Msg::WindowEvent(w))
                        if w.kind == Kind::ChildAdded as i32 && w.root_id == env.hwnd
                )
            })
            .await
        {
            Ok(ev) => {
                child_added = Some(ev);
                break;
            }
            Err(err) => {
                last_err = Some(err);
                if rd_window_win::window_group(env.hwnd as isize).len() > 1 {
                    bail!(
                        "菜单已弹出但 {MENU_EVENT_TIMEOUT:?} 内未收到 CHILD_ADDED\
                         （事件应与子管线建立解耦，不重试掩盖）: {:#}",
                        last_err.as_ref().expect("刚记录的错误")
                    );
                }
            }
        }
        if attempt == 1 {
            println!("[t06] 第 1 次右键未弹出菜单（可能置前失败被丢弃），预置前后重试");
        }
    }

    // 统一收尾：无论事件是否到达都必须关闭菜单，否则 testwin 消息循环被
    // TrackPopupMenu 模态阻塞，后续用例全部失效。菜单点击/Esc 同样经 Agent
    // 注入并过前台门控（FIX-1：置前失败即被丢弃）——每次注入前预置前，未
    // 关闭则逐级重试；最终仍是硬断言：菜单必须关闭。
    let menu_open_now = rd_window_win::window_group(env.hwnd as isize).len() > 1;
    if menu_open_now {
        let menu_id = child_added.as_ref().map(child_window_id);
        click_menu_item(&mut c, menu_id, env.hwnd).await?;
        if !menu_closed(env.hwnd, Duration::from_secs(3)).await {
            click_menu_item(&mut c, menu_id, env.hwnd).await?;
        }
        if !menu_closed(env.hwnd, Duration::from_secs(3)).await {
            common::preset_foreground(env.hwnd);
            for down in [true, false] {
                c.send(Msg::Key(KeyEvent {
                    scancode: 0x01, // Esc
                    extended: false,
                    down,
                    modifiers: 0,
                }))
                .await?;
            }
        }
        anyhow::ensure!(
            menu_closed(env.hwnd, Duration::from_secs(3)).await,
            "菜单未能关闭（testwin 消息循环仍被 TrackPopupMenu 阻塞，后续用例会失败）"
        );
    }

    match child_added {
        Some(ev) => {
            let Msg::WindowEvent(w) = ev.msg.context("空信封")? else {
                bail!("不可达");
            };
            let menu_id = w.info.as_ref().map(|i| i.id).unwrap_or(0);
            anyhow::ensure!(menu_id != 0, "CHILD_ADDED 未携带子窗口 id");
            // 显式关连接（t07 依赖本连接断开触发的管线清理，见 Client::close）。
            c.close().await;
            println!(
                "t06 ok: CHILD_ADDED child={menu_id:#x} root={:#x} 偏移=({},{}) 尺寸={}x{}；菜单已关闭",
                w.root_id,
                w.info.as_ref().map(|i| i.x).unwrap_or(0),
                w.info.as_ref().map(|i| i.y).unwrap_or(0),
                w.info.as_ref().map(|i| i.w).unwrap_or(0),
                w.info.as_ref().map(|i| i.h).unwrap_or(0),
            );
            Ok(())
        }
        None => {
            // 失败路径也显式关闭，避免残留管线拖垮下一用例的判定环境。
            c.close().await;
            let err = last_err.expect("两次尝试各记录一次错误");
            bail!("两次右键注入均未收到 CHILD_ADDED 且本地窗口组未见菜单（右键注入或菜单弹出失败）: {err:#}")
        }
    }
}

/// 从窗口事件信封中取子窗口 id（无 info 时为 0）。
fn child_window_id(ev: &rd_proto::v0::Envelope) -> u64 {
    match &ev.msg {
        Some(Msg::WindowEvent(w)) => w.info.as_ref().map(|i| i.id).unwrap_or(0),
        _ => 0,
    }
}

/// 点击菜单项关闭菜单（预置前后注入；无子窗口 id 时静默跳过，走 Esc 路径）。
async fn click_menu_item(c: &mut Client, menu_id: Option<u64>, root: u64) -> Result<()> {
    if let Some(id) = menu_id {
        common::preset_foreground(root);
        send_click(c, id, 40, 15, 0).await?;
    }
    Ok(())
}

/// 轮询本地窗口组，直到组内只剩根窗口（菜单已销毁）。
async fn menu_closed(root: u64, timeout: Duration) -> bool {
    common::wait_for(timeout, Duration::from_millis(100), || {
        rd_window_win::window_group(root as isize).len() <= 1
    })
    .await
}

// ---------------------------------------------------------------------------
// t07 未打开窗口的注入被拒绝
// ---------------------------------------------------------------------------

/// 不 OpenWindow 直接对 testwin 点击 → 标题计数不变（Agent 侧门禁丢弃）。
#[tokio::test(flavor = "multi_thread")]
async fn t07_unopened_window_input_rejected() -> Result<()> {
    let _guard = common::serial_lock().await;
    let env = common::env();
    let mut c = Client::connect().await?;

    // 门禁恢复探测（FIX-1 加固）：上一用例连接断开后 Agent 异步清理窗口管线
    // （on_disconnect），清理完成前旧管线的 is_open 门仍放行注入——固定
    // 500ms 在负载高时不够，曾致门禁漏放。改为探测点击轮询：标题变化 = 门禁
    // 仍放行（更新基线继续探）；标题不变 = 门禁已恢复。3s 上限内必须恢复。
    const GATE_RECOVER_TIMEOUT: Duration = Duration::from_secs(3);
    common::preset_foreground(env.hwnd);
    let mut baseline = common::testwin_title(env.hwnd)?;
    let probe_deadline = Instant::now() + GATE_RECOVER_TIMEOUT;
    loop {
        send_click(&mut c, env.hwnd, 100, 100, 0).await?;
        // 注入处理与标题更新（WM_SETTEXT）常规耗时 <100ms，留 150ms 裕量。
        tokio::time::sleep(Duration::from_millis(150)).await;
        let now = common::testwin_title(env.hwnd)?;
        if now == baseline {
            break;
        }
        baseline = now;
        anyhow::ensure!(
            Instant::now() < probe_deadline,
            "{GATE_RECOVER_TIMEOUT:?} 内门禁未恢复（未打开窗口的注入仍被放行，\
             on_disconnect 清理超时；最后: {baseline:?}）"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 正式断言（语义不变）：门禁恢复后，未打开窗口的点击必须被拒绝。1s 注入
    // 生效窗口内轮询，标题一旦变化立即失败（fail-fast，等效原固定 1s 等待）。
    let before = common::testwin_title(env.hwnd)?;
    send_click(&mut c, env.hwnd, 100, 100, 0).await?;
    let leaked = common::wait_for(Duration::from_secs(1), Duration::from_millis(100), || {
        common::testwin_title(env.hwnd).is_ok_and(|t| t != before)
    })
    .await;
    anyhow::ensure!(
        !leaked,
        "未打开窗口的点击被注入了：{before:?} → {:?}",
        common::testwin_title(env.hwnd).unwrap_or_default()
    );
    println!("t07 ok: 标题保持 {before:?}（点击被拒绝）");
    Ok(())
}

// ---------------------------------------------------------------------------
// t08 终端回显
// ---------------------------------------------------------------------------

/// OpenTerminal(cmd.exe) → TerminalOpened → 写 `echo rd_e2e` → 3s 内收到回显。
#[tokio::test(flavor = "multi_thread")]
async fn t08_terminal_echo() -> Result<()> {
    let _guard = common::serial_lock().await;
    // 终端不依赖捕获，但单跑本用例时也需先拉起共享的 testwin/Agent 环境。
    common::env();
    let mut c = Client::connect().await?;

    c.send(Msg::OpenTerm(OpenTerminal {
        shell: "cmd.exe".to_string(),
        cols: 80,
        rows: 24,
        resume_id: String::new(),
    }))
    .await?;
    let opened = c
        .recv_until(Duration::from_secs(5), |e: &rd_proto::v0::Envelope| {
            matches!(e.msg, Some(Msg::TermOpened(_)))
        })
        .await?;
    let Msg::TermOpened(term) = opened.msg.context("空信封")? else {
        bail!("不可达");
    };
    anyhow::ensure!(!term.term_id.is_empty(), "TerminalOpened.term_id 为空");

    let (kind, stream_id, mut term_tx, mut term_rx) = c.conn.accept_bidi().await?;
    anyhow::ensure!(
        kind == StreamKind::Terminal,
        "双向流种类应为终端，实际 {kind:?}"
    );
    anyhow::ensure!(
        u64::from(term.stream_id) == stream_id,
        "TerminalOpened.stream_id {} 与实际流 id {stream_id} 不符",
        term.stream_id
    );

    term_tx.send_raw(b"echo rd_e2e\r\n").await?;

    // 3s 内累计原始输出，命中 `rd_e2e` 即成功（快照与 VT 序列原样转发）。
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !left.is_zero(),
            "3s 内未收到 rd_e2e 回显（已收 {} 字节）",
            buf.len()
        );
        let frame = tokio::time::timeout(left, term_rx.recv())
            .await
            .context("终端流读取超时")?
            .context("终端流接收失败")?;
        match frame {
            Some(DataFrame::Raw(bytes)) => {
                buf.extend_from_slice(&bytes);
                if buf.windows(6).any(|w| w == b"rd_e2e") {
                    break;
                }
            }
            Some(other) => println!("[e2e] 终端流收到非预期帧: {other:?}"),
            None => bail!("终端流被对端关闭（已收 {} 字节）", buf.len()),
        }
    }
    println!(
        "t08 ok: term={} stream={stream_id}，{} 字节输出内命中 rd_e2e",
        term.term_id,
        buf.len()
    );
    Ok(())
}
