//! 码率自适应（baseline-adaptive，v0.2：替换 T-011 的 AIMD）。
//!
//! 规则来源：RustDesk 1.5.0 QoS（PR #16082，spec 07 `sb.qos.baseline-adaptive`），
//! 在 rd 200 ms 反馈周期上落地：
//! - **延迟基线**：首反馈建立基线；好样本以 α=1/32 渐进适应（比 AIMD 的 1/8 慢，
//!   防把拥塞期 RTT 学进基线）；坏样本冻结基线。
//! - **分级降级**：rtt > 基线×2 → 严重（×0.5 立即）；积压连续 2 tick（D-013 防抖
//!   保留）/丢包 >2%/rtt > 基线×1.5 → 拥塞（×0.7，**先降码率后降帧率**）。
//! - **确认机制**：降帧率需码率已到底（≤ 下限×1.2）**且**连续 3 个坏样本——
//!   避免抖动误伤帧率。
//! - **快恢复**：连续 2 个好样本 → ×1.15；启动期（前 12 个好样本）×1.3 激进爬坡
//!   （对齐 RustDesk "60fps 达成 7.99s→2.98s" 的启动优化）；帧率恢复需连续 6 个
//!   好样本，拥塞回归即回滚。
//!
//! 反馈周期 200 ms：[`RateController::update`] 每次消费一条 [`RateInput`]，
//! 返回码率/帧率建议（[`RateDecision`]）；调用方（window_session 发送管线）
//! 把码率应用到编码器并按帧率节流。

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
/// 上限 200 Mbps（宪法要求 #1：局域网高分辨率 144Hz 场景允许千兆/2.5G 带宽换画质；
/// 跨网场景由 baseline-adaptive 控制器按 RTT/丢包自行收敛）。
const MAX_KBPS: u32 = 200_000;
/// 拥塞降档因子 ×0.7（整型避免浮点）。
const DROP_NUM: u32 = 7;
const DROP_DEN: u32 = 10;
/// 严重超延迟降档因子 ×0.5。
const SEVERE_NUM: u32 = 1;
const SEVERE_DEN: u32 = 2;
/// 恢复升档因子 ×1.15。
const GROW_NUM: u32 = 23;
const GROW_DEN: u32 = 20;
/// 启动爬坡因子 ×1.3。
const WARM_NUM: u32 = 13;
const WARM_DEN: u32 = 10;
/// 积压信号需连续满足的 tick 数（400 ms）：鼠标扫过全窗的 hover 变化会让
/// 编码帧瞬间膨胀、单 tick 积压尖峰被 QUIC 缓冲吸收即可，立刻降码率会让
/// 「移动中画面变糊」——正是用户抱怨的顿挫感来源（2026-10-23）。
const BACKLOG_STREAK: u32 = 2;
/// RTT 相对基线的拥塞判定阈值（>50% 即坏样本）。
const RTT_BAD_RATIO: f32 = 1.5;
/// 严重超延迟阈值（>100%，立即 ×0.5）。
const RTT_SEVERE_RATIO: f32 = 2.0;
/// 丢包阈值（>2% 视为真实拥塞；≤2% 是 tailnet/DERP 单包抖动）。
const LOST_RATIO_THRESHOLD: f32 = 0.02;
/// 降帧率所需的连续坏样本数（RustDesk 1.5.0：3 连坏才动帧率）。
const BAD_STREAK_FOR_FPS: u32 = 3;
/// 快恢复所需的连续好样本数。
const GOOD_STREAK_GROW: u32 = 2;
/// 帧率恢复所需的连续好样本数（比码率恢复更严格）。
const GOOD_STREAK_FPS_RECOVER: u32 = 6;
/// 启动爬坡的好样本配额（≈2.4 s；之后切 ×1.15 稳态）。
const WARMUP_GOOD: u32 = 12;
/// RTT 基线渐进适应系数（1/32，慢于 AIMD 的 1/8）。
const BASELINE_ADAPT: f32 = 1.0 / 32.0;
/// 码率低于该阈值（kbps）时建议降帧率。
const LOW_KBPS: u32 = 1500;
/// 低码率时的建议帧率。
const LOW_FPS: u32 = 30;

/// baseline-adaptive 码率控制器。
pub struct RateController {
    kbps: u32,
    max_fps: u32,
    /// 起始码率：爬坡的封顶线（爬坡只用于向它快速恢复，不越过）。
    start_kbps: u32,
    /// RTT 基线；首次反馈初始化。坏样本冻结，好样本 α=1/32 渐进适应。
    baseline_rtt: Option<f32>,
    /// 连续坏样本数（驱动降帧率确认）。
    bad_streak: u32,
    /// 连续好样本数（驱动快恢复/帧率恢复）。
    good_streak: u32,
    /// 帧率已被降档（恢复需更长的干净期）。
    fps_capped: bool,
    /// 爬坡剩余配额（好样本数）。
    warmup_left: u32,
    /// 积压信号连续 tick 计数。
    backlog_streak: u32,
}

impl RateController {
    /// 创建控制器：`start_kbps` 起始码率（自动收敛到上下限内），
    /// `max_fps` 帧率上限。
    pub fn new(start_kbps: u32, max_fps: u32) -> Self {
        Self {
            kbps: start_kbps.clamp(MIN_KBPS, MAX_KBPS),
            start_kbps: start_kbps.clamp(MIN_KBPS, MAX_KBPS),
            max_fps: max_fps.max(1),
            baseline_rtt: None,
            bad_streak: 0,
            good_streak: 0,
            fps_capped: false,
            warmup_left: WARMUP_GOOD,
            backlog_streak: 0,
        }
    }

    /// 消费一条反馈，返回最新码率/帧率建议。
    pub fn update(&mut self, i: RateInput) -> RateDecision {
        let rtt = i.rtt_ms as f32;
        let baseline = *self.baseline_rtt.get_or_insert(rtt);

        // 一帧估算大小：kbps × 125 / fps（fps 取当前生效档）。
        let fps = self.current_fps();
        let frame_bytes = (self.kbps as usize) * 125 / (fps.max(1) as usize);
        if i.queued_bytes > frame_bytes {
            self.backlog_streak += 1;
        } else {
            self.backlog_streak = 0;
        }

        let severe = rtt > baseline * RTT_SEVERE_RATIO;
        let bad = self.backlog_streak >= BACKLOG_STREAK
            || i.lost_ratio > LOST_RATIO_THRESHOLD
            || rtt > baseline * RTT_BAD_RATIO;

        if severe {
            // 严重超延迟：立即 ×0.5，基线冻结
            self.kbps = clamp_kbps(self.kbps * SEVERE_NUM / SEVERE_DEN);
            self.bad_streak = self.bad_streak.saturating_add(1);
            self.good_streak = 0;
        } else if bad {
            // 拥塞：先降码率（×0.7）；码率到底且 3 连坏才降帧率
            self.kbps = clamp_kbps(self.kbps * DROP_NUM / DROP_DEN);
            self.bad_streak = self.bad_streak.saturating_add(1);
            self.good_streak = 0;
            if self.bad_streak >= BAD_STREAK_FOR_FPS && self.kbps <= MIN_KBPS * 12 / 10 {
                self.fps_capped = true;
            }
        } else {
            // 好样本：基线渐进适应 + 恢复
            self.baseline_rtt = Some(baseline * (1.0 - BASELINE_ADAPT) + rtt * BASELINE_ADAPT);
            self.good_streak = self.good_streak.saturating_add(1);
            self.bad_streak = 0;
            if self.warmup_left > 0 && self.kbps < self.start_kbps {
                // 爬坡：低于起始码率时 ×1.3 快速恢复（封顶 start_kbps）
                self.warmup_left -= 1;
                self.kbps = clamp_kbps(self.kbps * WARM_NUM / WARM_DEN).min(self.start_kbps);
            } else if self.good_streak >= GOOD_STREAK_GROW {
                self.kbps = clamp_kbps(self.kbps * GROW_NUM / GROW_DEN);
                self.good_streak = 0;
            }
            if self.fps_capped && self.good_streak >= GOOD_STREAK_FPS_RECOVER {
                self.fps_capped = false;
            }
        }

        RateDecision {
            kbps: self.kbps,
            fps: self.current_fps(),
        }
    }

    /// 当前生效帧率档：帧率已降档或码率过低 → 30；否则上限。
    fn current_fps(&self) -> u32 {
        if self.fps_capped || self.kbps < LOW_KBPS {
            self.max_fps.min(LOW_FPS)
        } else {
            self.max_fps
        }
    }
}

/// 收敛码率到 [500, 200_000] kbps。
fn clamp_kbps(kbps: u32) -> u32 {
    kbps.clamp(MIN_KBPS, MAX_KBPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normal() -> RateInput {
        RateInput {
            rtt_ms: 20,
            queued_bytes: 0,
            lost_ratio: 0.0,
        }
    }

    #[test]
    fn 首反馈建立基线_不动码率() {
        let mut rc = RateController::new(4000, 60);
        assert_eq!(rc.update(normal()).kbps, 4000);
    }

    #[test]
    fn rtt_超基线1_5倍_降码率07() {
        let mut rc = RateController::new(4000, 60);
        rc.update(normal());
        // 31 > 20×1.5=30 → ×0.7
        assert_eq!(
            rc.update(RateInput {
                rtt_ms: 31,
                ..normal()
            })
            .kbps,
            2800
        );
    }

    #[test]
    fn rtt_超基线2倍_严重降半() {
        let mut rc = RateController::new(4000, 60);
        rc.update(normal());
        assert_eq!(
            rc.update(RateInput {
                rtt_ms: 41,
                ..normal()
            })
            .kbps,
            2000
        );
    }

    #[test]
    fn 丢包阈值化_2以内不降() {
        let mut rc = RateController::new(4000, 60);
        rc.update(normal()); // 基线
        let before = rc
            .update(RateInput {
                rtt_ms: 20,
                lost_ratio: 0.02,
                queued_bytes: 0,
            })
            .kbps;
        assert!(before >= 4000, "≤2% 不降（健康链路稳态爬升允许）: {before}");
        let after = rc
            .update(RateInput {
                rtt_ms: 20,
                lost_ratio: 0.05,
                queued_bytes: 0,
            })
            .kbps;
        assert!(after < before, ">2% 真实拥塞 → 降码率: {before} → {after}");
    }

    #[test]
    fn 积压连续2tick才降_防hover误伤() {
        let mut rc = RateController::new(4000, 60);
        let cong = RateInput {
            rtt_ms: 20,
            queued_bytes: 1 << 20,
            lost_ratio: 0.0,
        };
        assert_eq!(rc.update(cong).kbps, 4000, "单 tick 积压由 QUIC 吸收");
        assert_eq!(rc.update(cong).kbps, 2800);
    }

    #[test]
    fn 高码率连续坏_只降码率不动帧率() {
        // 高码率下连续坏：先降码率；帧率暂不受影响（尚未到底）
        let mut rc = RateController::new(4000, 60);
        let cong = RateInput {
            rtt_ms: 20,
            queued_bytes: 1 << 20,
            lost_ratio: 0.0,
        };
        rc.update(cong); // 防抖（好样本）
        assert_eq!(rc.update(cong).kbps, 2800); // ×0.7
        let d = rc.update(cong); // ×0.7 → 1960（bad_streak=2，未到 3）
        assert_eq!(d.kbps, 1960);
        assert_eq!(d.fps, 60, "码率未到底 + bad_streak<3 → 帧率不动");
        // 低码率 + 3 连坏 → 降帧
        let mut rc = RateController::new(600, 60);
        rc.update(cong); // 防抖
        assert_eq!(rc.update(cong).kbps, 500, "×0.7 → 420 → clamp 500");
        assert_eq!(rc.update(cong).kbps, 500);
        assert_eq!(rc.update(cong).fps, 30, "码率到底 + bad_streak≥3 → 降帧");
    }

    #[test]
    fn 恢复_爬坡回起始码率后转稳态() {
        let mut rc = RateController::new(4000, 60);
        rc.update(normal()); // 基线
        rc.update(RateInput {
            rtt_ms: 31,
            ..normal()
        }); // ×0.7 → 2800
            // 好样本 1：低于起始码率 → 爬坡 ×1.3 = 3640
        assert_eq!(rc.update(normal()).kbps, 3640);
        // 好样本 2：爬坡封顶 start_kbps = 4000
        assert_eq!(rc.update(normal()).kbps, 4000);
        // 回到起始码率后转稳态：好样本累计 2 连 → ×1.15
        assert_eq!(rc.update(normal()).kbps, 4600);
    }

    #[test]
    fn 上下限钳制() {
        let mut rc = RateController::new(0, 60);
        // 首样本建立基线；起始 500 已在下限，好样本稳态 2 连后 ×1.15
        assert_eq!(rc.update(normal()).kbps, 500);
        assert_eq!(rc.update(normal()).kbps, 575);
        let mut rc = RateController::new(999_999, 144);
        assert_eq!(rc.update(normal()).kbps, 200_000);
        for _ in 0..3 {
            assert_eq!(rc.update(normal()).kbps, 200_000);
        }
    }

    #[test]
    fn 低码率降帧_长干净期后恢复() {
        let mut rc = RateController::new(600, 60);
        let cong = RateInput {
            rtt_ms: 20,
            queued_bytes: 1 << 20,
            lost_ratio: 0.0,
        };
        rc.update(cong); // 防抖
        assert_eq!(rc.update(cong).kbps, 500, "×0.7 → 420 → clamp 500");
        assert_eq!(rc.update(cong).fps, 30, "码率到底 + 3 连坏 → 降帧");
        // 拥塞结束：爬坡封顶 600，稳态逐步爬过 1500 后帧率恢复（6 连好规则 +
        // LOW_KBPS 规则都要满足）
        for _ in 0..24 {
            rc.update(normal());
        }
        assert_eq!(rc.current_fps(), 60, "长干净期 → 帧率恢复");
    }

    // ---- 端到端收敛仿真（无网络可测的关键 QoS 参数回归护栏）----
    // 每条 tick 对应 200ms 反馈周期。把码控从「稳态干链路 → 拥塞 2s → 恢复」
    // 全流程推演一遍，断言收敛到明确时间目标，防止改动破坏爬坡/恢复机制
    // （如误删 warmup ×1.3，恢复将退化为 ×1.15、耗时翻倍以上）。

    /// 稳态好样本：RTT 保持 20ms、零积压、零丢包。
    fn good() -> RateInput {
        RateInput {
            rtt_ms: 20,
            queued_bytes: 0,
            lost_ratio: 0.0,
        }
    }

    /// 拥塞坏样本：RTT 飙到 5×基线、高积压、5% 丢包（一次性触达各降级分支）。
    fn congest() -> RateInput {
        RateInput {
            rtt_ms: 100,
            queued_bytes: 1 << 20,
            lost_ratio: 0.05,
        }
    }

    #[test]
    fn 拥塞恢复_warmup快速爬回起始码率() {
        let start = 4000;
        let mut rc = RateController::new(start, 60);
        // 1) 先建 RTT 基线（20ms）。
        rc.update(good());
        // 2) 重度拥塞 20 tick（4s）：rtt 5×基线触发 ×0.5 严重路径，跌到下限 500。
        for _ in 0..20 {
            rc.update(congest());
        }
        assert_eq!(rc.update(congest()).kbps, 500, "拥塞应跌到下限 clamp");

        // 3) 恢复：warmup ×1.3 从头爬到 start（约 8 tick），最多 12 tick（2.4s）
        //    内应回到 start。该断言锁定「去拥塞后的快速爬坡」，若误删 warmup
        //    （退化为 ×1.15）需 30 tick 才能回 4000，将立即被此用例抓住。
        let mut ticks = 0;
        let mut kbps = 0;
        for _ in 0..12 {
            kbps = rc.update(good()).kbps;
            ticks += 1;
            if kbps >= start {
                break;
            }
        }
        assert!(
            kbps >= start,
            "拥塞后 {ticks} tick({}ms)仍未爬回起始码率（kbps={kbps}）",
            ticks * 200
        );
    }

    #[test]
    fn 启动即达起始码率_不先跌再爬() {
        // 启动契约：创建码率即生效，首样本就 == start（不会从低位慢慢爬）。
        // warmup×1.3 是「恢复到 start」的坡，不是启动坡；启动时 kbps==start，
        // warmup 条件 kbps<start 恒假，直接走好样本稳态。
        let mut rc = RateController::new(8000, 60);
        assert_eq!(rc.update(good()).kbps, 8000, "首样本即起始码率");
    }
}
