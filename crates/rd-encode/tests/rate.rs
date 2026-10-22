//! T-011 集成测试：AIMD 码率控制、编解码协商与能力探测。

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
fn queued_backlog_drops_bitrate() {
    let mut rc = RateController::new(4000, 60);
    // 首次反馈：积压 > 1 帧（4000 kbps / 60 fps ≈ 8.3 KB）→ 降档。
    assert_eq!(rc.update(congested_queued()).kbps, 2800);
    // 连续降档 4000 → 2800 → 1960 → 1372 → 960。
    assert_eq!(rc.update(congested_queued()).kbps, 1960);
    assert_eq!(rc.update(congested_queued()).kbps, 1372);
    let d = rc.update(congested_queued());
    assert_eq!(d.kbps, 960);
    // 码率 < 1500 kbps → 建议帧率降至 30。
    assert_eq!(d.fps, 30);
    // 继续降档直到下限 500。
    rc.update(congested_queued());
    rc.update(congested_queued());
    assert_eq!(rc.update(congested_queued()).kbps, 500);
    assert_eq!(rc.update(congested_queued()).kbps, 500);
}

#[test]
fn rtt_spike_drops_bitrate() {
    let mut rc = RateController::new(4000, 60);
    // 首次反馈建立基线 RTT=20。
    assert_eq!(rc.update(normal()).kbps, 4000);
    // RTT 升至 31（> 基线 1.5×=30）→ 拥塞降档。
    assert_eq!(
        rc.update(RateInput {
            rtt_ms: 31,
            queued_bytes: 0,
            lost_ratio: 0.0
        })
        .kbps,
        2800
    );
}

#[test]
fn packet_loss_drops_bitrate() {
    let mut rc = RateController::new(4000, 60);
    // 丢包阈值化（D-013 包4/E3）：≤2% 视为 tailnet/DERP 抖动，不降码率。
    assert_eq!(
        rc.update(RateInput {
            rtt_ms: 20,
            queued_bytes: 0,
            lost_ratio: 0.02
        })
        .kbps,
        4000
    );
    // >2% 真实拥塞 → 降档。
    assert_eq!(
        rc.update(RateInput {
            rtt_ms: 20,
            queued_bytes: 0,
            lost_ratio: 0.05
        })
        .kbps,
        2800
    );
}

#[test]
fn recovery_grows_bitrate_after_one_second() {
    let mut rc = RateController::new(4000, 60);
    assert_eq!(rc.update(congested_queued()).kbps, 2800);
    // 连续 4 次无拥塞反馈（800 ms）不升档。
    for _ in 0..4 {
        assert_eq!(rc.update(normal()).kbps, 2800);
    }
    // 第 5 次（≈1 s）升档 +10%。
    let d = rc.update(normal());
    assert_eq!(d.kbps, 3080);
    assert_eq!(d.fps, 60);
    // 再 5 次继续升档 3080 → 3388。
    for _ in 0..4 {
        rc.update(normal());
    }
    assert_eq!(rc.update(normal()).kbps, 3388);
}

#[test]
fn bitrate_upper_bound_clamped() {
    let mut rc = RateController::new(20_000, 60);
    for _ in 0..5 {
        let d = rc.update(normal());
        assert!(d.kbps <= 20_000);
    }
    assert_eq!(rc.update(normal()).kbps, 20_000);
}

#[test]
fn bitrate_lower_bound_clamped() {
    let mut rc = RateController::new(600, 60);
    assert_eq!(rc.update(congested_queued()).kbps, 500);
    // 持续拥塞不跌破下限。
    assert_eq!(rc.update(congested_queued()).kbps, 500);
    // 恢复后从下限开始升档 500 → 550。
    for _ in 0..4 {
        assert_eq!(rc.update(normal()).kbps, 500);
    }
    assert_eq!(rc.update(normal()).kbps, 550);
}

#[test]
fn low_bitrate_drops_fps() {
    // 低起始码率直接降帧。
    let mut rc = RateController::new(1400, 60);
    let d = rc.update(normal());
    assert_eq!(
        d,
        RateDecision {
            kbps: 1400,
            fps: 30
        }
    );
    // 升回 ≥1500 后帧率恢复上限。
    for _ in 0..4 {
        rc.update(normal());
    }
    let d = rc.update(normal());
    assert_eq!(
        d,
        RateDecision {
            kbps: 1540,
            fps: 60
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
    assert_eq!(rc.update(normal()).kbps, 20_000);
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
