//! SB-Bench v0.1.2：T0/T3 等效/T4 剖面（spec/04）的基准与评分实现。
//!
//! 用法：
//! ```text
//! cargo run -p switchboard-bench -- --profile T0 --fast --out bench.json
//! cargo run -p switchboard-bench -- --profile T3 --rtt-ms 80 --loss 0.02 --mbps 10
//! cargo run -p switchboard-bench -- --profile T4 --media sb.moonlight.fec-rs
//! ```
//!
//! 剖面语义（04 §1）：
//! - `T0`：环回 + 可选弱网仿真（回归用，×0.9）；
//! - `T3`：**双口径**——均匀随机丢包 + 突发丢包各跑一轮，综合分取均值 ×0.9
//!   （04 §4 修订：防 FEC 被均匀口径高估）；
//! - `T4`：强制中继——所有信道以 `path.kind = relay` 如实标注（06 §1 探针口径），
//!   并叠加中继跳延迟 40ms。
//!
//! v0.1.2：W1 升级为**字模渲染 + 最近邻识别**（合成 OCR）——发送端把字符渲染成
//! 5×7 位图，接收端按模板最近邻解码，损坏分片直接体现为误识字符。

use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use switchboard_core::channel::Channel;
use async_trait::async_trait;
use switchboard_core::executor::{execute, ModuleProvider};
use switchboard_core::modules::fec::{FecConfig, FecShards};
use switchboard_core::modules::lanbcast::LanBroadcast;
use switchboard_core::modules::loopback::LoopbackPairTransport;
use switchboard_core::modules::lossy::{shape, LossOverride, LossyParams, LossySimTransport};
use switchboard_core::modules::punch_stub::{DerpRelayStub, RustDeskPunchStub};
use switchboard_core::plan::Plan;
use switchboard_core::stages::{
    Discoverer, EncodedFrame, FrameIn, FrameSink, MediaAdapt, Puncher, Relay, Transport,
};
use switchboard_core::telemetry::{EventBus, PathTag, StageStats};
use switchboard_core::types::PathKind;

// ── 模块供给（harness 版 ModuleProvider） ──

struct CoreProvider {
    loopback: Arc<LoopbackPairTransport>,
    lossy: LossyParams,
    loss_override: Arc<LossOverride>,
    seed: u64,
}

impl ModuleProvider for CoreProvider {
    fn discoverer(&self, id: &str) -> Option<Arc<dyn Discoverer>> {
        (id == "sb.lan.udp-broadcast")
            .then(|| Arc::new(LanBroadcast::default()) as Arc<dyn Discoverer>)
    }
    fn puncher(&self, id: &str) -> Option<Arc<dyn Puncher>> {
        (id == "sb.rustdesk.punch-v2").then(|| Arc::new(RustDeskPunchStub) as Arc<dyn Puncher>)
    }
    fn relay(&self, id: &str) -> Option<Arc<dyn Relay>> {
        (id == "sb.derp.self-hk").then(|| Arc::new(DerpRelayStub) as Arc<dyn Relay>)
    }
    fn transport(&self, id: &str) -> Option<Arc<dyn Transport>> {
        (id == "sb.core.loopback").then(|| {
            Arc::new(LossySimTransport {
                inner: Arc::clone(&self.loopback) as Arc<dyn Transport>,
                params: self.lossy,
                loss_override: Arc::clone(&self.loss_override),
                seed: self.seed,
            }) as Arc<dyn Transport>
        })
    }
}

/// T4 用：把内层信道的 path.kind 如实改标为 relay（06 §1 防谎报口径，
/// 仅用于 T4 剖面——这里是真实语义：T4 = 强制走中继路径）。
struct RelayLabeledChannel {
    inner: Box<dyn Channel>,
}

#[async_trait]
impl Channel for RelayLabeledChannel {
    fn caps(&self) -> &switchboard_core::caps::ChannelCaps {
        self.inner.caps()
    }
    fn conn_id(&self) -> &str {
        self.inner.conn_id()
    }
    async fn send_dgram(&self, b: Bytes) -> Result<(), switchboard_core::channel::SendError> {
        self.inner.send_dgram(b).await
    }
    async fn recv_dgram(&self) -> Result<Bytes, switchboard_core::channel::RecvError> {
        self.inner.recv_dgram().await
    }
    async fn open_stream(
        &self,
        k: switchboard_core::channel::StreamKind,
    ) -> Result<Box<dyn switchboard_core::channel::Stream>, switchboard_core::channel::ChannelError> {
        self.inner.open_stream(k).await
    }
    async fn accept_stream(
        &self,
    ) -> Result<
        (switchboard_core::channel::StreamKind, Box<dyn switchboard_core::channel::Stream>),
        switchboard_core::channel::ChannelError,
    > {
        self.inner.accept_stream().await
    }
    fn stats(&self) -> StageStats {
        let mut s = self.inner.stats();
        s.path = PathTag { kind: PathKind::Relay, local: None, remote: None };
        s.module = "sb.core.relay-loopback".into();
        s
    }
    fn close(&self) {
        self.inner.close();
    }
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

// ── 采样 ──

#[derive(Clone, Debug)]
struct Rec {
    fid: u32,
    lat_ms: f32,
    ok: bool,
    t_ms: f32,
    payload: Option<Bytes>,
}

#[derive(Default)]
struct Samples {
    sent: std::sync::Mutex<HashMap<u32, Instant>>,
    recvs: std::sync::Mutex<Vec<Rec>>,
}

fn pct(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f32 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 04 §4 锚点线性插值归一（0-100，锚点 (指标值, 分数) 升序）。
fn normalize(v: f32, anchors: &[(f32, f32)]) -> f32 {
    if v <= anchors[0].0 {
        return anchors[0].1;
    }
    for w in anchors.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        if v <= x1 {
            return y0 + (v - x0) * (y1 - y0) / (x1 - x0);
        }
    }
    anchors[anchors.len() - 1].1
}

const A_LATENCY: &[(f32, f32)] =
    &[(50.0, 100.0), (80.0, 80.0), (120.0, 60.0), (200.0, 40.0), (400.0, 0.0)];
const A_FPS_LOW: &[(f32, f32)] = &[(57.0, 100.0), (50.0, 80.0), (45.0, 60.0), (38.0, 40.0), (30.0, 0.0)];
const A_RECOVERY: &[(f32, f32)] =
    &[(100.0, 100.0), (250.0, 80.0), (500.0, 60.0), (1500.0, 40.0), (3000.0, 0.0)];
const A_STARTUP: &[(f32, f32)] =
    &[(100.0, 100.0), (250.0, 80.0), (500.0, 60.0), (1000.0, 40.0), (2000.0, 0.0)];
/// M5 文字正确率锚点（04 §4；百分数 0-100）。
const A_M5: &[(f32, f32)] = &[(99.9, 100.0), (99.5, 80.0), (99.0, 60.0), (98.0, 40.0), (95.0, 0.0)];

/// fps 1% low（04 §3 M3）。
fn fps_1p_low(gaps_ms: &[f32]) -> f32 {
    if gaps_ms.is_empty() {
        return 0.0;
    }
    let mut sorted = gaps_ms.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let tail_n = sorted.len().min(1 + sorted.len() / 100);
    let tail = &sorted[..tail_n];
    1000.0 / (tail.iter().sum::<f32>() / tail.len() as f32)
}

#[derive(Clone, Copy, Debug, Default)]
struct WindowMetrics {
    lat_p95: f32,
    fps_low: f32,
    lost: u64,
}

fn window_metrics(recs: &[Rec], fid_range: (u32, u32)) -> WindowMetrics {
    let mut lats: Vec<f32> = Vec::new();
    let mut times: Vec<f32> = Vec::new();
    let mut lost = 0u64;
    for r in recs {
        if r.fid < fid_range.0 || r.fid >= fid_range.1 {
            continue;
        }
        if r.ok && r.lat_ms < f32::MAX {
            lats.push(r.lat_ms);
            times.push(r.t_ms);
        } else {
            lost += 1;
        }
    }
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let gaps: Vec<f32> = times.windows(2).map(|w| w[1] - w[0]).collect();
    let _fps_mean = if times.len() >= 2 {
        let span = times[times.len() - 1] - times[0];
        if span > 0.0 { (times.len() as f32 - 1.0) * 1000.0 / span } else { 0.0 }
    } else {
        0.0
    };
    let _ = pct; // lat_p50 保留口径，暂未消费
    WindowMetrics {
        lat_p95: pct(&lats, 0.95),
        fps_low: fps_1p_low(&gaps),
        lost,
    }
}

/// M4 recovery（04 §3 修订）：t0 = fps 首次跌破基线 ×0.9；全程未跌破 → 0（豁免）。
fn recovery_ms(recs: &[Rec], fid_range: (u32, u32), burst_start_ms: f32, burst_end_ms: f32) -> f32 {
    let in_range: Vec<&Rec> = recs
        .iter()
        .filter(|r| r.fid >= fid_range.0 && r.fid < fid_range.1 && r.ok && r.lat_ms < f32::MAX)
        .collect();
    let pre: Vec<f32> =
        in_range.iter().filter(|r| r.t_ms < burst_start_ms).map(|r| r.t_ms).collect();
    let mut pre_gaps: Vec<f32> = pre.windows(2).map(|w| w[1] - w[0]).collect();
    if pre_gaps.is_empty() {
        return 3000.0;
    }
    pre_gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = pre_gaps[pre_gaps.len() / 2].max(1.0);
    let pre_fps = if pre.len() >= 2 {
        (pre.len() as f32 - 1.0) * 1000.0 / (pre[pre.len() - 1] - pre[0])
    } else {
        0.0
    };
    let burst_frames =
        in_range.iter().filter(|r| r.t_ms >= burst_start_ms && r.t_ms < burst_end_ms).count() as f32;
    let burst_span = (burst_end_ms - burst_start_ms).max(1.0);
    let burst_fps = burst_frames * 1000.0 / burst_span;
    if pre_fps > 0.0 && burst_fps >= pre_fps * 0.9 {
        return 0.0; // 豁免
    }
    let post: Vec<&Rec> = in_range.iter().filter(|r| r.t_ms >= burst_end_ms).copied().collect();
    for w in post.windows(6) {
        let ok = (1..6).all(|k| w[k].t_ms - w[k - 1].t_ms <= med * 2.0);
        if ok {
            return (w[1].t_ms - burst_end_ms).max(0.0);
        }
    }
    3000.0
}

// ── W1：字模渲染 + 最近邻识别（合成 OCR） ──

const GLYPH_ROWS: usize = 7;
const GLYPH_BYTES: usize = GLYPH_ROWS; // 每行 1 字节（低 5 位有效）
const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,:!?-";

/// 字模：由 (字符, 行号) 确定性生成（合成字体；发送/接收同源模板）。
fn glyph_template(ch: u8, row: usize) -> u8 {
    let seed = (ch as u32).wrapping_mul(2654435761).wrapping_add(row as u32 * 0x9E37);
    let mut x = seed ^ (seed >> 15);
    x = x.wrapping_mul(0x2545F491);
    x ^= x >> 13;
    (x & 0x1F) as u8 // 5 列
}

/// 文本帧：K 个字符逐个渲染为位图行（无明文，接收端须"识别"）。
fn render_text_frame(fid: u32, glyph_count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(glyph_count * GLYPH_BYTES);
    for g in 0..glyph_count {
        let ch = CHARSET[(fid as usize * 13 + g * 7) % CHARSET.len()];
        for row in 0..GLYPH_ROWS {
            out.push(glyph_template(ch, row));
        }
    }
    out
}

/// 识别：逐字模最近邻模板匹配，返回正确率。
fn recognize_text_frame(data: &[u8], glyph_count: usize) -> f32 {
    if data.len() < glyph_count * GLYPH_BYTES {
        return 0.0;
    }
    let mut correct = 0usize;
    for g in 0..glyph_count {
        let bitmap = &data[g * GLYPH_BYTES..(g + 1) * GLYPH_BYTES];
        let best_dist: usize = CHARSET
            .iter()
            .map(|ch| {
                (0..GLYPH_ROWS)
                    .map(|row| (bitmap[row] ^ glyph_template(*ch, row)).count_ones() as usize)
                    .sum()
            })
            .min()
            .unwrap_or(usize::MAX);
        if best_dist == 0 {
            correct += 1;
        }
    }
    correct as f32 / glyph_count as f32 * 100.0
}

// ── 评分输出 ──

#[derive(Clone, Copy, Debug, serde::Serialize)]
struct PassParams {
    rtt_ms: f32,
    loss: f32,
    mbps: f32,
    mtu: u16,
    burst_len: u32,
    /// T4：path.kind 如实标注 relay + 中继跳延迟
    relay_label: bool,
    seed: u64,
}

struct BenchCfg {
    media: String,
    fast: bool,
    passes: Vec<PassParams>,
}

async fn run_pass(cfg: &BenchCfg, pass: PassParams, run: usize) -> serde_json::Value {
    let fast = cfg.fast;
    let plan_src = "spec = \"switchboard/0.1\"\nname = \"sbbench\"\n\n[[path]]\nname = \"loopback\"\ntransport = \"sb.core.loopback\"\n";
    let plan = Plan::from_toml(plan_src).expect("内置计划必须合法");
    let fps = 60u32;
    let w1_dur = if fast { 2.0 } else { 60.0 };
    let w2_dur = if fast { 2.0 } else { 60.0 };
    let w3_dur = if fast { 3.0 } else { 120.0 };
    let w6_dur = if fast { 6.0 } else { 15.0 };
    let w6_burst = if fast { 0.5 } else { 3.0 };

    let bus = Arc::new(EventBus::new());
    let loss_override = Arc::new(LossOverride::default());
    let prov = CoreProvider {
        loopback: Arc::new(LoopbackPairTransport::new(pass.mtu)),
        lossy: LossyParams {
            rtt_ms: pass.rtt_ms + if pass.relay_label { 40.0 } else { 0.0 },
            jitter_ms: 5.0,
            loss_ratio: pass.loss,
            mbps: pass.mbps,
            mtu: pass.mtu,
            burst_len: pass.burst_len,
        },
        loss_override: Arc::clone(&loss_override),
        seed: 1000 + run as u64 + (pass.burst_len as u64) * 7,
    };

    // W7 启动
    let t0 = Instant::now();
    let est = execute(&plan, &prov, Arc::clone(&bus)).await.unwrap_or_else(|e| panic!("建立失败: {e}"));
    let raw_remote = prov.loopback.take_remote().await.expect("对端存在");
    let mut remote = shape(Box::new(raw_remote), prov.lossy, Arc::clone(&loss_override), prov.seed + 500);
    if pass.relay_label {
        remote = Box::new(RelayLabeledChannel { inner: remote });
    }
    let startup_ms = t0.elapsed().as_millis() as f32;

    let a: Arc<dyn switchboard_core::channel::Channel> = Arc::from(est.channel);
    let b: Arc<dyn switchboard_core::channel::Channel> = Arc::from(remote);
    let fec: Arc<dyn MediaAdapt> = match cfg.media.as_str() {
        "sb.moonlight.fec-rs" => Arc::new(switchboard_mod_fec_rs::FecRs::new(
            switchboard_mod_fec_rs::FecRsConfig::default(),
        )),
        _ => Arc::new(FecShards::new(FecConfig::default())),
    };
    let (mut tx, rx) = fec.bind(a, b, Arc::clone(&bus)).await.unwrap();

    let samples = Arc::new(Samples::default());
    let total_dur = w1_dur + w2_dur + w3_dur + w6_dur + w6_burst + 1.0;
    let s_drain = Arc::clone(&samples);
    let drain = tokio::spawn(async move {
        let mut rx = rx;
        let deadline = Instant::now() + Duration::from_secs_f32(total_dur);
        let base = Instant::now();
        while let Ok(Some(fin)) = tokio::time::timeout_at(deadline.into(), rx.next()).await {
            let t_ms = base.elapsed().as_secs_f32() * 1000.0;
            match fin {
                FrameIn::Complete { frame_id, data } => {
                    let lat = s_drain
                        .sent
                        .lock()
                        .unwrap()
                        .get(&frame_id)
                        .map(|t| t.elapsed().as_secs_f32() * 1000.0)
                        .unwrap_or(f32::MAX);
                    s_drain.recvs.lock().unwrap().push(Rec {
                        fid: frame_id,
                        lat_ms: lat,
                        ok: true,
                        t_ms,
                        payload: Some(data),
                    });
                }
                FrameIn::Lost { frame_id } => {
                    s_drain.recvs.lock().unwrap().push(Rec {
                        fid: frame_id,
                        lat_ms: f32::MAX,
                        ok: false,
                        t_ms,
                        payload: None,
                    });
                }
            }
        }
    });

    async fn send_window(
        tx: &mut Box<dyn FrameSink>,
        samples: &Arc<Samples>,
        dur_s: f32,
        fps: u32,
        make: impl Fn(u32) -> Vec<u8>,
        fid: &mut u32,
    ) {
        let start = Instant::now();
        let dt = Duration::from_millis(1000 / fps as u64);
        while start.elapsed().as_secs_f32() < dur_s {
            samples.sent.lock().unwrap().insert(*fid, Instant::now());
            let _ = tx.submit(EncodedFrame { frame_id: *fid, data: Bytes::from(make(*fid)) }).await;
            *fid += 1;
            tokio::time::sleep(dt).await;
        }
    }

    let glyph_count = 512; // 512×7B = 3584B/帧
    let mut fid = 0u32;
    // W1 文字：字模渲染帧 @30fps
    let w1_start = fid;
    {
        let start = Instant::now();
        let dt = Duration::from_millis(33);
        while start.elapsed().as_secs_f32() < w1_dur {
            samples.sent.lock().unwrap().insert(fid, Instant::now());
            let _ = tx
                .submit(EncodedFrame { frame_id: fid, data: Bytes::from(render_text_frame(fid, glyph_count)) })
                .await;
            fid += 1;
            tokio::time::sleep(dt).await;
        }
    }
    let w1_end = fid;
    // W2 hover 风暴
    let w2_start = fid;
    send_window(&mut tx, &samples, w2_dur, fps, |f| {
        let mut v = vec![0x00u8; if f % 10 == 0 { 80_000 } else { 8_000 }];
        v[0] = 0xAB;
        v
    }, &mut fid)
    .await;
    let w2_end = fid;
    // W3 视频 6Mbps
    let w3_start = fid;
    send_window(&mut tx, &samples, w3_dur, fps, |_| vec![0x11u8; 12_500], &mut fid).await;
    let w3_end = fid;
    // W6 弱网突发
    let w6_start = fid;
    let ov = Arc::clone(&loss_override);
    let burst_at = Duration::from_secs_f32(w6_dur / 3.0);
    let burst = Duration::from_secs_f32(w6_burst);
    let injector = tokio::spawn(async move {
        tokio::time::sleep(burst_at).await;
        ov.set(0.20);
        tokio::time::sleep(burst).await;
        ov.set(0.0);
    });
    send_window(&mut tx, &samples, w6_dur, fps, |_| vec![0x22u8; 12_500], &mut fid).await;
    let w6_end = fid;
    let _ = injector.await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let recvs = samples.recvs.lock().unwrap().clone();
    let m2 = window_metrics(&recvs, (w2_start, w2_end));
    let m3 = window_metrics(&recvs, (w3_start, w3_end));
    let m6 = window_metrics(&recvs, (w6_start, w6_end));

    // W1 M5：识别正确率（完整帧按识别计；丢帧全错）
    let w1_recs: Vec<&Rec> =
        recvs.iter().filter(|r| r.fid >= w1_start && r.fid < w1_end).collect();
    let mut chars_ok = 0f32;
    let mut chars_total = 0f32;
    let mut w1_lats: Vec<f32> = Vec::new();
    for r in &w1_recs {
        chars_total += glyph_count as f32;
        if let (true, Some(d)) = (r.ok, &r.payload) {
            chars_ok += recognize_text_frame(d, glyph_count);
            if r.lat_ms < f32::MAX {
                w1_lats.push(r.lat_ms);
            }
        }
    }
    w1_lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m5 = chars_ok / chars_total.max(1.0) * 100.0;
    let m1_w1 = pct(&w1_lats, 0.95);
    let w1_score = 0.7 * normalize(m5, A_M5) + 0.3 * normalize(m1_w1, A_LATENCY);

    let burst_end_ms = (w1_dur + w2_dur + w3_dur + w6_dur / 3.0 + w6_burst) * 1000.0;
    let burst_start_ms = (w1_dur + w2_dur + w3_dur + w6_dur / 3.0) * 1000.0;
    let rec6 = recovery_ms(&recvs, (w6_start, w6_end), burst_start_ms, burst_end_ms);

    let w1 = w1_score;
    let w2s = 0.55 * normalize(m2.lat_p95, A_LATENCY) + 0.45 * normalize(m2.fps_low, A_FPS_LOW);
    let w3s = 0.4 * normalize(m3.lat_p95, A_LATENCY) + 0.6 * normalize(m3.fps_low, A_FPS_LOW);
    let w6s = 0.4 * normalize(m6.fps_low, A_FPS_LOW) + 0.6 * normalize(rec6, A_RECOVERY);
    let w7s = normalize(startup_ms, A_STARTUP);
    let _ = drain.await;

    serde_json::json!({
        "pass": { "burst_len": pass.burst_len, "relay": pass.relay_label, "seed": pass.seed },
        "w1": { "score": w1, "M5_legibility_pct": (m5*100.0).round()/100.0, "M1_p95_ms": (m1_w1*10.0).round()/10.0 },
        "w2": { "score": w2s, "M1_p95_ms": (m2.lat_p95*10.0).round()/10.0, "fps_low": (m2.fps_low*10.0).round()/10.0 },
        "w3": { "score": w3s, "fps_low": (m3.fps_low*10.0).round()/10.0 },
        "w6": { "score": w6s, "M4_recovery_ms": (rec6*10.0).round()/10.0, "frames_lost": m6.lost },
        "w7": { "score": w7s, "M9_startup_ms": startup_ms }
    })
}

fn med_of(mut v: Vec<f32>) -> f32 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[tokio::main]
async fn main() {
    let mut profile = "T0".to_string();
    let mut rtt_ms = 80.0f32;
    let mut loss = 0.02f32;
    let mut mbps = 10.0f32;
    let mut mtu = 1200u16;
    let mut burst_len = 5u32;
    let mut runs = 3usize;
    let mut fast = false;
    let mut media = "sb.core.fec-xor".to_string();
    let mut out_path = "bench.json".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--profile" => {
                profile = args.next().unwrap_or(profile);
                if !matches!(profile.as_str(), "T0" | "T3" | "T4") {
                    eprintln!("未知 profile {profile}（可选 T0/T3/T4）");
                    std::process::exit(2);
                }
            }
            "--rtt-ms" => rtt_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(rtt_ms),
            "--loss" => loss = args.next().and_then(|v| v.parse().ok()).unwrap_or(loss),
            "--mbps" => mbps = args.next().and_then(|v| v.parse().ok()).unwrap_or(mbps),
            "--mtu" => mtu = args.next().and_then(|v| v.parse().ok()).unwrap_or(mtu),
            "--burst" => burst_len = args.next().and_then(|v| v.parse().ok()).unwrap_or(burst_len),
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(runs),
            "--fast" => fast = true,
            "--media" => {
                media = args.next().unwrap_or(media);
                if !matches!(
                    media.as_str(),
                    "sb.core.fec-xor" | "sb.moonlight.fec-rs"
                ) {
                    eprintln!("未知 media {media}（可选 sb.core.fec-xor / sb.moonlight.fec-rs）");
                    std::process::exit(2);
                }
            }
            "--out" => out_path = args.next().unwrap_or(out_path),
            other => {
                eprintln!("未知参数 {other}；用法：--profile T0|T3|T4 --rtt-ms --loss --mbps --mtu --burst --runs --fast --media --out");
                std::process::exit(2);
            }
        }
    }
    let runs = if fast { runs.min(1) } else { runs };

    // 剖面 → passes（04 §1/§4：T3 双口径；T4 强制中继标注）
    let base = PassParams { rtt_ms, loss, mbps, mtu, burst_len: 0, relay_label: false, seed: 0 };
    let passes: Vec<PassParams> = match profile.as_str() {
        "T3" => vec![
            PassParams { burst_len: 0, ..base },                        // 均匀口径
            PassParams { burst_len: burst_len.max(5), ..base },         // 突发口径
        ],
        "T4" => vec![PassParams { burst_len, relay_label: true, ..base }],
        _ => vec![PassParams { burst_len, ..base }],
    };

    let cfg = BenchCfg {
        media: media.clone(),
        fast,
        passes,
    };

    // 每 run 每 pass 跑一遍
    let mut pass_reports: Vec<serde_json::Value> = Vec::new();
    let mut composites: Vec<f32> = Vec::new();
    for (pi, pass) in cfg.passes.clone().into_iter().enumerate() {
        let mut run_reports: Vec<serde_json::Value> = Vec::new();
        let mut run_comps: Vec<f32> = Vec::new();
        for run in 0..runs {
            let mut pass = pass;
            pass.seed += run as u64;
            let r = run_pass(&cfg, pass, run).await;
            let comp = r["w1"]["score"].as_f64().unwrap_or(0.0) as f32 * 0.2
                + r["w2"]["score"].as_f64().unwrap_or(0.0) as f32 * 0.2
                + r["w3"]["score"].as_f64().unwrap_or(0.0) as f32 * 0.2
                + r["w6"]["score"].as_f64().unwrap_or(0.0) as f32 * 0.25
                + r["w7"]["score"].as_f64().unwrap_or(0.0) as f32 * 0.15;
            run_comps.push(comp);
            run_reports.push(r);
        }
        let comp = med_of(run_comps);
        composites.push(comp);
        pass_reports.push(serde_json::json!({
            "pass_index": pi,
            "params": pass,
            "composite": (comp * 100.0).round() / 100.0,
            "runs": run_reports
        }));
    }

    // T3/T4：多口径均值 ×0.9（T0 单口径直接 ×0.9 回归系数）
    let mean = composites.iter().sum::<f32>() / composites.len().max(1) as f32;
    let composite = mean * 0.9;

    let report = serde_json::json!({
        "schema": "sbbench/1",
        "module": { "id": format!("sb.core.loopback+{media}"), "version": "0.1.0" },
        "plan": "inline:sbbench",
        "profile": profile,
        "profile_params": { "rtt_ms": rtt_ms, "loss": loss, "mbps": mbps, "mtu": mtu, "burst_len": burst_len },
        "runs": runs,
        "passes": pass_reports,
        "composite": (composite * 100.0).round() / 100.0,
        "note": match profile.as_str() {
            "T3" => "T3 合成等效剖面（均匀+突发双口径均值×0.9）；注册表认证仍需实网复跑",
            "T4" => "T4 强制中继剖面（path.kind 如实标注 relay）；合成等效，×0.9",
            _ => "T0 回归剖面（×0.9）",
        },
        "env": { "os": std::env::consts::OS },
        "tool_version": "switchboard-bench 0.1.2"
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&report).unwrap()).expect("写出 bench.json");

    println!("SB-Bench {profile} 完成（runs={runs}, fast={fast}, media={media}）");
    for (i, c) in composites.iter().enumerate() {
        println!("  pass{i} 综合 : {c:.1}");
    }
    println!("  剖面综合分  : {composite:.1} → {out_path}");
}
