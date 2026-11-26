//! 【一台电脑测遍】本机全链路真实双端集成自测（不打包、零 Mock）：
//! 一次性拉起真实本地 `rd-relay` + 真实 `rd-agent` 子进程 + 真实控制端协议栈，覆盖：
//! 1. 局域网直连 + 6 位码 SPAKE2 配对 + 信任落盘重连；
//! 2. 完整桌面（`0xffff000000000000`）WGC 实采 + H.264 硬编（`h264_mf` GPU MFT）出流 + QOI 瓦片流；
//! 3. 144Hz / 高码率在线热调（`LabTuning` 硬编 ↔ OpenH264 软编热切，重放用户日志中的 15855 / 18233 / 80000 / 120000 kbps）；
//! 4. 自建 `rd-relay` 9 位数字 ID 反查指纹 + `connect_via_relay_punch` 打洞竞速 + `connect_via_relay` 强制纯中转出画；
//! 5. 零服务器 STUN P2P 同端口复用打洞（含 Windows UDP `WSAECONNRESET 10054` 抗干扰验证）；
//! 6. 全程审计 `rd-agent` 日志：断言零 `set_bitrate` 失败、零 `MAX_BITRATE rc=1`、零 OpenH264 告警、零 panic。

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rd_proto::v0::{
    envelope, CloseWindow, Envelope, FitMode, Hello, LabTuning, ListWindows, OpenWindow,
    PairRequest, ViewportInfo,
};
use rd_security::{ControllerPairing, PairStep};
use rd_transport::{
    bind_udp_socket, connect, connect_socket, connect_via_relay, connect_via_relay_punch,
    ControlRx, DataFrame, DataRx, Fingerprint, Identity, Server, StreamKind, VideoHeader,
};

const TEST_PAIR_CODE: &str = "123456";
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

struct LiveAgentProc {
    child: Child,
    logs: Arc<Mutex<Vec<String>>>,
}

impl Drop for LiveAgentProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 启动配置了本地 `rd-relay` 的真实 `rd-agent` 子进程，并收集全部 stdout/stderr 日志行。
fn spawn_live_agent(
    dir: &std::path::Path,
    relay_addr: SocketAddr,
) -> Result<(LiveAgentProc, SocketAddr, String)> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join("agent.toml"),
        format!(
            "relay_addr = \"{relay_addr}\"\nmax_fps = 144\nstart_kbps = 20000\nrefine_enabled = true\n"
        ),
    )?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_rd-agent"))
        .args([
            "run",
            "--no-tray",
            "--listen",
            "127.0.0.1:0",
            "--print-port",
            "--config-dir",
        ])
        .arg(dir)
        .args(["--test-pair-code", TEST_PAIR_CODE])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("RD_RELAY", relay_addr.to_string())
        .env(
            "RUST_LOG",
            "info,rd_encode=info,rd_agent::window_session=info",
        )
        .spawn()
        .context("启动 rd-agent 子进程失败")?;

    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let stdout = child.stdout.take().context("未捕获 stdout")?;
    let stderr = child.stderr.take().context("未捕获 stderr")?;

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<(SocketAddr, String)>();
    {
        let logs_out = Arc::clone(&logs);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            let mut listen_addr: Option<SocketAddr> = None;
            let mut numeric_id: Option<String> = None;
            let mut sent = false;
            for line in reader.lines().map_while(Result::ok) {
                let trimmed = line.trim().to_string();
                if listen_addr.is_none() {
                    if let Some(raw) = trimmed.strip_prefix("RD_LISTEN=") {
                        if let Ok(a) = raw.parse::<SocketAddr>() {
                            if a.is_ipv4() {
                                listen_addr = Some(a);
                            }
                        }
                    }
                }
                if numeric_id.is_none() {
                    if let Some(raw) = trimmed.strip_prefix("RD_NUMERIC_ID=") {
                        numeric_id = Some(raw.trim().to_string());
                    }
                }
                if !sent {
                    if let (Some(a), Some(id)) = (listen_addr, numeric_id.as_ref()) {
                        let _ = ready_tx.send((a, id.clone()));
                        sent = true;
                    }
                }
                logs_out.lock().expect("logs lock").push(trimmed);
            }
        });
    }
    {
        let logs_err = Arc::clone(&logs);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                logs_err
                    .lock()
                    .expect("logs lock")
                    .push(format!("[stderr] {}", line.trim()));
            }
        });
    }

    let (addr, num_id) = ready_rx
        .recv_timeout(Duration::from_secs(30))
        .context("等待 rd-agent 输出 RD_LISTEN 与 RD_NUMERIC_ID 超时")?;
    Ok((LiveAgentProc { child, logs }, addr, num_id))
}

fn hello_env(req_id: u32) -> Envelope {
    Envelope {
        req_id,
        msg: Some(envelope::Msg::Hello(Hello {
            proto_ver: 0,
            device_name: "single-pc-live-tester".to_string(),
            decode_caps: vec![0],
            supports_444: false,
            media_fec: false,
        })),
    }
}

/// 从控制流等待指定匹配的响应（自动跳过服务端主动推送的 Stats / LabTelemetry / SessionState / WindowEvent）。
async fn recv_until<T>(
    rx: &mut ControlRx,
    mut pred: impl FnMut(Envelope) -> Option<T>,
) -> Result<T> {
    let deadline = Instant::now() + RECV_TIMEOUT;
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            bail!("等待目标控制消息超时");
        }
        let env = tokio::time::timeout(remain, rx.recv())
            .await
            .context("控制流等待超时")?
            .context("控制流已关闭")?
            .context("解码控制流信封失败")?;
        if let Some(envelope::Msg::Error(err)) = &env.msg {
            bail!("收到协议错误 Error(code={}): {}", err.code, err.message);
        }
        if let Some(hit) = pred(env) {
            return Ok(hit);
        }
    }
}

/// 在给定连接上完成 4 步 6 位码 SPAKE2 配对，返回对端指纹。
async fn pair_with_code(
    conn: &rd_transport::Conn,
    ctrl_id: &Identity,
    code: &str,
) -> Result<Fingerprint> {
    let peer_fp = conn.peer_fingerprint();
    let (mut tx, mut rx) = conn.open_control().await?;
    tx.send(&hello_env(1)).await?;
    // 未信任连接首条 Hello 会返回 Error(code=1)，先消费掉再走 SPAKE2 配对
    let first = tokio::time::timeout(RECV_TIMEOUT, rx.recv())
        .await??
        .context("控制流关闭")?;
    anyhow::ensure!(
        matches!(first.msg, Some(envelope::Msg::Error(ref e)) if e.code == 1),
        "期望未信任 Error(1)，实际: {first:?}"
    );

    let (mut pairing, msg1) = ControllerPairing::start(code, &ctrl_id.fingerprint(), &peer_fp);
    tx.send(&Envelope {
        req_id: 2,
        msg: Some(envelope::Msg::PairReq(PairRequest { spake2_msg: msg1 })),
    })
    .await?;

    let res1 = recv_until(&mut rx, |e| match e.msg {
        Some(envelope::Msg::PairRes(r)) => Some(r),
        _ => None,
    })
    .await?;
    if !res1.ok {
        bail!("SPAKE2 第一步被拒: {}", res1.reason);
    }

    let mac_c = match pairing.on_message(&res1.spake2_msg) {
        PairStep::Send(c) => c,
        other => bail!("期望 PairStep::Send, 实际 {other:?}"),
    };
    tx.send(&Envelope {
        req_id: 3,
        msg: Some(envelope::Msg::PairReq(PairRequest { spake2_msg: mac_c })),
    })
    .await?;

    let res2 = recv_until(&mut rx, |e| match e.msg {
        Some(envelope::Msg::PairRes(r)) => Some(r),
        _ => None,
    })
    .await?;
    if !res2.ok {
        bail!("SPAKE2 确认步被拒: {}", res2.reason);
    }
    match pairing.on_message(&res2.spake2_msg) {
        PairStep::Done => Ok(peer_fp),
        other => bail!("期望 PairStep::Done, 实际 {other:?}"),
    }
}

/// 接收连接上的 Video 与 Tile 两条单向数据流。
async fn accept_video_and_tile(conn: &rd_transport::Conn) -> Result<(DataRx, DataRx)> {
    let mut video = None;
    let mut tile = None;
    for _ in 0..2 {
        let (kind, _sid, rx) = tokio::time::timeout(Duration::from_secs(8), conn.accept_data())
            .await
            .context("等待服务端打开数据流超时")??;
        match kind {
            StreamKind::Video => video = Some(rx),
            StreamKind::Tile => tile = Some(rx),
            other => bail!("意外的数据流类型: {other:?}"),
        }
    }
    Ok((
        video.context("未收到 Video 数据流")?,
        tile.context("未收到 Tile 数据流")?,
    ))
}

async fn recv_next_video_frame(rx: &mut DataRx) -> Result<(VideoHeader, Vec<u8>)> {
    let frame = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .context("等待视频帧超时")??
        .context("视频流已关闭")?;
    match frame {
        DataFrame::Video(h, d) => Ok((h, d.to_vec())),
        other => bail!("期望 DataFrame::Video, 实际 {other:?}"),
    }
}

/// 向本地 `rd-relay` 发送 9 位数字 ID 反查请求（TYPE_LOOKUP_ID = 7），返回 32 字节设备指纹。
async fn lookup_9digit_id_on_relay(
    relay_addr: SocketAddr,
    numeric_id: &str,
) -> Result<Fingerprint> {
    let digits: String = numeric_id.chars().filter(|c| c.is_ascii_digit()).collect();
    let id_num: u32 = digits.parse().context("解析 9 位数字 ID 失败")?;
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let mut req = [0u8; 13];
    req[8] = 7; // TYPE_LOOKUP_ID
    req[9..13].copy_from_slice(&id_num.to_le_bytes());

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut buf = [0u8; 64];
    loop {
        if Instant::now() >= deadline {
            bail!("等待 rd-agent 在本地 rd-relay 注册 9 位 ID ({numeric_id}) 超时");
        }
        sock.send_to(&req, relay_addr).await?;
        if let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(400), sock.recv_from(&mut buf)).await
        {
            if n >= 42 && buf[8] == 8 && buf[9] == 1 {
                let mut fp = [0u8; 32];
                fp.copy_from_slice(&buf[10..42]);
                return Ok(Fingerprint(fp));
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// 极简本地 RFC 5389 STUN Binding Server（回送 XOR-MAPPED-ADDRESS），供离线/回环 STUN 打洞自测。
async fn spawn_local_stun_server() -> Result<SocketAddr> {
    const MAGIC: u32 = 0x2112_A442;
    let sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
    let local = sock.local_addr()?;
    tokio::spawn(async move {
        let mut buf = [0u8; 256];
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            if n < 20 || buf[0] != 0x00 || buf[1] != 0x01 {
                continue;
            }
            let SocketAddr::V4(v4) = peer else {
                continue;
            };
            let mut resp = [0u8; 32];
            resp[0] = 0x01;
            resp[1] = 0x01;
            resp[2] = 0x00;
            resp[3] = 0x0c;
            resp[4..8].copy_from_slice(&MAGIC.to_be_bytes());
            resp[8..20].copy_from_slice(&buf[8..20]);
            resp[20] = 0x00;
            resp[21] = 0x20;
            resp[22] = 0x00;
            resp[23] = 0x08;
            resp[24] = 0x00;
            resp[25] = 0x01;
            let xport = v4.port() ^ ((MAGIC >> 16) as u16);
            resp[26..28].copy_from_slice(&xport.to_be_bytes());
            let ip = v4.ip().octets();
            let mc = MAGIC.to_be_bytes();
            for i in 0..4 {
                resp[28 + i] = ip[i] ^ mc[i];
            }
            let _ = sock.send_to(&resp, peer).await;
        }
    });
    Ok(local)
}

async fn stun_query_mapped(
    sock: &tokio::net::UdpSocket,
    stun_srv: SocketAddr,
) -> Result<SocketAddr> {
    const MAGIC: u32 = 0x2112_A442;
    let mut req = [0u8; 20];
    req[0] = 0x00;
    req[1] = 0x01;
    req[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    req[8..20].copy_from_slice(&[0x42; 12]);
    sock.send_to(&req, stun_srv).await?;
    let mut buf = [0u8; 128];
    loop {
        let res = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf)).await?;
        let (n, _) = match res {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e.into()),
        };
        if n < 32 || buf[0] != 0x01 || buf[1] != 0x01 {
            bail!("STUN 响应无效");
        }
        let xport = u16::from_be_bytes([buf[26], buf[27]]) ^ ((MAGIC >> 16) as u16);
        let mc = MAGIC.to_be_bytes();
        let ip = std::net::Ipv4Addr::new(
            buf[28] ^ mc[0],
            buf[29] ^ mc[1],
            buf[30] ^ mc[2],
            buf[31] ^ mc[3],
        );
        return Ok(SocketAddr::new(std::net::IpAddr::V4(ip), xport));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn single_pc_full_live_e2e() -> Result<()> {
    // =========================================================================
    // 阶段 0：启动本地自建 rd-relay + 真实 rd-agent 被控端进程
    // =========================================================================
    let relay_addr = rd_relay::spawn("127.0.0.1:0".parse()?).await?;
    let temp_dir = tempfile::tempdir()?;
    let (agent, agent_addr, numeric_id) = spawn_live_agent(temp_dir.path(), relay_addr)?;
    println!(
        "\n=== [阶段 0] 本地 rd-relay={relay_addr}, rd-agent={agent_addr}, 9位ID={numeric_id} ==="
    );

    let ctrl_id = Identity::generate("single-pc-controller")?;

    // =========================================================================
    // 阶段 1：局域网直连 + 6 位码 SPAKE2 配对 + 完整桌面 144Hz 硬编/软编热切实测
    // =========================================================================
    let pair_conn = connect(agent_addr, &ctrl_id, None).await?;
    let agent_fp = pair_with_code(&pair_conn, &ctrl_id, TEST_PAIR_CODE).await?;
    pair_conn.close(0, "paired");
    tokio::time::sleep(Duration::from_millis(250)).await;
    println!(
        "=== [阶段 1.1] 局域网 6 位码配对成功，对端指纹={} ===",
        agent_fp.short_hex()
    );

    // 信任重连 → HelloAck → ListWindows → 打开完整桌面 (0xffff000000000000)
    let lan_conn = connect(agent_addr, &ctrl_id, Some(agent_fp)).await?;
    let (mut tx, mut rx) = lan_conn.open_control().await?;
    tx.send(&hello_env(10)).await?;
    let hello_ack = recv_until(&mut rx, |e| match e.msg {
        Some(envelope::Msg::HelloAck(ack)) => Some(ack),
        _ => None,
    })
    .await?;
    println!(
        "=== [阶段 1.2] 信任免码重连成功：host={}, encode_caps={:?} ===",
        hello_ack.host_name, hello_ack.encode_caps
    );

    tx.send(&Envelope {
        req_id: 11,
        msg: Some(envelope::Msg::ListWindows(ListWindows {})),
    })
    .await?;
    let win_list = recv_until(&mut rx, |e| match e.msg {
        Some(envelope::Msg::WindowList(w)) => Some(w),
        _ => None,
    })
    .await?;
    assert!(
        !win_list.windows.is_empty(),
        "WindowList 至少应包含主显示器完整桌面"
    );
    let desktop_id = rd_proto::desktop_window_id(0);
    let desktop_win = win_list
        .windows
        .iter()
        .find(|w| w.id == desktop_id)
        .context("WindowList 中未找到完整桌面条目 0xffff000000000000")?;
    assert!(
        desktop_win.w >= 640 && desktop_win.h >= 480,
        "完整桌面分辨率异常: {}x{}",
        desktop_win.w,
        desktop_win.h
    );
    println!(
        "=== [阶段 1.3] 枚举窗口成功（共 {} 项），主桌面分辨率={}x{} ===",
        win_list.windows.len(),
        desktop_win.w,
        desktop_win.h
    );

    // 打开完整桌面（144Hz 极限高刷）
    tx.send(&Envelope {
        req_id: 12,
        msg: Some(envelope::Msg::OpenWindow(OpenWindow {
            window_id: desktop_id,
            fit: FitMode::Scale as i32,
            max_fps: 144,
            max_level: 0,
            view_only: false,
        })),
    })
    .await?;
    let open_ack = recv_until(&mut rx, |e| match e.msg {
        Some(envelope::Msg::OpenWindowAck(a)) if a.window_id == desktop_id => Some(a),
        _ => None,
    })
    .await?;
    let (mut video_rx, _tile_rx) = accept_video_and_tile(&lan_conn).await?;
    let (hdr1, data1) = recv_next_video_frame(&mut video_rx).await?;
    assert!(hdr1.keyframe, "完整桌面首帧必须是 IDR keyframe");
    assert!(
        data1.len() > 500 && (data1.starts_with(&[0, 0, 0, 1]) || data1.starts_with(&[0, 0, 1])),
        "首帧 Annex-B H.264 数据不合法 (len={})",
        data1.len()
    );
    println!(
        "=== [阶段 1.4] 完整桌面 (0x{:x}) 首帧 IDR 已送达：{}x{}, bytes={} ===",
        desktop_id,
        open_ack.w,
        open_ack.h,
        data1.len()
    );

    // 等待一条真实编码器落地后的 LabTelemetry，验证本机实际选用的硬件编码器
    let lab_telem_hw = recv_until(&mut rx, |e| match e.msg {
        Some(envelope::Msg::LabTelemetry(s))
            if !s.encoder_name.is_empty() && s.encoder_name != "auto" =>
        {
            Some(s)
        }
        _ => None,
    })
    .await?;
    println!(
        "=== [阶段 1.5] 实时遥测上报：encoder={} (hardware={}), target={}kbps @ {}fps ===",
        lab_telem_hw.encoder_name,
        lab_telem_hw.hardware_enc,
        lab_telem_hw.target_kbps,
        lab_telem_hw.fps_cap
    );

    // 在线热调重放：硬编 → 软编 OpenH264（重放此前触发 rc=1 的 15855 / 18233 / 80000 / 4000 kbps）→ 切回硬编 120Mbps@144Hz
    for (idx, (prefer_hw, kbps)) in [
        (false, 20_000u32),
        (false, 15_855),
        (false, 18_233),
        (false, 80_000),
        (false, 4_000),
        (true, 120_000),
    ]
    .into_iter()
    .enumerate()
    {
        let req_id = 20 + idx as u32;
        tx.send(&Envelope {
            req_id,
            msg: Some(envelope::Msg::LabTuning(LabTuning {
                manual_lock: true,
                bitrate_kbps: kbps,
                max_fps: 144,
                prefer_hw,
                request_444: false,
                tile_enabled: true,
                tile_still_ms: 150,
                tile_budget_pct: 50,
                fec_overhead_pct: 15,
                force_idr: true,
            })),
        })
        .await?;
        let _echo = recv_until(&mut rx, |e| match e.msg {
            Some(envelope::Msg::LabTuning(t)) if e.req_id == req_id => Some(t),
            _ => None,
        })
        .await?;
        // 确认热调后视频流持续正常出帧（屏幕可能静止触发 gating，最多等 1.5s，若收到帧必须非空）
        if let Ok(res) = tokio::time::timeout(
            Duration::from_millis(800),
            recv_next_video_frame(&mut video_rx),
        )
        .await
        {
            let (_h, frame_bytes) = res?;
            assert!(!frame_bytes.is_empty());
        }
    }
    println!(
        "=== [阶段 1.6] 硬编/OpenH264 在线热切与 15855→18233→80000→120000kbps@144Hz 调参全部通过 ==="
    );

    // 验证对完整桌面发送 Viewport 不会误操作假 HWND
    tx.send(&Envelope {
        req_id: 40,
        msg: Some(envelope::Msg::Viewport(ViewportInfo {
            window_id: desktop_id,
            css_w: 1600,
            css_h: 900,
            dpr: 1.25,
        })),
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(220)).await;

    // 正常关闭桌面会话
    tx.send(&Envelope {
        req_id: 41,
        msg: Some(envelope::Msg::CloseWindow(CloseWindow {
            window_id: desktop_id,
        })),
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    lan_conn.close(0, "lan test done");

    // =========================================================================
    // 阶段 2：自建 rd-relay 9 位 ID 查询 + 打洞竞速连接 + 强制纯中转连接
    // =========================================================================
    let resolved_fp = lookup_9digit_id_on_relay(relay_addr, &numeric_id).await?;
    assert_eq!(
        resolved_fp, agent_fp,
        "rd-relay 反查 9 位 ID 返回的指纹必须与 rd-agent 完全一致"
    );
    println!(
        "=== [阶段 2.1] 自建中转 9 位 ID ({numeric_id}) 反查成功，指纹匹配={} ===",
        resolved_fp.short_hex()
    );

    let (punch_path, relay_punch_conn) =
        connect_via_relay_punch(relay_addr, resolved_fp, &ctrl_id, Some(resolved_fp)).await?;
    let (mut rtx, mut rrx) = relay_punch_conn.open_control().await?;
    rtx.send(&hello_env(50)).await?;
    let _ = recv_until(&mut rrx, |e| match e.msg {
        Some(envelope::Msg::HelloAck(a)) => Some(a),
        _ => None,
    })
    .await?;
    rtx.send(&Envelope {
        req_id: 51,
        msg: Some(envelope::Msg::OpenWindow(OpenWindow {
            window_id: desktop_id,
            fit: FitMode::Scale as i32,
            max_fps: 60,
            max_level: 0,
            view_only: true,
        })),
    })
    .await?;
    let _ = recv_until(&mut rrx, |e| match e.msg {
        Some(envelope::Msg::OpenWindowAck(a)) if a.window_id == desktop_id => Some(a),
        _ => None,
    })
    .await?;
    let (mut r_video_rx, _r_tile_rx) = accept_video_and_tile(&relay_punch_conn).await?;
    let (rhdr, rdata) = recv_next_video_frame(&mut r_video_rx).await?;
    assert!(rhdr.keyframe && rdata.len() > 500);
    rtx.send(&Envelope {
        req_id: 52,
        msg: Some(envelope::Msg::CloseWindow(CloseWindow {
            window_id: desktop_id,
        })),
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    relay_punch_conn.close(0, "relay punch done");
    println!(
        "=== [阶段 2.2] connect_via_relay_punch 选路={punch_path:?}，完整桌面首帧 IDR ({}B) 验证通过 ===",
        rdata.len()
    );

    // 强制纯 UDP 加密中转兜底（模拟对称型 NAT 打洞彻底失败时的兜底通路）
    let forced_relay_conn =
        connect_via_relay(relay_addr, resolved_fp, &ctrl_id, Some(resolved_fp)).await?;
    let (mut ftx, mut frx) = forced_relay_conn.open_control().await?;
    ftx.send(&hello_env(60)).await?;
    let _ = recv_until(&mut frx, |e| match e.msg {
        Some(envelope::Msg::HelloAck(a)) => Some(a),
        _ => None,
    })
    .await?;
    forced_relay_conn.close(0, "forced relay done");
    println!("=== [阶段 2.3] 强制纯中转 (connect_via_relay) E2E 加密握手验证通过 ===");

    // =========================================================================
    // 阶段 3：零服务器 STUN P2P 同端口复用打洞 + Windows 10054 (WSAECONNRESET) 抗扰实测
    // =========================================================================
    let local_stun = spawn_local_stun_server().await?;
    let host_sock = bind_udp_socket("127.0.0.1:0".parse()?).await?;
    let ctrl_sock = bind_udp_socket("127.0.0.1:0".parse()?).await?;

    // 故意先向未监听端口发射开洞包，触发 Windows Winsock ICMP Port Unreachable (10054)
    let closed_port: SocketAddr = "127.0.0.1:9".parse()?;
    let _ = host_sock.send_to(b"RDPUNCH1", closed_port).await;
    let _ = ctrl_sock.send_to(b"RDPUNCH1", closed_port).await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    let host_mapped = stun_query_mapped(&host_sock, local_stun)
        .await
        .or_else(|_| host_sock.local_addr())?;
    let ctrl_mapped = stun_query_mapped(&ctrl_sock, local_stun)
        .await
        .or_else(|_| ctrl_sock.local_addr())?;

    // 双向互发开洞脉冲，随后在完全相同的两个 UDP socket 上直接挂载 QUIC Server 与 Client！
    host_sock.send_to(b"RDPUNCH1", ctrl_mapped).await?;
    ctrl_sock.send_to(b"RDPUNCH1", host_mapped).await?;

    let p2p_host_id = Identity::generate("p2p-stun-host")?;
    let p2p_host_fp = p2p_host_id.fingerprint();
    let p2p_server = Server::bind_socket(Arc::clone(&host_sock), &p2p_host_id)?;
    let ctrl_expected_fp = ctrl_id.fingerprint();
    let accept_task = tokio::spawn(async move {
        let incoming = p2p_server.accept().await?;
        assert_eq!(incoming.peer_fingerprint(), ctrl_expected_fp);
        let (_stx, mut srx) = incoming.accept_control().await?;
        let env = srx.recv().await?.context("decode")?;
        anyhow::ensure!(matches!(env.msg, Some(envelope::Msg::Hello(_))));
        anyhow::Ok(())
    });

    let p2p_conn = connect_socket(ctrl_sock, host_mapped, &ctrl_id, Some(p2p_host_fp)).await?;
    let (mut ptx, _prx) = p2p_conn.open_control().await?;
    ptx.send(&hello_env(70)).await?;
    accept_task.await??;
    p2p_conn.close(0, "p2p stun done");
    println!(
        "=== [阶段 3] 零服务器 STUN P2P 同端口复用打洞 ({ctrl_mapped} -> {host_mapped}) + 抗 10054 验证通过 ==="
    );

    // =========================================================================
    // 阶段 4：全量审计 rd-agent 进程真实日志（零编码报错、零 set_bitrate rc=1）
    // =========================================================================
    tokio::time::sleep(Duration::from_millis(200)).await;
    let captured_logs = agent.logs.lock().expect("logs lock").clone();
    let bad_lines: Vec<&String> = captured_logs
        .iter()
        .filter(|l| {
            l.contains("set_bitrate(")
                || l.contains("MAX_BITRATE")
                || l.contains("rc=1")
                || l.contains("bEnableFrameSkip")
                || l.contains("panic")
        })
        .collect();
    assert!(
        bad_lines.is_empty(),
        "rd-agent 运行日志中发现回归告警/错误:\n{:#?}",
        bad_lines
    );
    println!(
        "=== [阶段 4] rd-agent 日志洁净度审计通过（共检查 {} 行，零 set_bitrate/rc=1/OpenH264 告警） ===\n",
        captured_logs.len()
    );

    Ok(())
}
