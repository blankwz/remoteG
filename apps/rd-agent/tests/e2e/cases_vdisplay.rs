//! t15 虚拟屏（T-044 一期步骤 7）：建屏 → 窗口落虚拟屏 → 换档 → 撤屏还原。
//!
//! 独立 agent（`RD_VDISPLAY=1`，e2e 默认关）连接共享 testwin。驱动缺失的
//! 环境（CI 无驱动）按建屏 FAILED 消息判 skip，不静默通过。用例会把共享
//! testwin 短暂挪到虚拟屏中央，结束后（含失败路径）撤屏并断言窗口回到原位。

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use rd_proto::v0::envelope::Msg;
use rd_proto::v0::virtual_display_state::Kind;
use rd_proto::v0::{CloseWindow, Envelope, SetDisplayWidth};

use crate::common::{self, Client};

/// 建屏通知（真机实测 528ms，留足宽松余量）与换档通知的超时。
const WAIT_NOTIFY: Duration = Duration::from_secs(30);
/// 等指定尺寸关键帧（含编码器重配置）。
const WAIT_FRAME: Duration = Duration::from_secs(20);
/// 断开后等撤屏 + 窗口布局还原。
const WAIT_RESTORE: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread")]
async fn t15_virtual_display_lifecycle() -> Result<()> {
    let _guard = common::serial_lock().await;
    if crate::skip_if_ci_video() {
        return Ok(());
    }
    if crate::skip_if_no_wgc() {
        return Ok(());
    }
    let hwnd = common::env().hwnd;

    // 连接前的拓扑与窗口矩形：撤屏后必须逐项还原
    let before = rd_vdisplay::displays().context("枚举显示拓扑失败")?;
    for d in &before {
        println!(
            "[t15] 连接前拓扑: {} ({}) {}×{} @{:?}",
            d.device_name, d.adapter, d.width, d.height, d.position
        );
    }
    let rect_before =
        rd_window_win::resize::window_rect(hwnd as isize).context("读窗口矩形失败")?;

    // 独立 agent：共享环境的 agent 带 RD_VDISPLAY=0（防 t05/t06 真实鼠标坐标漂移）。
    // config-dir 故意固定在 target/ 下而非 TempDir：失败后 agent 日志仍在原处可查，
    // 下次运行覆盖 license 即可，不残留语义状态。
    let cfg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/e2e-t15-cfg");
    std::fs::create_dir_all(&cfg).context("创建 config-dir 失败")?;
    println!("[t15] agent 日志目录: {}", cfg.join("logs").display());
    common::write_pro_license(&cfg)?;
    let _cleanup = common::AgentCleanup(cfg.clone());
    let (_agent, addr, _rx) = common::spawn_agent_custom(
        Path::new(env!("CARGO_BIN_EXE_rd-agent")),
        &cfg,
        &[("RD_VDISPLAY", "1")],
        &["--no-tray", "--listen", "127.0.0.1:0", "--print-port"],
    )?;

    let mut c = Client::connect_to(addr).await?;
    let fit = rd_vdisplay::ScreenFit::standard();
    ensure!(
        fit.base_width == 540,
        "标准机基准档应为 540，实际 {}",
        fit.base_width
    );
    let base = fit.size(540);

    // ── 1. 建屏：CREATED（标准机 540×950）+ 系统拓扑出现虚拟屏 ──
    let state = recv_vdisplay_state(&mut c).await?;
    match state.kind {
        k if k == Kind::Failed as i32 => {
            if state.message.contains("驱动管道不可用") {
                println!("SKIP: 虚拟屏驱动未安装（{}），跳过 t15", state.message);
                c.close().await;
                return Ok(());
            }
            bail!("建屏失败: {}", state.message);
        }
        k if k == Kind::Created as i32 => {}
        k => bail!("期望 VdisplayState{{CREATED}}，实际 kind={k}"),
    }
    ensure!(
        (state.width, state.height) == base,
        "建屏尺寸应为 {base:?}（标准机基准档），实际 {}×{}",
        state.width,
        state.height
    );
    // 上轮崩溃残留的同尺寸虚拟屏会在建屏时被清理，Windows 可能把同一设备号
    // 复用给新屏；故按「虚adapter + 基准尺寸」识别，优先取 before 里没有的名字。
    let created = wait_created_display(&before, base, Duration::from_secs(10)).await?;
    ensure!(
        created.is_virtual_display(),
        "新显示器适配器应含 \"Virtual Display\"，实际 {:?}",
        created.adapter
    );
    ensure!(
        (created.width, created.height) == base,
        "新显示器尺寸应为 {base:?}，实际 {}×{}",
        created.width,
        created.height
    );
    println!(
        "[t15] 建屏: {} ({}) {}×{} @({},{})",
        created.device_name,
        created.adapter,
        created.width,
        created.height,
        created.position.0,
        created.position.1
    );

    // ── 2. OpenWindow + Viewport：窗口落到虚拟屏中央并按视口出帧 ──
    let mut streams = common::open_window(&mut c, hwnd).await?;
    crate::send_viewport(&mut c, hwnd, 500, 900).await?;
    let got = crate::wait_keyframe_size(&mut streams.video, hwnd, WAIT_FRAME, (500, 900)).await?;
    println!("[t15] 视口 500×900 关键帧: {got:?}");

    let area = rd_vdisplay::displays()
        .context("枚举显示拓扑失败")?
        .into_iter()
        .find(|d| d.device_name == created.device_name)
        .context("虚拟屏从拓扑中消失")?;
    let rect = rd_window_win::resize::window_rect(hwnd as isize)?;
    let (cx, cy) = (rect.x + rect.w as i32 / 2, rect.y + rect.h as i32 / 2);
    ensure!(
        cx >= area.position.0
            && cx < area.position.0 + area.width as i32
            && cy >= area.position.1
            && cy < area.position.1 + area.height as i32,
        "窗口中心 ({cx},{cy}) 不在虚拟屏 {:?}+{}×{} 内",
        area.position,
        area.width,
        area.height
    );
    println!("[t15] 窗口已落虚拟屏中央: rect={rect:?}");

    // ── 3. 非法档位 → Error{60}，会话与屏均不受影响 ──
    let bad = c
        .call(Msg::SetDisplayWidth(SetDisplayWidth { width: 555 }))
        .await?;
    let Some(Msg::Error(err)) = bad.msg else {
        bail!("非法档位应回 Error{{60}}，实际 {:?}", bad.msg);
    };
    ensure!(
        err.code == 60,
        "非法档位错误码应为 60，实际 {}（{}）",
        err.code,
        err.message
    );
    println!("[t15] 非法档位 555 被拒: Error{{60}}");

    // ── 4. 换档到 1080：echo + SWITCHED(1080×1900) + 拓扑同步 ──
    let target = 1080u32;
    let target_size = fit.size(target);
    let echo = c
        .call(Msg::SetDisplayWidth(SetDisplayWidth { width: target }))
        .await?;
    let Some(Msg::SetDisplayWidth(m)) = echo.msg else {
        bail!("换档应回 SetDisplayWidth echo，实际 {:?}", echo.msg);
    };
    ensure!(
        m.width == target,
        "echo 宽度应为 {target}，实际 {}",
        m.width
    );
    let switched = recv_vdisplay_state(&mut c).await?;
    ensure!(
        switched.kind == Kind::Switched as i32,
        "换档后应通知 SWITCHED，实际 kind={}",
        switched.kind
    );
    ensure!(
        (switched.width, switched.height) == target_size,
        "换档后尺寸应为 {target_size:?}，实际 {}×{}",
        switched.width,
        switched.height
    );
    let after_switch = rd_vdisplay::displays()
        .context("枚举显示拓扑失败")?
        .into_iter()
        .find(|d| d.device_name == created.device_name)
        .context("换档后虚拟屏从拓扑中消失（换档不应重建屏）")?;
    ensure!(
        (after_switch.width, after_switch.height) == target_size,
        "系统拓扑尺寸应为 {target_size:?}，实际 {}×{}",
        after_switch.width,
        after_switch.height
    );
    println!(
        "[t15] 换档 540→{target}: 拓扑 {}×{}（屏未掉线）",
        after_switch.width, after_switch.height
    );

    // ── 5. 换档后按新档尺寸出帧：管线按新视口 reconfigure 且首帧为 IDR ──
    crate::send_viewport(&mut c, hwnd, 1000, 1800).await?;
    let got = crate::wait_keyframe_size(&mut streams.video, hwnd, WAIT_FRAME, (1000, 1800)).await?;
    println!("[t15] 换档后视口 1000×1800 关键帧: {got:?}");

    // ── 6. 断开：撤屏 + 窗口布局还原（先还原布局，再撤屏）──
    c.send(Msg::CloseWindow(CloseWindow { window_id: hwnd }))
        .await?;
    c.close().await;

    // 期望的「还原后拓扑」= 连接前拓扑扣除崩溃残留的同尺寸虚拟屏：建屏时
    // 清理残留是正确行为，故残留屏不应出现在还原结果里（若本来没有残留，
    // 期望即连接前拓扑）。
    let mut want = names(&before)
        .into_iter()
        .filter(|n| {
            !before.iter().any(|d| {
                d.device_name == *n && d.is_virtual_display() && (d.width, d.height) == base
            })
        })
        .collect::<Vec<_>>();
    want.sort();

    let deadline = Instant::now() + WAIT_RESTORE;
    loop {
        let now = rd_vdisplay::displays().context("枚举显示拓扑失败")?;
        let gone = !now.iter().any(|d| d.device_name == created.device_name);
        let rect = rd_window_win::resize::window_rect(hwnd as isize)?;
        if gone && rect == rect_before {
            let got = names(&now);
            ensure!(want == got, "撤屏后显示拓扑应还原为 {want:?}，实际 {got:?}");
            ensure!(
                !now.iter()
                    .any(|d| d.is_virtual_display() && (d.width, d.height) == base),
                "撤屏后仍残留基准尺寸的虚拟屏: {:?}",
                names(&now)
            );
            println!("t15 ok: 建屏/落屏/换档/撤屏全链通过，窗口矩形与显示拓扑已还原");
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "断开后 {:?} 内未完成撤屏/还原：屏已消失={gone}，窗口 rect={rect:?}（原 {rect_before:?}）",
            WAIT_RESTORE
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 等下一条虚拟屏状态通知（丢弃其间其他消息）。
async fn recv_vdisplay_state(c: &mut Client) -> Result<rd_proto::v0::VirtualDisplayState> {
    let env = c
        .recv_until(WAIT_NOTIFY, |e: &Envelope| {
            matches!(e.msg, Some(Msg::VdisplayState(_)))
        })
        .await
        .context("等待 VirtualDisplayState 超时")?;
    match env.msg {
        Some(Msg::VdisplayState(state)) => Ok(state),
        _ => bail!("收到空信封"),
    }
}

/// 轮询显示拓扑，找到本次建出的虚拟屏（虚 adapter + 基准尺寸）。
///
/// 优先返回 `before` 里没有的设备名（正常路径）；若设备名与连接前同名，
/// 说明那是上轮崩溃残留在建屏时被清理后系统复用了名字——此时拓扑里只剩
/// 这一块同尺寸虚拟屏，就是本次建的。
async fn wait_created_display(
    before: &[rd_vdisplay::DisplayInfo],
    base: (u32, u32),
    timeout: Duration,
) -> Result<rd_vdisplay::DisplayInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        let now = rd_vdisplay::displays().context("枚举显示拓扑失败")?;
        let mut cands: Vec<rd_vdisplay::DisplayInfo> = now
            .into_iter()
            .filter(|d| d.is_virtual_display() && (d.width, d.height) == base)
            .collect();
        if let Some(pos) = cands
            .iter()
            .position(|d| !before.iter().any(|b| b.device_name == d.device_name))
        {
            return Ok(cands.swap_remove(pos));
        }
        if cands.len() == 1 {
            return Ok(cands.remove(0));
        }
        ensure!(
            Instant::now() < deadline,
            "建屏后 {:?} 内未找到基准尺寸 {base:?} 的虚拟屏（候选 {} 块）",
            timeout,
            cands.len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 显示设备名集合（拓扑对比用）。
fn names(displays: &[rd_vdisplay::DisplayInfo]) -> Vec<String> {
    let mut v: Vec<String> = displays.iter().map(|d| d.device_name.clone()).collect();
    v.sort();
    v
}
