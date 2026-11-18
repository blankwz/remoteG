//! 滚动事件的组成与惯性（snug Scrolling.swift 的 Rust 重写）。
//!
//! 字段语义对应 CGEvent 的连续滚动（触控板那种带相位的像素滚动）；相位值
//! 对齐 CGEvent 常量，与 rd-proto `ContinuousScroll` 的 phase/momentum 字段
//! 一一对应。

/// kCGScrollWheelEventScrollPhase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollPhase {
    None = 0,
    Began = 1,
    Changed = 2,
    Ended = 4,
}

/// kCGScrollWheelEventMomentumPhase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MomentumPhase {
    None = 0,
    Begin = 1,
    Continue = 2,
    End = 3,
}

/// 一帧连续滚动。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollStep {
    /// 整数像素（小数部分累计到下一帧，慢慢拖也不会一直是 0）
    pub pixels_x: i32,
    pub pixels_y: i32,
    /// 定点精度的原始位移
    pub delta_x: f64,
    pub delta_y: f64,
    pub phase: ScrollPhase,
    pub momentum: MomentumPhase,
    /// 滚动发生的位置：手指按下的地方（滚的是按下处的那个列表）
    pub location: crate::translator::Point,
}

/// 把小数位移累计成整数像素（snug ScrollWheel）。
#[derive(Debug, Default, Clone)]
pub struct ScrollWheel {
    remainder_x: f64,
    remainder_y: f64,
}

impl ScrollWheel {
    pub fn step(
        &mut self,
        dx: f64,
        dy: f64,
        phase: ScrollPhase,
        momentum: MomentumPhase,
        at: crate::translator::Point,
    ) -> ScrollStep {
        self.remainder_x += dx;
        self.remainder_y += dy;
        // toward-zero 舍入：余量带符号保留（snug 的 rounded(.towardZero)）
        let whole_x = self.remainder_x.trunc();
        let whole_y = self.remainder_y.trunc();
        self.remainder_x -= whole_x;
        self.remainder_y -= whole_y;
        ScrollStep {
            pixels_x: whole_x as i32,
            pixels_y: whole_y as i32,
            delta_x: dx,
            delta_y: dy,
            phase,
            momentum,
            location: at,
        }
    }
}

/// 松手后的惯性：每帧速度乘衰减系数，慢到看不出来就停（snug Momentum）。
#[derive(Debug, Clone)]
pub struct Momentum {
    vx: f64,
    vy: f64,
    stop_speed: f64,
    started: bool,
}

impl Momentum {
    /// 每帧速度乘这个系数（60fps 下约 0.8s 停下；snug Scrolling.swift:49 实测值）。
    pub const DECAY: f64 = 0.95;
    /// 60fps（调用方按此节奏驱动 [`Momentum::next`]；snug:47）。
    pub const FRAME_RATE: f64 = 60.0;
    /// 基准宽度下甩出速度低于这个（点/秒）不带惯性（snug:51）。
    pub const MINIMUM_SPEED: f64 = 200.0;
    /// 基准宽度下速度降到这个以下就停（snug:53）。
    pub const STOP_SPEED: f64 = 40.0;

    /// 太慢的不算甩（None）；scale = 范围比基准宽几倍，速度门槛跟着放大。
    pub fn new(vx: f64, vy: f64, scale: f64) -> Option<Self> {
        if vx.hypot(vy) <= Self::MINIMUM_SPEED * scale {
            return None;
        }
        Some(Self {
            vx,
            vy,
            stop_speed: Self::STOP_SPEED * scale,
            started: false,
        })
    }

    /// 下一帧位移 (dx, dy, momentum_phase)；停下时 None。
    /// 相位语义照 snug：首帧 begin、之后 continue。
    pub fn advance_frame(&mut self) -> Option<(f64, f64, MomentumPhase)> {
        self.vx *= Self::DECAY;
        self.vy *= Self::DECAY;
        if self.vx.hypot(self.vy) < self.stop_speed {
            return None;
        }
        let phase = if self.started {
            MomentumPhase::Continue
        } else {
            MomentumPhase::Begin
        };
        self.started = true;
        Some((
            self.vx / Self::FRAME_RATE,
            self.vy / Self::FRAME_RATE,
            phase,
        ))
    }
}
