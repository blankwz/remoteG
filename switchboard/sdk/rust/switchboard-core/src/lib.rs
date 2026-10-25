//! # Switchboard SDK v0.1
//!
//! 规范文本见 `switchboard/spec/`。本 crate 是 01 篇（端口契约）、02 篇（Plan DSL）、
//! 03 篇（遥测）的规范性 Rust 绑定，并附带可运行的参考模块（`modules`）。
//!
//! 布局：
//! - [`caps`]：能力描述与协商（01 §1）
//! - [`types`]：EndpointCandidate / PathHandle（01 §2）
//! - [`channel`]：Channel / Stream 统一传输面（01 §6）
//! - [`stages`]：S1-S5 阶段 trait（01 §3-§8）
//! - [`telemetry`]：StageStats + EventBus（03 篇）
//! - [`plan`]：Plan DSL 解析与校验（02 篇）
//! - [`registry`]：模块 manifest（06 篇）
//! - [`executor`]：计划执行器（02 §2）
//! - [`modules`]：参考模块（loopback / lossy-sim / fec-xor / lan-broadcast / punch 桩）
//!
//! 设计红线：`send_dgram` 队列超限必须丢新包返回 `WouldBlock`，绝不阻塞（01 §6 MUST 2）。

pub mod caps;
pub mod channel;
pub mod executor;
pub mod modules;
pub mod plan;
pub mod registry;
pub mod stages;
pub mod telemetry;
pub mod types;

/// 规范版本（Plan `spec` 字段必须匹配）。
pub const SPEC_VERSION: &str = "switchboard/0.1";
/// 遥测 schema 版本。
pub const TELEMETRY_VERSION: u8 = 1;

pub use bytes::Bytes;
