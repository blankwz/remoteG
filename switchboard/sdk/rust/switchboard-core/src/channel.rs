//! Channel 统一传输面（规范 01 §6）：规范核心。
//!
//! MUST（01 §6）：
//! 1. 业务数据前必须完成指纹互认（由执行器/上层负责，Channel 只提供会话标识）；
//! 2. `send_dgram` 队列超限 MUST 丢新包返回 `WouldBlock`，绝不阻塞；
//! 3. `stats()` 必须反映真实路径（direct/relay 如实标注）；
//! 4. datagram 与 stream 共存。

use async_trait::async_trait;
use bytes::Bytes;

use crate::caps::ChannelCaps;
use crate::telemetry::StageStats;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Control,
    Video,
    Tile,
    Terminal,
    Generic,
}

impl StreamKind {
    pub fn as_u8(self) -> u8 {
        match self {
            StreamKind::Control => 0,
            StreamKind::Video => 1,
            StreamKind::Tile => 2,
            StreamKind::Terminal => 3,
            StreamKind::Generic => 4,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => StreamKind::Control,
            1 => StreamKind::Video,
            2 => StreamKind::Tile,
            3 => StreamKind::Terminal,
            4 => StreamKind::Generic,
            _ => return None,
        })
    }
}

/// 01 §6 MUST 2：队列超限丢新包。
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("发送队列已满，数据报被丢弃（WouldBlock）")]
    WouldBlock,
    #[error("Channel 已关闭")]
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum RecvError {
    #[error("对端已关闭")]
    Closed,
    /// lossy-sim 仿真丢包（真实模块不产生）。
    #[error("报文在仿真信道中丢弃")]
    Dropped,
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("能力不支持：{0}")]
    Unsupported(&'static str),
    #[error("Channel 已关闭")]
    Closed,
}

#[async_trait]
pub trait Stream: Send {
    async fn send(&mut self, b: &[u8]) -> Result<(), ChannelError>;
    /// 对端 finish/关闭后返回 None。
    async fn recv(&mut self) -> Option<Bytes>;
    /// 发送 FIN。
    fn finish(&mut self);
}

/// 统一传输面。
#[async_trait]
pub trait Channel: Send + Sync {
    fn caps(&self) -> &ChannelCaps;
    /// 连接标识（遥测 conn 字段）。
    fn conn_id(&self) -> &str;
    async fn send_dgram(&self, b: Bytes) -> Result<(), SendError>;
    async fn recv_dgram(&self) -> Result<Bytes, RecvError>;
    async fn open_stream(&self, kind: StreamKind) -> Result<Box<dyn Stream>, ChannelError>;
    async fn accept_stream(&self) -> Result<(StreamKind, Box<dyn Stream>), ChannelError>;
    /// 03 篇契约：统一遥测采样点。
    fn stats(&self) -> StageStats;
    fn close(&self);
    fn is_closed(&self) -> bool;
}
