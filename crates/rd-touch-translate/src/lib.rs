//! rd-touch-translate — 触摸语义 → 远控事件的手势翻译（改造项 C）。
//!
//! 「手机上怎么操作，内容就怎么动」：
//! - 点一下 → 点击（停惯性场景除外）；单指滑动 → 连续滚动，松手带惯性；
//! - 按住 0.5s 再拖 → 选择；按住 0.5s 松手 → 右键菜单；
//! - 点窗口关闭按钮 → 吞掉点击，交调用方核对（核对不过照常补发点击）。
//!
//! **架构决策（改造计划改造项 C）**：翻译放**控制端**（手机侧把手势识别为
//! scroll/right-click/drag 后直接发协议事件，被控端零改动）——与 snug 相反
//! （snug 改不了 UU 手机端只能在被控端 CGEvent tap）；snug 的价值在状态机
//! 本身与全部实测常数。
//!
//! 纯逻辑零依赖：时间（单调秒）由调用方注入，无 I/O 无线程无第三方 crate。
//! 算法与常数 derived from zephyr4123/snug (MIT)，Sources/SnugCore/Touch/
//! TouchTranslator.swift 与 Scrolling.swift，重写为 Rust；阈值按基准宽度标
//! 定，乘 region 缩放（换档放大手感不变）。
//!
//! 协议预留：输出 [`TouchCommand::Scroll`] 的相位/动量值对齐 CGEvent 常量，
//! 对应 rd-proto `ContinuousScroll`（tag 51）——移动端控制端（未来项目）
//! 接线时映射即可。

pub mod scrolling;
pub mod translator;

pub use scrolling::{Momentum, MomentumPhase, ScrollPhase, ScrollStep, ScrollWheel};
pub use translator::{
    CloseButton, Point, Rect, TouchCommand, TouchInput, TouchKind, TouchRegion, TouchResponse,
    TouchTranslator,
};
