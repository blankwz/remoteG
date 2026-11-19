//! 屏幕档位数学。
//!
//! 移植自 snug 的 `Sources/SnugCore/PhoneScreen/ScreenFit.swift`
//! （derived from zephyr4123/snug，MIT）。语义保持逐条一致，仅把 Swift 的
//! `CGFloat`/`ClosedRange` 换成 Rust 的 `f64`：
//!
//! - 比例跟着手机走：`手机屏幕高宽比 − 上下各让出的工具栏余量`；
//! - 基准宽度：从 540 起找最窄一档，使「宽 × 比例」不小于 800（矮屏靠加宽
//!   才够高，比例不小于 1 时 800 宽一定够）；
//! - 档位：基准宽度到 1080，每 10 点一档，建屏时一次登记全部档位；
//! - 选档：窗口在基准档铺不下时，按超出比例往上找装得下的最窄一档。
//!
//! 与 macOS 的差异（见预研报告 §1.6）：这里不做 2 倍高清门槛判定，档位
//! 直接作为虚拟显示器支持的显示模式登记；Windows 侧的 DPI 由系统按显示器
//! 单独管理。

/// 设计宽度：面板与手势阈值按这一档设计。
pub const DESIGN_WIDTH: u32 = 540;
/// 最窄档（macOS「2 倍高清」门槛宽度；Windows 目前仅作档位下限）。
pub const MIN_WIDTH: u32 = 540;
/// 最矮高度门槛：基准档的「宽 × 比例」不足这个值时加宽基准档。
pub const MIN_HEIGHT: f64 = 800.0;
/// 最宽一档。
pub const MAX_WIDTH: u32 = 1080;
/// 相邻两档差（点）。
pub const STEP: u32 = 10;
/// 高宽比下限（最方到正方形）。
pub const RATIO_MIN: f64 = 1.0;
/// 高宽比上限（最长 2）。
pub const RATIO_MAX: f64 = 2.0;
/// 画面上下各让出的高度（点）：对齐 UU 工具栏余量。
pub const UU_INSET: f64 = 91.0;
/// 不知道逻辑宽度时（安卓/手填）按这个估，安卓常见 360–412dp。
pub const ASSUMED_POINTS: f64 = 400.0;
/// 虚拟显示器刷新率（档位共用）。
pub const REFRESH_RATE: u32 = 60;

/// 用户手机的一块屏：物理像素宽高 + 逻辑宽度（点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhoneSpec {
    pub width: u32,
    pub height: u32,
    /// 逻辑宽度（点）；`None` = 未知，按 [`ASSUMED_POINTS`] 估。
    pub points: Option<u32>,
}

impl PhoneSpec {
    /// 没选过手机时的规格：17 Pro Max（1320×2868 像素，440 点宽）。
    pub const fn standard() -> Self {
        Self {
            width: 1320,
            height: 2868,
            points: Some(440),
        }
    }

    /// 像素宽高有效性（0 或倒置的高宽视为无效，调用方回落 [`Self::standard`]）。
    pub fn is_valid(&self) -> bool {
        self.width > 0 && self.height >= self.width
    }

    /// 换算高宽比：`高/宽 − 2·余量/逻辑宽`，夹到 [`RATIO_MIN`]..=[`RATIO_MAX`]。
    pub fn ratio(&self) -> f64 {
        let points = self
            .points
            .filter(|p| *p > 0)
            .map(f64::from)
            .unwrap_or(ASSUMED_POINTS);
        let raw = f64::from(self.height) / f64::from(self.width) - 2.0 * UU_INSET / points;
        raw.clamp(RATIO_MIN, RATIO_MAX)
    }
}

/// 一套档位（比例 + 基准宽 + 全部可登记宽度）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenFit {
    /// 高宽比（宽 × 比例 = 高）。
    pub ratio: f64,
    /// 基准宽：资源库与能铺满的窗口用这一档。
    pub base_width: u32,
    /// 全部档位宽度（基准档起，每 10 点一档，到 1080）。
    pub widths: Vec<u32>,
}

impl ScreenFit {
    /// 按手机规格算档位。
    pub fn for_phone(phone: &PhoneSpec) -> Self {
        let phone = if phone.is_valid() {
            *phone
        } else {
            PhoneSpec::standard()
        };
        Self::for_ratio(phone.ratio())
    }

    /// 默认档位（17 Pro Max，540×950）。
    pub fn standard() -> Self {
        Self::for_phone(&PhoneSpec::standard())
    }

    /// 按高宽比算档位（比例先夹到允许范围）。
    pub fn for_ratio(ratio: f64) -> Self {
        let ratio = if ratio.is_finite() {
            ratio.clamp(RATIO_MIN, RATIO_MAX)
        } else {
            RATIO_MIN
        };
        // 最窄一档起，找「宽 × 比例」不低于 800 的第一档作基准；凑不到用最宽档。
        let base_width = (MIN_WIDTH..=MAX_WIDTH)
            .step_by(STEP as usize)
            .find(|w| (f64::from(*w) * ratio).round() >= MIN_HEIGHT)
            .unwrap_or(MAX_WIDTH);
        let widths = (base_width..=MAX_WIDTH)
            .step_by(STEP as usize)
            .collect::<Vec<_>>();
        Self {
            ratio,
            base_width,
            widths,
        }
    }

    /// 某一档的逻辑尺寸（点）：`(宽, 高)`，高按比例四舍五入。
    pub fn size(&self, width: u32) -> (u32, u32) {
        (width, (f64::from(width) * self.ratio).round() as u32)
    }

    /// 这一档是否已登记。
    pub fn contains(&self, width: u32) -> bool {
        self.widths.contains(&width)
    }

    /// 窗口该用哪一档。
    ///
    /// `window` 是按 `current` 档铺过之后读回的实际客户区，`area` 是当档可放
    /// 区域：装得下就仍是 `current`；App 不让缩到那么小，就按超出比例往上
    /// 找装得下的最窄一档，超过最宽一档就用最宽一档。
    ///
    /// 先乘后除：`540 × (580 / 540)` 在浮点里是 580.0000000000001，向上取整
    /// 会多进一档——与 snug 同款防溢出写法。
    pub fn width_to_fit(&self, window: (u32, u32), area: (u32, u32), current: u32) -> u32 {
        const TOLERANCE: f64 = 2.0;
        if area.0 == 0 || area.1 == 0 {
            return current;
        }
        let (ww, wh) = (f64::from(window.0), f64::from(window.1));
        let (aw, ah) = (f64::from(area.0), f64::from(area.1));
        if ww <= aw + TOLERANCE && wh <= ah + TOLERANCE {
            return current;
        }
        let base = f64::from(current);
        let needed = (base * ww / aw).max(base * wh / ah);
        self.widths
            .iter()
            .copied()
            .find(|w| f64::from(*w) >= needed.ceil())
            .unwrap_or(MAX_WIDTH)
    }

    /// 屏上的点比设计宽度小了几倍（面板与手势阈值按它放大）。
    pub fn scale(&self, screen_width: u32) -> f64 {
        f64::from(screen_width) / f64::from(DESIGN_WIDTH)
    }

    /// 底部小横条高度（跟档位缩放）。
    pub fn home_bar_height(&self, screen_width: u32) -> u32 {
        (28.0 * self.scale(screen_width)).round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_phone_is_540x950_base() {
        let fit = ScreenFit::standard();
        assert_eq!(fit.base_width, 540);
        assert_eq!(fit.size(540), (540, 950)); // 17 Pro Max 实测尺寸
        assert!(fit.contains(540) && fit.contains(1080));
        assert_eq!(fit.widths.len(), 55); // 540..=1080 step 10
        assert_eq!(fit.widths.first(), Some(&540));
        assert_eq!(fit.widths.last(), Some(&1080));
    }

    #[test]
    fn squat_ratio_raises_base_width_to_reach_min_height() {
        // 1:1 手机（折叠屏）：540 只有 540 高，不足 800 → 基准加宽到 800。
        let fit = ScreenFit::for_ratio(1.0);
        assert_eq!(fit.base_width, 800);
        assert_eq!(fit.size(800), (800, 800));
        assert_eq!(*fit.widths.first().unwrap(), 800);
    }

    #[test]
    fn ratio_is_clamped_to_range() {
        // 带鱼屏手机：原始比例超出 2 → 夹到 2。
        let tall = PhoneSpec {
            width: 1080,
            height: 3000,
            points: Some(400),
        };
        assert_eq!(ScreenFit::for_phone(&tall).ratio, RATIO_MAX);
        // 方屏手机：原始比例低于 1 → 夹到 1。
        let square = PhoneSpec {
            width: 1080,
            height: 1080,
            points: Some(400),
        };
        assert_eq!(ScreenFit::for_phone(&square).ratio, RATIO_MIN);
    }

    #[test]
    fn invalid_phone_falls_back_to_standard() {
        assert_eq!(
            ScreenFit::for_phone(&PhoneSpec {
                width: 0,
                height: 0,
                points: None
            }),
            ScreenFit::standard()
        );
        // 高 < 宽视为倒置，同样回落。
        let upside = PhoneSpec {
            width: 1200,
            height: 800,
            points: Some(400),
        };
        assert_eq!(ScreenFit::for_phone(&upside), ScreenFit::standard());
    }

    #[test]
    fn ratio_uses_points_when_known_and_assumed_when_not() {
        let known = PhoneSpec {
            width: 1320,
            height: 2868,
            points: Some(440),
        };
        assert!((known.ratio() - (2868.0 / 1320.0 - 2.0 * 91.0 / 440.0)).abs() < 1e-9);
        let unknown = PhoneSpec {
            width: 1320,
            height: 2868,
            points: None,
        };
        assert!((unknown.ratio() - (2868.0 / 1320.0 - 2.0 * 91.0 / 400.0)).abs() < 1e-9);
    }

    #[test]
    fn fit_keeps_current_when_window_fits() {
        let fit = ScreenFit::standard();
        // 窗口 520×900 在 540×950 的可放区域里：保持基准档。
        assert_eq!(fit.width_to_fit((520, 900), (540, 950), 540), 540);
        // 容差 2 点内仍算装得下。
        assert_eq!(fit.width_to_fit((542, 952), (540, 950), 540), 540);
    }

    #[test]
    fn fit_raises_width_when_window_too_wide() {
        let fit = ScreenFit::standard();
        // 550 宽超出 540 档，按比例应升一档（实际比例 1.759 → 约 560）。
        let w = fit.width_to_fit((550, 900), (540, 950), 540);
        assert!(w > 540 && w <= 1080, "{w}");
        assert!(fit.contains(w));
    }

    #[test]
    fn fit_raises_width_when_window_too_tall() {
        let fit = ScreenFit::standard();
        // 只高不宽：窗口 540×1000 超出 950 高 → 也需要加宽（高随之增长）。
        let w = fit.width_to_fit((540, 1000), (540, 950), 540);
        assert!(w > 540, "{w}");
    }

    #[test]
    fn fit_caps_at_max_width() {
        let fit = ScreenFit::standard();
        // 夸张的窗口 → 封顶最宽档。
        assert_eq!(fit.width_to_fit((5000, 5000), (540, 950), 540), MAX_WIDTH);
    }

    #[test]
    fn fit_does_not_over_round_on_float_noise() {
        let fit = ScreenFit::standard();
        // 540 × (580/540) = 580.0000000000001；向上取整会多进一档 → 先乘后除后应恰为 580。
        let w = fit.width_to_fit((580, 950), (540, 950), 540);
        assert_eq!(w, 580);
    }

    #[test]
    fn degenerate_area_returns_current() {
        let fit = ScreenFit::standard();
        assert_eq!(fit.width_to_fit((800, 800), (0, 0), 700), 700);
    }

    #[test]
    fn scale_and_home_bar_follow_width() {
        let fit = ScreenFit::standard();
        assert_eq!(fit.scale(540), 1.0);
        assert_eq!(fit.home_bar_height(540), 28);
        assert_eq!(fit.home_bar_height(1080), 56);
    }
}
