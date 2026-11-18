//! 四相手势状态机（snug TouchTranslator.swift 的 Rust 重写）。
//!
//! 吃进按下/拖动/抬起，吐出「原事件放不放行 + 要补发的事件」；时间由调用
//! 方注入（单调秒）。只有按下落在 [`TouchRegion`] 里的那次会被接管。

use crate::scrolling::{Momentum, MomentumPhase, ScrollPhase, ScrollStep, ScrollWheel};

/// 基准宽度下移动不到这么多点算没动（snug TouchTranslator.swift:91 实测值，
/// 约合手机上 8 点）。
pub const SLOP: f64 = 10.0;
/// 按住这么久算长按（snug:93，与 iOS 一致）。
pub const HOLD_DURATION: f64 = 0.5;
/// 用松手前这段时间的位移估算甩出速度（snug:95）。
pub(crate) const FLING_WINDOW: f64 = 0.1;
/// 估速样本至少跨这么久：事件经网络转发会几个挤在几毫秒里一起到，跨度太短
/// 算出来的速度是假的（snug:97）。
pub(crate) const MIN_FLING_SPAN: f64 = 0.04;
/// 基准宽度下甩出速度上限（点/秒）：约滚一千多点就停（snug:99）。
pub(crate) const MAX_FLING_SPEED: f64 = 4000.0;

/// 平面点（f64；snug 的 CGPoint）。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// 平面矩形（左上原点，全局坐标；snug 的 CGRect 简化版）。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.x + self.w && p.y >= self.y && p.y < self.y + self.h
    }
}

/// 手势翻译管的范围（全局坐标）与基准宽度缩放倍数：阈值都乘它。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchRegion {
    pub frame: Rect,
    /// 这块范围比基准宽度宽几倍（手机屏换档时阈值跟着放大，手感不变）
    pub scale: f64,
    /// 手机屏上那个窗口的关闭按钮；没铺窗口时 None
    pub close_button: Option<CloseButton>,
}

/// 窗口关闭按钮的缓存位置（铺好时读的）；按下时用窗口服务器再核对一次
/// （窗口可能被拖走/盖住/进全屏）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CloseButton {
    pub frame: Rect,
    /// 窗口的系统编号
    pub window: u32,
    /// 读按钮时窗口的位置
    pub window_frame: Rect,
}

impl CloseButton {
    /// 这一点上最上面的窗口还是它、且仍在读按钮时的位置。`on_screen`：窗口
    /// 服务器给的屏上窗口（从前到后）。核对不过 → 调用方照常补发点击。
    pub fn is_frontmost(&self, at: Point, on_screen: &[(u32, Rect)]) -> bool {
        if !self.frame.contains(at) {
            return false;
        }
        let Some((id, frame)) = on_screen
            .iter()
            .find(|(_, f)| f.contains(at))
            .map(|(id, f)| (*id, *f))
        else {
            return false;
        };
        id == self.window && frame == self.window_frame
    }
}

/// 翻译器吃的输入（UU 式单指事件）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchInput {
    pub kind: TouchKind,
    pub location: Point,
    /// 单调时间（秒）
    pub time: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchKind {
    Press,
    Drag,
    Release,
}

/// 翻译器吐出的命令（与 snug 的 TouchCommand 同语义）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchCommand {
    /// 补发扣住的那次按下
    ReplayPress,
    /// 补发当前这个事件（原事件吞掉，保证顺序在补发的按下之后）
    ReplayCurrent,
    RightClick(Point),
    Scroll(ScrollStep),
    /// 点了窗口的关闭按钮：原始点击扣着；调用方核对按钮还在（is_frontmost）
    /// 才交给「放回原位再关」，核对不过照常补发这次点击。
    CloseWindow(Point),
}

/// 原事件放不放行 + 要补发的事件。
#[derive(Debug, Clone, PartialEq)]
pub struct TouchResponse {
    pub pass_original: bool,
    pub commands: Vec<TouchCommand>,
}

impl TouchResponse {
    pub const PASS: Self = Self {
        pass_original: true,
        commands: Vec::new(),
    };

    fn swallow(commands: Vec<TouchCommand>) -> Self {
        Self {
            pass_original: false,
            commands,
        }
    }
}

/// 四相手势状态机。
#[derive(Debug, Clone)]
pub struct TouchTranslator {
    phase: Phase,
    origin: Point,
    last: Point,
    /// 估速样本（时间, 位置），最多 30 个
    samples: Vec<(f64, Point)>,
    wheel: ScrollWheel,
    momentum: Option<Momentum>,
    /// 这次按下时范围比基准宽几倍
    scale: f64,
    /// 这次按下停掉了惯性：很快松手只是停住，不补发点击
    press_stopped_coast: bool,
    /// 这次按在窗口的关闭按钮上
    press_on_close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// 按下了，还看不出是点/滑/长按（按下事件先扣着）
    Pending {
        since: f64,
    },
    Scrolling,
    Selecting,
}

impl Default for TouchTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl TouchTranslator {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            origin: Point::default(),
            last: Point::default(),
            samples: Vec::new(),
            wheel: ScrollWheel::default(),
            momentum: None,
            scale: 1.0,
            press_stopped_coast: false,
            press_on_close: false,
        }
    }

    /// 惯性还在走：调用方按 [`Momentum::FRAME_RATE`] 节奏调 [`Self::momentum_frame`]。
    pub fn is_coasting(&self) -> bool {
        self.momentum.is_some()
    }

    /// 吃一个输入。region：接管范围（None = 全吞模式不接管？不——None 表示
    /// 这次按下不落在任何接管范围，原样放行）。
    pub fn handle(&mut self, input: TouchInput, region: Option<&TouchRegion>) -> TouchResponse {
        match input.kind {
            TouchKind::Press => self.press(input, region),
            TouchKind::Drag => self.drag(input),
            TouchKind::Release => self.release(input),
        }
    }

    /// 惯性的下一帧；结束的那一帧之后返回 None。
    pub fn momentum_frame(&mut self) -> Option<ScrollStep> {
        let m = self.momentum.as_mut()?;
        let Some((dx, dy, momentum_phase)) = m.advance_frame() else {
            self.momentum = None;
            return Some(self.wheel.step(
                0.0,
                0.0,
                ScrollPhase::None,
                MomentumPhase::End,
                self.origin,
            ));
        };
        Some(
            self.wheel
                .step(dx, dy, ScrollPhase::None, momentum_phase, self.origin),
        )
    }

    fn press(&mut self, input: TouchInput, region: Option<&TouchRegion>) -> TouchResponse {
        // 新的一按先停掉还在走的惯性（和 iPhone 上按住正在滚的列表一样）
        let stop = self.stop_momentum();
        let Some(region) = region.filter(|r| r.frame.contains(input.location)) else {
            self.phase = Phase::Idle;
            return TouchResponse {
                pass_original: true,
                commands: stop,
            };
        };
        self.scale = region.scale;
        self.press_stopped_coast = !stop.is_empty();
        self.press_on_close = region
            .close_button
            .is_some_and(|b| b.frame.contains(input.location));
        self.phase = Phase::Pending { since: input.time };
        self.origin = input.location;
        self.last = input.location;
        self.samples = vec![(input.time, input.location)];
        self.wheel = ScrollWheel::default();
        TouchResponse::swallow(stop)
    }

    fn drag(&mut self, input: TouchInput) -> TouchResponse {
        match self.phase {
            Phase::Idle | Phase::Selecting => TouchResponse::PASS,
            Phase::Pending { since } => {
                let moved = distance(input.location, self.origin);
                if moved < SLOP * self.scale {
                    return TouchResponse::swallow(vec![]);
                }
                if input.time - since >= HOLD_DURATION {
                    // 按住后再拖：补发按下，接下来照旧拖动（就是原来的选择）
                    self.phase = Phase::Selecting;
                    return TouchResponse::swallow(vec![
                        TouchCommand::ReplayPress,
                        TouchCommand::ReplayCurrent,
                    ]);
                }
                self.phase = Phase::Scrolling;
                let began = self.wheel.step(
                    0.0,
                    0.0,
                    ScrollPhase::Began,
                    MomentumPhase::None,
                    self.origin,
                );
                let step = self.scroll_step(&input);
                TouchResponse::swallow(vec![
                    TouchCommand::Scroll(began),
                    TouchCommand::Scroll(step),
                ])
            }
            Phase::Scrolling => {
                let step = self.scroll_step(&input);
                TouchResponse::swallow(vec![TouchCommand::Scroll(step)])
            }
        }
    }

    fn release(&mut self, input: TouchInput) -> TouchResponse {
        match self.phase {
            Phase::Idle | Phase::Selecting => {
                self.phase = Phase::Idle;
                TouchResponse::PASS
            }
            Phase::Pending { since } => {
                self.phase = Phase::Idle;
                if input.time - since >= HOLD_DURATION {
                    return TouchResponse::swallow(vec![TouchCommand::RightClick(self.origin)]);
                }
                // iPhone 上点一下正在滚的列表只是停住；补发点击会点到手指下
                // 还在移动的东西
                if self.press_stopped_coast {
                    return TouchResponse::swallow(vec![]);
                }
                if self.press_on_close {
                    return TouchResponse::swallow(vec![TouchCommand::CloseWindow(self.origin)]);
                }
                TouchResponse::swallow(vec![TouchCommand::ReplayPress, TouchCommand::ReplayCurrent])
            }
            Phase::Scrolling => {
                self.phase = Phase::Idle;
                let ended = self.wheel.step(
                    0.0,
                    0.0,
                    ScrollPhase::Ended,
                    MomentumPhase::None,
                    self.origin,
                );
                let (vx, vy) = self.fling_velocity(input.time);
                self.momentum = Momentum::new(vx, vy, self.scale);
                TouchResponse::swallow(vec![TouchCommand::Scroll(ended)])
            }
        }
    }

    /// 手指拖动的增量滚动帧（相对上一位置的位移，余量累计成整数像素）。
    fn scroll_step(&mut self, input: &TouchInput) -> ScrollStep {
        let dx = input.location.x - self.last.x;
        let dy = input.location.y - self.last.y;
        self.last = input.location;
        self.samples.push((input.time, input.location));
        if self.samples.len() > 30 {
            let excess = self.samples.len() - 30;
            self.samples.drain(0..excess);
        }
        self.wheel.step(
            dx,
            dy,
            ScrollPhase::Changed,
            MomentumPhase::None,
            self.origin,
        )
    }

    fn stop_momentum(&mut self) -> Vec<TouchCommand> {
        if self.momentum.is_none() {
            return Vec::new();
        }
        self.momentum = None;
        vec![TouchCommand::Scroll(self.wheel.step(
            0.0,
            0.0,
            ScrollPhase::None,
            MomentumPhase::End,
            self.origin,
        ))]
    }

    /// 松手前 [`FLING_WINDOW`] 内的平均速度（点/秒）。松手前停住了（窗口内
    /// 不到两个样本）就不甩；样本跨度不足 [`MIN_FLING_SPAN`] 时往前多取再
    /// 封顶（按这一档放大的）[`MAX_FLING_SPEED`]。
    fn fling_velocity(&self, time: f64) -> (f64, f64) {
        let recent: Vec<(f64, Point)> = self
            .samples
            .iter()
            .copied()
            .filter(|(t, _)| time - *t <= FLING_WINDOW)
            .collect();
        let Some(&first) = recent.first() else {
            return (0.0, 0.0);
        };
        let b = *recent.last().expect("first 存在则 last 存在");
        let mut a = first;
        if b.0 - a.0 < MIN_FLING_SPAN {
            a = *self
                .samples
                .iter()
                .rev()
                .find(|(t, _)| b.0 - *t >= MIN_FLING_SPAN)
                .unwrap_or(&first);
        }
        let dt = b.0 - a.0;
        if dt <= 0.0 {
            return (0.0, 0.0);
        }
        let vx = (b.1.x - a.1.x) / dt;
        let vy = (b.1.y - a.1.y) / dt;
        let speed = vx.hypot(vy);
        let cap = MAX_FLING_SPEED * self.scale;
        if speed <= cap {
            (vx, vy)
        } else {
            (vx / speed * cap, vy / speed * cap)
        }
    }
}

fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}
