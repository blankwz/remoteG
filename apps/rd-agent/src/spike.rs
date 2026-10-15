//! M0 Spike：capture → encode → transport 端到端延迟管线（T-006）。
//!
//! `rd-agent spike --title <子串>`：按标题子串找到窗口 → WGC 捕获客户区 →
//! H.264 编码（首帧强制 IDR，resize 时 `reconfigure`）→ QUIC 单向视频流发给
//! 首个连接的客户端；每 2 秒打印管线统计。断开后回到 accept 循环，Ctrl+C 退出。
//!
//! 仅为 Spike：无鉴权流程、无控制流、串行服务单连接；M1 正式管线由 T-016 重写复用组件。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use rd_capture_win::{CaptureOptions, CapturedFrame, WindowCapture};
use rd_encode::frame::RawFrame;
use rd_encode::{create_best, EncodedAu, EncoderConfig, VideoEncoder};
use rd_transport::{Conn, DataTx, Identity, Server, StreamKind, VideoHeader};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};

/// 统计打印周期。
const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// spike 子命令参数。
#[derive(Args, Debug)]
pub struct SpikeArgs {
    /// 目标窗口标题子串（大小写不敏感，取第一个匹配的可见顶层窗口）。
    #[arg(long)]
    pub title: String,
    /// 监听地址。
    #[arg(long, default_value = "0.0.0.0:21118")]
    pub listen: SocketAddr,
    /// 目标帧率。
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// 目标码率（kbps）。
    #[arg(long, default_value_t = 8000)]
    pub kbps: u32,
}

/// 管线各阶段计数（跨线程原子累计；stats 任务每 2 秒取增量计算速率）。
#[derive(Debug, Default)]
struct PipeStats {
    /// 编码线程收到的捕获帧数。
    captured: AtomicU64,
    /// 编码输出的访问单元数（`EncodedAu` 非 `None`）。
    encoded: AtomicU64,
    /// 编码耗时累计（微秒）。
    encode_us: AtomicU64,
    /// 发送字节数（25 字节帧头 + AU 负载）。
    sent_bytes: AtomicU64,
}

/// spike 子命令入口：初始化日志，进入 tokio runtime。
pub fn run(args: SpikeArgs) -> Result<()> {
    init_tracing();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("创建 tokio runtime 失败")?;
    let result = rt.block_on(run_async(args));
    rt.shutdown_timeout(Duration::from_secs(1));
    result
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// accept 循环：服务每个客户端直到断开，Ctrl+C 优雅退出。
async fn run_async(args: SpikeArgs) -> Result<()> {
    let identity = Identity::generate("rd-agent-spike").context("生成 Agent 身份失败")?;
    tracing::info!(
        fingerprint = identity.fingerprint().short_hex(),
        "Agent 身份已生成（spike 阶段不做指纹预校验）"
    );

    let server = Server::bind(args.listen, &identity)
        .with_context(|| format!("监听 {} 失败", args.listen))?;
    tracing::info!(
        listen = %server.local_addr(),
        title = %args.title,
        fps = args.fps,
        kbps = args.kbps,
        "spike 服务就绪，等待控制端连接（Ctrl+C 退出）"
    );

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            accepted = server.accept() => {
                let conn = accepted.context("接受连接失败")?;
                tracing::info!(peer = conn.peer_fingerprint().short_hex(), "客户端已接入");
                if let Err(e) = serve_conn(&conn, &args).await {
                    tracing::warn!("会话结束: {e:#}");
                }
                tracing::info!("连接断开，回到等待状态");
            }
            _ = ctrl_c.as_mut() => {
                tracing::info!("收到 Ctrl+C，退出");
                return Ok(());
            }
        }
    }
}

/// 为一个连接服务完整视频会话：找窗口 → 捕获 → 编码线程 → 发送任务 → 统计任务。
/// 任一环节结束（连接断开 / 捕获停止 / 编码退出）都收敛退出，由调用方回到 accept 循环。
async fn serve_conn(conn: &Conn, args: &SpikeArgs) -> Result<()> {
    let hwnd = find_window(&args.title)?;
    tracing::info!(hwnd = format!("{hwnd:#x}"), "已锁定捕获窗口");

    let (capture, frame_rx) = WindowCapture::start(hwnd, CaptureOptions { max_fps: args.fps })
        .context("启动窗口捕获失败")?;

    let stats = Arc::new(PipeStats::default());
    // 编码线程 → 发送任务。有界通道形成背压；连接断开时接收端关闭，编码线程随之退出。
    let (au_tx, au_rx) = tokio::sync::mpsc::channel::<EncodedAu>(64);

    let enc_stats = Arc::clone(&stats);
    let (encode_fps, encode_kbps) = (args.fps, args.kbps);
    let encode_thread = std::thread::Builder::new()
        .name("spike-encode".into())
        .spawn(move || encode_loop(frame_rx, au_tx, enc_stats, encode_fps, encode_kbps))
        .context("启动编码线程失败")?;

    let mut data_tx = conn
        .open_data(StreamKind::Video)
        .await
        .context("打开视频数据流失败")?;

    let send_stats = Arc::clone(&stats);
    let send_task =
        tokio::spawn(async move { send_loop(&mut data_tx, au_rx, send_stats, hwnd as u64).await });

    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let stats_task = tokio::spawn(stats_loop(conn.clone(), Arc::clone(&stats), done_rx));

    // 阻塞到发送任务结束（编码线程退出 → 通道关闭，或连接断开 → send 出错）。
    let send_result = send_task.await.context("发送任务 panic")?;
    let _ = done_tx.send(true);
    stats_task.await.context("统计任务 panic")?;

    // 先停捕获（保证编码线程的 recv 不再无限等待），再 join 编码线程。
    capture.stop();
    tokio::task::spawn_blocking(move || encode_thread.join())
        .await
        .context("join 编码线程失败")?
        .map_err(|_| anyhow!("编码线程 panic"))?;
    send_result
}

/// 编码线程主循环：捕获帧 → `RawFrame`(BGRA) → H.264 AU → 发送通道。
///
/// 首帧按实际客户区尺寸创建最优编码器并强制 IDR；尺寸变化时 `reconfigure`
/// 并强制 IDR。退出条件：捕获通道关闭、发送端关闭、编码出错（记 error 后退出）。
fn encode_loop(
    frame_rx: crossbeam_channel::Receiver<CapturedFrame>,
    au_tx: tokio::sync::mpsc::Sender<EncodedAu>,
    stats: Arc<PipeStats>,
    fps: u32,
    kbps: u32,
) {
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut force_key = true;
    let mut last_size: Option<(u32, u32)> = None;

    loop {
        let frame = match frame_rx.recv() {
            Ok(f) => f,
            Err(_) => return, // 捕获会话已关闭
        };
        stats.captured.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            w = frame.width,
            h = frame.height,
            resized = frame.resized,
            readback_us = frame.readback.as_micros() as u64,
            "捕获帧"
        );

        let enc = match encoder.as_mut() {
            Some(enc) => enc,
            None => {
                let cfg = EncoderConfig {
                    width: frame.width,
                    height: frame.height,
                    fps,
                    bitrate_kbps: kbps,
                    prefer_hw: true,
                    pixel_format: rd_encode::PixelFormat::default(),
                };
                match create_best(&cfg) {
                    Ok(enc) => {
                        last_size = Some((frame.width, frame.height));
                        encoder.insert(enc)
                    }
                    Err(e) => {
                        tracing::error!("创建编码器失败: {e:#}");
                        return;
                    }
                }
            }
        };

        // 尺寸变化检测：以帧的实际客户区尺寸为准（`CapturedFrame.resized` 基于
        // 捕获纹理尺寸（resized 信号修复前的历史绕过，保留作双保险——见
        // 这里对帧尺寸本身做差量检测，二者任一为真都触发 reconfigure + IDR）。
        let size_changed = frame.resized || last_size != Some((frame.width, frame.height));
        if size_changed {
            tracing::info!(
                width = frame.width,
                height = frame.height,
                "窗口尺寸变化，编码器 reconfigure（下一帧强制 IDR）"
            );
            if let Err(e) = enc.reconfigure(frame.width, frame.height) {
                tracing::error!(
                    width = frame.width,
                    height = frame.height,
                    "编码器 reconfigure 失败: {e:#}"
                );
                return;
            }
            force_key = true;
        }
        last_size = Some((frame.width, frame.height));

        let raw = RawFrame {
            width: frame.width,
            height: frame.height,
            stride: frame.stride as i32,
            pts_us: unix_us(),
            data: frame.data,
        };

        let t0 = Instant::now();
        let au = match enc.encode(&raw, force_key) {
            Ok(au) => au,
            Err(e) => {
                tracing::error!("编码失败: {e:#}");
                return;
            }
        };
        force_key = false;
        stats
            .encode_us
            .fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);

        // h264_mf 等编码器有内部缓冲，部分帧无输出，跳过即可。
        if let Some(au) = au {
            stats.encoded.fetch_add(1, Ordering::Relaxed);
            if au_tx.blocking_send(au).is_err() {
                return; // 发送任务已退出（连接断开）
            }
        }
    }
}

/// 发送循环：编码 AU → `VideoHeader` + Annex-B → QUIC 数据流。
/// 连接断开（send 出错）时返回 Err，携带错误向上收敛会话。
async fn send_loop(
    data_tx: &mut DataTx,
    mut au_rx: tokio::sync::mpsc::Receiver<EncodedAu>,
    stats: Arc<PipeStats>,
    window_id: u64,
) -> Result<()> {
    let mut seq: u32 = 0;
    while let Some(au) = au_rx.recv().await {
        let header = VideoHeader {
            window_id,
            seq,
            pts_us: au.pts_us,
            keyframe: au.keyframe,
        };
        if let Err(e) = data_tx.send_video(&header, &au.data).await {
            return Err(e.context("发送视频帧失败（连接断开？）"));
        }
        seq = seq.wrapping_add(1);
        stats
            .sent_bytes
            .fetch_add(au.data.len() as u64 + 25, Ordering::Relaxed);
    }
    Ok(())
}

/// 统计循环：每 2 秒输出捕获 fps、平均编码耗时、发送字节率、QUIC RTT。
async fn stats_loop(
    conn: Conn,
    stats: Arc<PipeStats>,
    mut done: tokio::sync::watch::Receiver<bool>,
) {
    #[derive(Default)]
    struct Snap {
        captured: u64,
        encoded: u64,
        encode_us: u64,
        sent_bytes: u64,
    }
    let load = |s: &PipeStats| Snap {
        captured: s.captured.load(Ordering::Relaxed),
        encoded: s.encoded.load(Ordering::Relaxed),
        encode_us: s.encode_us.load(Ordering::Relaxed),
        sent_bytes: s.sent_bytes.load(Ordering::Relaxed),
    };
    let mut prev = load(&stats);
    let mut interval = tokio::time::interval(STATS_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = done.changed() => return,
        }
        let cur = load(&stats);
        let secs = STATS_INTERVAL.as_secs_f64();
        let cap_fps = (cur.captured - prev.captured) as f64 / secs;
        let enc_count = cur.encoded - prev.encoded;
        let enc_avg_ms = if enc_count > 0 {
            (cur.encode_us - prev.encode_us) as f64 / enc_count as f64 / 1000.0
        } else {
            0.0
        };
        let out_kbps = (cur.sent_bytes - prev.sent_bytes) as f64 * 8.0 / secs / 1000.0;
        tracing::info!(
            cap_fps = format!("{cap_fps:.1}"),
            enc_avg_ms = format!("{enc_avg_ms:.2}"),
            out_kbps = format!("{out_kbps:.0}"),
            rtt_ms = format!("{:.1}", conn.rtt().as_secs_f64() * 1000.0),
            "管线统计"
        );
        prev = cur;
    }
}

/// 按标题子串（大小写不敏感）查找第一个可见顶层窗口，返回 HWND（isize）。
fn find_window(title: &str) -> Result<isize> {
    struct Ctx {
        needle: String,
        found: Option<isize>,
    }
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() {
            return windows::core::BOOL(1);
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return windows::core::BOOL(1);
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        let text = if copied > 0 {
            String::from_utf16_lossy(&buf[..copied as usize])
        } else {
            String::new()
        };
        if text.to_lowercase().contains(&ctx.needle) {
            tracing::debug!(hwnd = format!("{:#x}", hwnd.0 as usize), %text, "匹配到窗口");
            ctx.found = Some(hwnd.0 as isize);
            return windows::core::BOOL(0); // 停止枚举
        }
        windows::core::BOOL(1)
    }

    let mut ctx = Ctx {
        needle: title.to_lowercase(),
        found: None,
    };
    // 注意：回调用 BOOL(0) 中止枚举（找到目标）时 EnumWindows 返回 0，而
    // GetLastError 是线程上的残留值（如 E_HANDLE），windows-rs 会把它转成
    // Err。因此这里不能信 EnumWindows 的 Result，只以回调写回的 found 为准。
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.found
        .ok_or_else(|| anyhow!("未找到标题包含 {title:?} 的可见顶层窗口"))
}

/// Unix epoch 起的微秒时间戳（发送端时钟；控制端据此估算端到端延迟）。
fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
