//! 码率自适应（AIMD，T-011）：根据 RTT / 丢包 / 发送队列积压调整码率与帧率。
//!
//! 反馈周期 200 ms：[`RateController::update`] 每次消费一个 [`RateInput`]，
//! 返回当前建议的码率与帧率（[`RateDecision`]）；调用方（T-016 发送管线）
//! 负责把码率应用到编码器（`VideoEncoder::set_bitrate`）并按帧率节流。
//!
//! 规则（任务卡 T-011 / 架构 §3.2）：
//! - 拥塞信号（任一满足即降）：发送积压 > 1 帧估算大小、RTT 比基线
//!   上升 > 50%、丢包率 > 0；
//! - 降：码率 ×0.7；升：连续 5 次反馈（≈ 1 s）无拥塞后码率 ×1.1；
//! - 上下限 [500, 20000] kbps；
//! - 码率 < 1500 kbps 时建议帧率降至 30（不超过创建时的上限）。
//!
//! RTT 基线：首次反馈的 RTT 为初始基线；无拥塞反馈用 EWMA（α = 1/8，
//! 与 TCP 相同）平滑更新；拥塞期基线冻结，避免网络尖峰把基线抬高后
//! 恢复期被误判为"RTT 正常"而提前加码。

/// 一条码率控制反馈（每 200 ms 一次）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateInput {
    /// 当前 QUIC RTT（毫秒）。
    pub rtt_ms: u32,
    /// 发送队列积压字节数。
    pub queued_bytes: usize,
    /// 当前周期丢包率 [0.0, 1.0]。
    pub lost_ratio: f32,
}

/// 码率/帧率决策。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateDecision {
    /// 建议目标码率（kbps）。
    pub kbps: u32,
    /// 建议帧率（fps）。
    pub fps: u32,
}

const MIN_KBPS: u32 = 500;
const MAX_KBPS: u32 = 20_000;
/// 降档因子 0.7（×7/10，整型避免浮点）。
const DROP_NUM: u32 = 7;
const DROP_DEN: u32 = 10;
/// 升档因子 1.1（×11/10）。
const GROW_NUM: u32 = 11;
const GROW_DEN: u32 = 10;
/// 升档所需连续无拥塞反馈次数（200 ms × 5 ≈ 1 s）。
const GROW_TICKS: u32 = 5;
/// RTT 相对基线的拥塞判定阈值（上升 > 50%）。
const RTT_SPIKE_RATIO: f32 = 1.5;
/// RTT 基线 EWMA 平滑系数（1/8，TCP 惯例）。
const RTT_EWMA: f32 = 0.125;
/// 码率低于该阈值（kbps）时建议降帧率。
const LOW_KBPS: u32 = 1500;
/// 低码率时的建议帧率。
const LOW_FPS: u32 = 30;

/// AIMD 码率控制器。
pub struct RateController {
    kbps: u32,
    max_fps: u32,
    /// RTT 基线（EWMA 平滑）；首次反馈时初始化。
    baseline_rtt: Option<f32>,
    /// 距上次升档的连续无拥塞反馈次数。
    good_ticks: u32,
}

impl RateController {
    /// 创建控制器：`start_kbps` 起始码率（自动收敛到上下限内），
    /// `max_fps` 帧率上限。
    pub fn new(start_kbps: u32, max_fps: u32) -> Self {
        Self {
            kbps: start_kbps.clamp(MIN_KBPS, MAX_KBPS),
            max_fps: max_fps.max(1),
            baseline_rtt: None,
            good_ticks: 0,
        }
    }

    /// 消费一条反馈，返回最新码率/帧率建议。
    pub fn update(&mut self, i: RateInput) -> RateDecision {
        let rtt = i.rtt_ms as f32;
        let baseline = *self.baseline_rtt.get_or_insert(rtt);

        // 一帧的估算大小（字节）：kbps × 1000 / 8 / fps ≈ kbps × 125 / fps，
        // fps 取当前生效帧率（低码率降帧到 30 后单帧更大，阈值相应放宽）。
        // 积压超过 1 帧说明发送速率跟不上产出速率，必须降码率。
        let fps = if self.kbps < LOW_KBPS {
            self.max_fps.min(LOW_FPS)
        } else {
            self.max_fps
        };
        let frame_bytes = (self.kbps as usize) * 125 / (fps.max(1) as usize);
        // 丢包阈值(D-013 包3/E3):任何丢包即降码率对 tailnet/DERP 的单包
        // 抖动过敏——码率被反复砸到几百 kbps 再缓慢爬升。>2% 窗口丢包才
        // 视为拥塞;积压与 RTT 尖峰仍为即时信号。
        const LOST_RATIO_THRESHOLD: f32 = 0.02;
        let congested = i.queued_bytes > frame_bytes
            || rtt > baseline * RTT_SPIKE_RATIO
            || i.lost_ratio > LOST_RATIO_THRESHOLD;

        if congested {
            self.kbps = clamp_kbps(self.kbps * DROP_NUM / DROP_DEN);
            self.good_ticks = 0;
        } else {
            self.baseline_rtt = Some(baseline * (1.0 - RTT_EWMA) + rtt * RTT_EWMA);
            self.good_ticks += 1;
            if self.good_ticks >= GROW_TICKS {
                self.kbps = clamp_kbps(self.kbps * GROW_NUM / GROW_DEN);
                self.good_ticks = 0;
            }
        }

        RateDecision {
            kbps: self.kbps,
            fps,
        }
    }
}

/// 收敛码率到 [500, 20000] kbps。
fn clamp_kbps(kbps: u32) -> u32 {
    kbps.clamp(MIN_KBPS, MAX_KBPS)
}
