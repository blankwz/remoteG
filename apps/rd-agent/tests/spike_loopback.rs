//! T-006 spike 管线回环验证（临时验证代码，M1 正式管线落地后可删）。
//!
//! - `synthetic_loopback`：合成帧 → `create_best` 编码 → `DataTx::send_video`，
//!   对端 `connect` + `accept_data` + `recv`，断言首帧为关键帧、seq 单调、帧持续到达。
//! - `live_spike_loopback`（`#[ignore]`，需外部环境）：连接已运行的
//!   `rd-agent spike`（默认 `127.0.0.1:21118`）实收视频流采样断言。
//!   运行方式见该测试注释。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rd_encode::frame::RawFrame;
use rd_encode::{create_best, EncoderConfig};
use rd_transport::{connect, DataFrame, Identity, Server, StreamKind, VideoHeader};

fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// 合成 BGRA 帧：竖条纹强度随 `n` 递增，保证每帧都有内容可压、编码器持续出 AU。
fn synthetic_frame(width: u32, height: u32, n: u64) -> RawFrame {
    let stride = (width * 4) as i32;
    let mut data = vec![0u8; stride as usize * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let v = ((x / 16 + n as usize * 7) % 256) as u8;
            let off = y * stride as usize + x * 4;
            data[off] = v;
            data[off + 1] = v.wrapping_add(16);
            data[off + 2] = v.wrapping_add(32);
            data[off + 3] = 255;
        }
    }
    RawFrame {
        width,
        height,
        stride,
        pts_us: unix_us(),
        data,
    }
}

/// 合成视频回环：编码器 + QUIC 视频流（spike 管线的 编码→传输 段）端到端走通。
#[tokio::test(flavor = "multi_thread")]
async fn synthetic_loopback() -> Result<()> {
    let agent_id = Identity::generate("spike-test-agent")?;
    let client_id = Identity::generate("spike-test-client")?;

    let server = Server::bind("127.0.0.1:0".parse()?, &agent_id)?;
    let addr = server.local_addr();

    // Agent 侧：编码合成视频并发送（对应 spike.rs 的 encode_loop + send_loop）。
    let agent_task = tokio::spawn(async move {
        let conn = server.accept().await?;
        let mut data_tx = conn.open_data(StreamKind::Video).await?;
        // 用 OpenH264（prefer_hw=false）保证确定性：每帧产出 AU，首帧必为 IDR。
        let cfg = EncoderConfig {
            width: 320,
            height: 240,
            fps: 60,
            bitrate_kbps: 8000,
            prefer_hw: false,
        };
        let mut enc = create_best(&cfg)?;
        let mut seq = 0u32;
        let mut sent = 0u32;
        for n in 0..120u64 {
            let raw = synthetic_frame(320, 240, n);
            if let Some(au) = enc.encode(&raw, n == 0)? {
                let h = VideoHeader {
                    window_id: 42,
                    seq,
                    pts_us: au.pts_us,
                    keyframe: au.keyframe,
                };
                data_tx.send_video(&h, &au.data).await?;
                seq += 1;
                sent += 1;
                if sent >= 20 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        data_tx.finish()?;
        // 等对端收完再关连接。
        tokio::time::sleep(Duration::from_millis(300)).await;
        conn.close(0, "done");
        anyhow::Ok(sent)
    });

    // 控制端侧：连接并接收视频流。
    let conn = connect(addr, &client_id, None).await?;
    let (kind, _stream_id, mut data_rx) = conn.accept_data().await?;
    assert_eq!(kind, StreamKind::Video);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut first: Option<VideoHeader> = None;
    let mut frames = 0u32;
    let mut last_seq: Option<u32> = None;
    while frames < 10 && Instant::now() < deadline {
        let Some(frame) = tokio::time::timeout(Duration::from_secs(5), data_rx.recv()).await??
        else {
            break;
        };
        match frame {
            DataFrame::Video(h, payload) => {
                if frames == 0 {
                    assert!(h.keyframe, "收到的首个视频帧必须是关键帧（IDR）");
                    first = Some(h);
                }
                assert!(!payload.is_empty(), "AU 负载不能为空");
                if let Some(prev) = last_seq {
                    assert_eq!(h.seq, prev + 1, "seq 必须连续递增");
                }
                last_seq = Some(h.seq);
                frames += 1;
            }
            other => panic!("期望视频帧，实际收到 {other:?}"),
        }
    }
    assert!(frames >= 10, "10 秒内应收满 10 帧，实际 {frames} 帧");
    assert_eq!(first.expect("首帧必已收到").window_id, 42);

    let sent = agent_task.await??;
    assert!(sent >= frames, "发送帧数 {sent} 应不少于接收帧数 {frames}");
    Ok(())
}

/// 连接外部已运行的 `rd-agent spike`（含被捕获窗口，如 clarity-bench testwin），
/// 实收约 8 秒视频流并断言：首帧为关键帧、帧持续到达、连接中 resize 后出现新 IDR。
///
/// 运行（需先启动 clarity-bench 与 rd-agent spike）：
/// `cargo test -p rd-agent --test spike_loopback live_spike_loopback -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要外部运行 rd-agent spike（监听 127.0.0.1:21118）与被捕获窗口"]
async fn live_spike_loopback() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();

    let addr: std::net::SocketAddr = "127.0.0.1:21118".parse()?;
    let id = Identity::generate("spike-live-client")?;
    let conn = connect(addr, &id, None).await?;
    let (kind, _, mut data_rx) = conn.accept_data().await?;
    assert_eq!(kind, StreamKind::Video);

    // 连接进行中触发窗口 resize（验证 spike 的尺寸变化 -> reconfigure -> 新 IDR 路径）。
    // 脚本：同目录 resize-testwin.ps1（Windows 专用，仅本机验证用）。
    let resize_script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resize-testwin.ps1");
    let resize_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let out = tokio::process::Command::new(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        )
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&resize_script)
        .args(["640", "480"])
        .output()
        .await;
        match out {
            Ok(o) => tracing::info!(
                stdout = String::from_utf8_lossy(&o.stdout).trim().to_string(),
                "已触发连接中 resize"
            ),
            Err(e) => tracing::warn!("触发 resize 失败（跳过 reconfigure 验证）: {e}"),
        }
    });

    let started = Instant::now();
    let deadline = started + Duration::from_secs(8);
    let (mut frames, mut keyframes, mut first_key, mut bytes) = (0u32, 0u32, false, 0u64);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), data_rx.recv()).await {
            Err(_elapsed) => break, // 2 秒无帧（窗口静止）即认为采样结束
            Ok(Err(e)) => return Err(e),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(DataFrame::Video(h, payload)))) => {
                if frames == 0 {
                    first_key = h.keyframe;
                    tracing::info!(keyframe = h.keyframe, "首帧到达");
                }
                keyframes += u32::from(h.keyframe);
                frames += 1;
                bytes += payload.len() as u64;
            }
            Ok(Ok(Some(other))) => panic!("期望视频帧，实际收到 {other:?}"),
        }
    }
    let elapsed = started.elapsed();
    resize_task.await.expect("resize 任务 panic");
    assert!(frames > 0, "未收到任何视频帧");
    assert!(first_key, "收到的首个视频帧必须是关键帧（IDR）");
    // 连接中 resize 会触发 reconfigure + 强制 IDR，故采样期内应出现 >=2 个关键帧。
    assert!(
        keyframes >= 2,
        "连接中 resize 后应出现新 IDR，实际关键帧数 {keyframes}"
    );
    println!(
        "live_spike_loopback: frames={frames} keyframes={keyframes} bytes={bytes} elapsed={elapsed:?}"
    );
    Ok(())
}
