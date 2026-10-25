//! 合成会话引擎：loopback 双端 + 弱网仿真 + FEC 媒体路径，产出实时遥测流。
//!
//! 与 SB-Bench 共享同一套参考模块（loopback / lossy-sim / fec-*），但目标是
//! **流式遥测**（Studio 运行态可视化的数据源），不是评分。

use bytes::Bytes;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};

use switchboard_core::channel::Channel;
use switchboard_core::modules::fec::{FecConfig, FecShards};
use switchboard_core::modules::loopback::LoopbackPairTransport;
use switchboard_core::modules::lossy::{shape, LossOverride, LossyParams};
use switchboard_core::stages::{EncodedFrame, FrameIn, MediaAdapt, Transport};
use switchboard_core::telemetry::EventBus;

/// 会话参数（Studio 运行页表单直映）。
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SimParams {
    pub rtt_ms: f32,
    pub loss: f32,
    pub mbps: f32,
    pub burst_len: u32,
    /// true = sb.moonlight.fec-rs；false = sb.core.fec-xor
    pub fec_rs: bool,
    /// 运行时长（秒）；0 = 直到手动停止
    pub duration_s: f32,
}

impl Default for SimParams {
    fn default() -> Self {
        Self { rtt_ms: 40.0, loss: 0.01, mbps: 20.0, burst_len: 0, fec_rs: true, duration_s: 30.0 }
    }
}

/// 推给前端的遥测帧（JSON，tag 化枚举）。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Telem {
    Stats {
        ts_ms: f64,
        rtt_ms: f32,
        fps: f32,
        send_mbps: f32,
        recv_mbps: f32,
        loss_pct: f32,
        recovered: u64,
        dropped: u64,
        path_kind: String,
        media: String,
    },
    Event { ts_ms: f64, event: String, detail: serde_json::Value },
    Ended { reason: String, score: Option<Score> },
}

/// 会话结束的自动评分（0-100，口径：流畅 40 / 送达 30 / 延迟 20 / 抗丢包 10）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct Score {
    pub total: f32,
    pub smoothness: f32,
    pub delivery: f32,
    pub latency: f32,
    pub robustness: f32,
    pub avg_fps: f32,
    pub avg_recv_mbps: f32,
    pub avg_loss_pct: f32,
    pub frames_sent: u32,
    pub frames_recv: u32,
}

#[derive(Default)]
struct Live {
    recv_frames: AtomicU32,
    recv_bytes: AtomicU64,
    sent_frames: AtomicU32,
    sent_bytes: AtomicU64,
    recovered: AtomicU64,
    dropped: AtomicU64,
}

/// 运行中的会话句柄。
pub struct SessionHandle {
    pub id: u64,
    pub params: SimParams,
    stop_tx: watch::Sender<bool>,
    pub telemetry_rx: broadcast::Receiver<Telem>,
    /// 引擎结束置位（QA #5：列表据此过滤已结束会话）
    pub done: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionHandle {
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 启动一个合成会话；返回句柄 + 遥测广播接收端（引擎结束前持续推送）。
pub fn start(params: SimParams) -> SessionHandle {
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (stop_tx, stop_rx) = watch::channel(false);
    let (telem_tx, telem_rx) = broadcast::channel(512);
    let media_name =
        if params.fec_rs { "sb.moonlight.fec-rs" } else { "sb.core.fec-xor" }.to_string();

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            // 每个 sim 会话独立小 runtime（线程内 block_on + 自己的任务）
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("sim runtime");
            rt.block_on(run_session(params, media_name, stop_rx, telem_tx.clone()));
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            rt.shutdown_timeout(Duration::from_secs(2));
        });
    }

    SessionHandle { id, params, stop_tx, telemetry_rx: telem_rx, done }
}

async fn run_session(
    p: SimParams,
    media_name: String,
    mut stop_rx: watch::Receiver<bool>,
    telem_tx: broadcast::Sender<Telem>,
) {
    let bus = Arc::new(EventBus::new());
    let loss_override = Arc::new(LossOverride::default());
    let lb = Arc::new(LoopbackPairTransport::new(1200));
    let lp = LossyParams {
        rtt_ms: p.rtt_ms,
        jitter_ms: 3.0,
        loss_ratio: p.loss,
        mbps: p.mbps,
        mtu: 1200,
        burst_len: p.burst_len,
    };
    let transport = switchboard_core::modules::lossy::LossySimTransport {
        inner: Arc::clone(&lb) as Arc<dyn switchboard_core::stages::Transport>,
        params: lp,
        loss_override: Arc::clone(&loss_override),
        seed: 42,
    };
    let est = transport.establish(Vec::new(), Arc::clone(&bus)).await.expect("建立");
    let raw_remote = lb.take_remote().await.expect("对端");
    let remote = shape(Box::new(raw_remote), lp, Arc::clone(&loss_override), 43);
    let a: Arc<dyn Channel> = Arc::from(est);
    let b: Arc<dyn Channel> = Arc::from(remote);

    let media: Arc<dyn MediaAdapt> = if p.fec_rs {
        Arc::new(switchboard_mod_fec_rs::FecRs::new(Default::default()))
    } else {
        Arc::new(FecShards::new(FecConfig::default()))
    };
    let (mut tx, mut rx) = media.bind(Arc::clone(&a), Arc::clone(&b), Arc::clone(&bus)).await.unwrap();

    let live = Arc::new(Live::default());
    let t0 = Instant::now();

    // 事件 → 遥测 + 计数
    {
        let mut evrx = bus.subscribe();
        let live = Arc::clone(&live);
        let telem = telem_tx.clone();
        tokio::spawn(async move {
            loop {
                match evrx.recv().await {
                    Ok(switchboard_core::telemetry::TelemetryMsg::Event(e)) => {
                        match e.event.as_str() {
                            "fec_recovered" => {
                                live.recovered.fetch_add(1, Ordering::Relaxed);
                            }
                            "frame_drop" | "frame_lost" => {
                                live.dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                        let _ = telem.send(Telem::Event {
                            ts_ms: t0.elapsed().as_secs_f64() * 1000.0,
                            event: e.event.clone(),
                            detail: e.detail.clone(),
                        });
                    }
                    Ok(switchboard_core::telemetry::TelemetryMsg::Stats(_)) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }

    // 接收任务
    {
        let live = Arc::clone(&live);
        tokio::spawn(async move {
            loop {
                match rx.next().await {
                    Some(FrameIn::Complete { data, .. }) => {
                        live.recv_frames.fetch_add(1, Ordering::Relaxed);
                        live.recv_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                    Some(FrameIn::Lost { .. }) => {}
                    None => break,
                }
            }
        });
    }

    // 发送任务：60fps 12.5KB ≈ 6Mbps
    {
        let live = Arc::clone(&live);
        let stop = stop_rx.clone();
        tokio::spawn(async move {
            let mut fid = 0u32;
            loop {
                if *stop.borrow() {
                    break;
                }
                // QA #3：负载随 mbps 缩放（目标线率 ~75%），单帧上限 120KB
                let frame_bytes = ((p.mbps * 1e6 / 8.0 / 60.0) as usize).clamp(2_000, 120_000);
                let data: Vec<u8> = (0..frame_bytes).map(|x| (x as u8) ^ fid as u8).collect();
                if tx.submit(EncodedFrame { frame_id: fid, data: Bytes::from(data) }).await.is_err() {
                    break;
                }
                live.sent_frames.fetch_add(1, Ordering::Relaxed);
                live.sent_bytes.fetch_add(frame_bytes as u64, Ordering::Relaxed);
                fid += 1;
                tokio::time::sleep(Duration::from_millis(1000 / 60)).await;
            }
        });
    }

    // 统计聚合器：250ms 窗口差分 → Stats 遥测 + 平均样本累加（结束出分用）
    let accum: Arc<Mutex<(f32, f32, f32, u32)>> = Arc::new(Mutex::new((0.0, 0.0, 0.0, 0))); // (Σfps, Σrecv_mbps, Σloss_pct, n)
    {
        let live = Arc::clone(&live);
        let accum = Arc::clone(&accum);
        let stop = stop_rx.clone();
        let telem = telem_tx.clone();
        tokio::spawn(async move {
            let mut last = Instant::now();
            let (mut f0, mut b0, mut s0) = (0u32, 0u64, 0u64);
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if *stop.borrow() {
                    break;
                }
                let dt = last.elapsed().as_secs_f32().max(0.001);
                last = Instant::now();
                let f1 = live.recv_frames.load(Ordering::Relaxed);
                let b1 = live.recv_bytes.load(Ordering::Relaxed);
                let s1 = live.sent_bytes.load(Ordering::Relaxed);
                let frames_delta = f1.saturating_sub(f0);
                let loss_pct = live.dropped.load(Ordering::Relaxed) as f32
                    / (f1 as f32 + live.dropped.load(Ordering::Relaxed) as f32 + 1.0)
                    * 100.0;
                let recv_mbps = (b1.saturating_sub(b0)) as f32 * 8.0 / dt / 1e6;
                let stats_frame = Telem::Stats {
                    ts_ms: t0.elapsed().as_secs_f64() * 1000.0,
                    rtt_ms: p.rtt_ms,
                    fps: frames_delta as f32 / dt,
                    send_mbps: (s1.saturating_sub(s0)) as f32 * 8.0 / dt / 1e6,
                    recv_mbps,
                    loss_pct,
                    recovered: live.recovered.load(Ordering::Relaxed),
                    dropped: live.dropped.load(Ordering::Relaxed),
                    path_kind: "loopback".into(),
                    media: media_name.clone(),
                };
                let _ = telem.send(stats_frame);
                let mut a = accum.lock().unwrap();
                a.0 += frames_delta as f32 / dt;
                a.1 += recv_mbps;
                a.2 += loss_pct;
                a.3 += 1;
                drop(a);
                f0 = f1;
                b0 = b1;
                s0 = s1;
            }
        });
    }

    // 时长/停止控制
    let deadline =
        (p.duration_s > 0.0).then(|| tokio::time::Instant::now() + Duration::from_secs_f32(p.duration_s));
    loop {
        tokio::select! {
            _ = stop_rx.changed() => break,
            _ = async {
                if let Some(dl) = deadline {
                    tokio::time::sleep_until(dl).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => break,
        }
    }
    // 出分：按 04 篇口径的简化版（流畅 40 / 送达 30 / 延迟 20 / 抗丢包 10）
    let a_acc = accum.lock().unwrap();
    let n = (a_acc.3 as f32).max(1.0);
    let avg_fps = a_acc.0 / n;
    let avg_recv = a_acc.1 / n;
    let avg_loss = a_acc.2 / n;
    drop(a_acc);
    let frames_sent = live.sent_frames.load(Ordering::Relaxed);
    let frames_recv = live.recv_frames.load(Ordering::Relaxed);
    let smoothness = (avg_fps / 60.0).min(1.0) * 100.0;
    let delivery = if frames_sent > 0 {
        (frames_recv as f32 / frames_sent as f32).min(1.0) * 100.0
    } else {
        0.0
    };
    let latency = if p.rtt_ms <= 50.0 {
        100.0
    } else if p.rtt_ms >= 400.0 {
        0.0
    } else {
        100.0 - (p.rtt_ms - 50.0) / 350.0 * 100.0
    };
    // QA #低：全丢包（delivery≈0）时抗丢包不应拿满分——与送达率联动
    let robustness = (100.0 - avg_loss * 10.0).clamp(0.0, 100.0) * (delivery / 100.0);
    let score = Score {
        total: smoothness * 0.4 + delivery * 0.3 + latency * 0.2 + robustness * 0.1,
        smoothness,
        delivery,
        latency,
        robustness,
        avg_fps,
        avg_recv_mbps: avg_recv,
        avg_loss_pct: avg_loss,
        frames_sent,
        frames_recv,
    };
    let _ = telem_tx.send(Telem::Ended {
        reason: "finished".into(),
        score: Some(score),
    });
    a.close();
    b.close();
}
