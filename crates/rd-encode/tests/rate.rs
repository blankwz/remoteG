//! T-011 集成测试：码率控制（v0.2 baseline-adaptive）、编解码协商与能力探测。

use rd_encode::caps::{negotiate, probe_encode_caps};
use rd_encode::rate::{RateController, RateDecision, RateInput};
use rd_proto::v0::Codec;

/// 正常反馈：RTT 20 ms、无积压、无丢包。
fn normal() -> RateInput {
    RateInput {
        rtt_ms: 20,
        queued_bytes: 0,
        lost_ratio: 0.0,
    }
}

/// 拥塞反馈：发送队列积压 1 MB。
fn congested_queued() -> RateInput {
    RateInput {
        rtt_ms: 20,
        queued_bytes: 1 << 20,
        lost_ratio: 0.0,
    }
}

#[test]
fn queued_backlog_drops_bitrate_after_debounce() {
    let mut rc = RateController::new(4000, 60);
    // 积压防抖（D-013 包5）：单 tick 积压尖峰（鼠标扫过全窗的 hover 变化）
    // 由 QUIC 缓冲吸收，不降码率。
    assert_eq!(rc.update(congested_queued()).kbps, 4000);
    // 连续第 2 tick（400 ms）确认拥塞 → 先降码率 ×0.7。
    assert_eq!(rc.update(congested_queued()).kbps, 2800);
    // 继续拥塞：2800 → 1960 → 1372；码率 < 1500 → 帧率 30。
    assert_eq!(rc.update(congested_queued()).kbps, 1960);
    let d = rc.update(congested_queued());
    assert_eq!(d.kbps, 1372);
    assert_eq!(d.fps, 30);
}

#[test]
fn rtt_spike_drops_bitrate() {
    let mut rc = RateController::new(4000, 60);
    // 首次反馈建立基线 RTT=20。
    assert_eq!(rc.update(normal()).kbps, 4000);
    // RTT 升至 31（> 基线 1.5×=30）→ 拥塞 ×0.7。
    assert_eq!(
        rc.update(RateInput {
            rtt_ms: 31,
            queued_bytes: 0,
            lost_ratio: 0.0
        })
        .kbps,
        2800
    );
    // RTT 升至 41（> 基线 2×）→ 严重 ×0.5。
    assert_eq!(
        rc.update(RateInput {
            rtt_ms: 41,
            queued_bytes: 0,
            lost_ratio: 0.0
        })
        .kbps,
        1400
    );
}

#[test]
fn packet_loss_threshold() {
    let mut rc = RateController::new(4000, 60);
    rc.update(normal());
    // 丢包阈值化（D-013 包4/E3）：≤2% 视为 tailnet/DERP 抖动，不降码率
    // （健康链路稳态爬升允许 ≥ 当前值）。
    let before = rc
        .update(RateInput {
            rtt_ms: 20,
            queued_bytes: 0,
            lost_ratio: 0.02,
        })
        .kbps;
    assert!(before >= 4000, "≤2% 不降码率: {before}");
    // >2% 真实拥塞 → 相对下降。
    let after = rc
        .update(RateInput {
            rtt_ms: 20,
            queued_bytes: 0,
            lost_ratio: 0.05,
        })
        .kbps;
    assert!(after < before, ">2% → 降码率: {before} → {after}");
}

#[test]
fn recovery_fast_climb_back_to_start_then_steady() {
    let mut rc = RateController::new(4000, 60);
    rc.update(normal());
    rc.update(congested_queued()); // 防抖 tick（好样本，稳态 streak=1）
    let degraded = rc.update(congested_queued()).kbps; // ×0.7
    // 好样本：低于起始码率 → 爬坡 ×1.3 快速恢复，封顶 start_kbps。
    let g1 = rc.update(normal()).kbps;
    assert!(g1 > degraded, "爬坡: {degraded} → {g1}");
    let g2 = rc.update(normal()).kbps;
    assert!(g2 > g1, "继续爬坡: {g1} → {g2}");
    assert!(g2 >= degraded, "恢复到降档前水平");
    // 超过起始码率后转稳态：2 连好 ×1.15。
    let s = rc.update(normal()).kbps;
    let s2 = rc.update(normal()).kbps;
    assert!(s2 > s || s >= 50_000, "稳态增长: {s} → {s2}");
}

#[test]
fn bitrate_upper_bound_clamped() {
    let mut rc = RateController::new(50_000, 60);
    for _ in 0..5 {
        let d = rc.update(normal());
        assert!(d.kbps <= 50_000);
    }
    assert_eq!(rc.update(normal()).kbps, 50_000);
}

#[test]
fn bitrate_lower_bound_clamped() {
    let mut rc = RateController::new(600, 60);
    // 积压防抖：首 tick 不降。
    assert_eq!(rc.update(congested_queued()).kbps, 600);
    // ×0.7 → 420 → clamp 500；持续拥塞不跌破下限。
    assert_eq!(rc.update(congested_queued()).kbps, 500);
    assert_eq!(rc.update(congested_queued()).kbps, 500);
}

#[test]
fn low_bitrate_drops_fps() {
    // 低起始码率直接降帧（LOW_KBPS 规则）。
    let mut rc = RateController::new(1400, 60);
    let d = rc.update(normal());
    assert_eq!(
        d,
        RateDecision {
            kbps: 1400,
            fps: 30
        }
    );
    // 帧率上限本身低于 30 时不会反向上调。
    let mut rc = RateController::new(1400, 24);
    assert_eq!(rc.update(normal()).fps, 24);
}

#[test]
fn new_clamps_start_bitrate() {
    let mut rc = RateController::new(0, 60);
    assert_eq!(rc.update(normal()).kbps, 500);
    let mut rc = RateController::new(999_999, 60);
    assert_eq!(rc.update(normal()).kbps, 50_000);
}

#[test]
fn negotiate_both_support_444() {
    let enc = [Codec::H264444, Codec::H264420];
    let dec = [Codec::H264420, Codec::H264444];
    assert_eq!(negotiate(&enc, &dec, true), Codec::H264444);
}

#[test]
fn negotiate_enc_missing_444() {
    let enc = [Codec::H264420];
    let dec = [Codec::H264444, Codec::H264420];
    assert_eq!(negotiate(&enc, &dec, true), Codec::H264420);
}

#[test]
fn negotiate_dec_missing_444() {
    let enc = [Codec::H264444, Codec::H264420];
    let dec = [Codec::H264420];
    assert_eq!(negotiate(&enc, &dec, true), Codec::H264420);
}

#[test]
fn negotiate_dec_disables_444() {
    // 解码端虽声明 H264444 能力，但 supports_444=false（如 iPhone Safari）。
    let enc = [Codec::H264444, Codec::H264420];
    let dec = [Codec::H264444, Codec::H264420];
    assert_eq!(negotiate(&enc, &dec, false), Codec::H264420);
}

#[test]
fn negotiate_empty_caps_falls_back() {
    assert_eq!(negotiate(&[], &[], true), Codec::H264420);
}

#[test]
fn probe_reports_420_at_least() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();
    let caps = probe_encode_caps();
    println!("probe_encode_caps() = {caps:?}");
    // 4:2:0 软编兜底始终可用。
    assert!(caps.contains(&Codec::H264420));
    // 无 ffmpeg 硬编路径时只包含 H264420。
    #[cfg(not(all(windows, feature = "ffmpeg")))]
    assert_eq!(caps, vec![Codec::H264420]);
    // 缓存生效：两次调用结果一致。
    assert_eq!(probe_encode_caps(), caps);
}
