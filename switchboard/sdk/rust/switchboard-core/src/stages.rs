//! 阶段 trait（规范 01 §3-§8）：S1 Discovery / S2 Punch / S3 Relay / S4 Transport / S5 MediaAdapt。

use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;

use crate::caps::ChannelCaps;
use crate::channel::Channel;
use crate::telemetry::EventBus;
use crate::types::{EndpointCandidate, PathHandle, PathKind};

/// S1 发现范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Lan,
    Tailnet,
    Public,
}

#[derive(Clone, Copy, Debug)]
pub struct DiscoverCaps {
    pub scope: Scope,
}

/// S1 Discovery（01 §3）。
#[async_trait]
pub trait Discoverer: Send + Sync {
    fn caps(&self) -> DiscoverCaps;
    /// 一次性拉取候选（bench/编辑器预览用；持续订阅语义见 01 §3 watch，0.2 落地）。
    async fn poll_once(&self) -> anyhow::Result<Vec<EndpointCandidate>>;
}

/// S2 Punch 结果。
pub enum PunchOutcome {
    Connected(PathHandle),
    Failed(String),
    /// L-A 研究桩：未实现完整打洞（诚实标注，见 06 篇合规等级）。
    Unsupported,
}

/// S2 Punch（01 §4）。实现 MUST 遵守 01 §4.1 五条打洞铁律。
#[async_trait]
pub trait Puncher: Send + Sync {
    async fn punch(
        &self,
        peers: Vec<EndpointCandidate>,
        budget: Duration,
        bus: Arc<EventBus>,
    ) -> PunchOutcome;
}

/// S3 Relay（01 §5）。
#[async_trait]
pub trait Relay: Send + Sync {
    async fn alloc(&self, peer: &str, budget: Duration, bus: Arc<EventBus>) -> anyhow::Result<PathHandle>;
}

/// S4 Transport（01 §6）。实现 MUST NOT 在内部做媒体语义（分帧/FEC/码控）。
#[async_trait]
pub trait Transport: Send + Sync {
    fn offers(&self) -> &ChannelCaps;
    async fn establish(
        &self,
        paths: Vec<PathHandle>,
        bus: Arc<EventBus>,
    ) -> anyhow::Result<Box<dyn Channel>>;
}

// ── S5 MediaAdapt（01 §8） ──

/// 编码帧。
pub struct EncodedFrame {
    pub frame_id: u32,
    pub data: Bytes,
}

/// 队列深度（码控消费）。
#[derive(Clone, Copy, Debug)]
pub struct QueueDepth {
    pub queued_frames: u32,
}

/// 帧丢失恢复类型（01 §8 recovery）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    /// 请求关键帧（reliable-idr profile）。
    IdRequest,
    /// 长期参考帧（fec-shards + LTR）。
    Ltr,
    /// 无损瓦片刷新（rd）。
    TileRefresh,
}

/// 接收侧产出。
#[derive(Debug)]
pub enum FrameIn {
    Complete { frame_id: u32, data: Bytes },
    /// FEC/重组不可恢复。
    Lost { frame_id: u32 },
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("发送通道关闭")]
    Closed,
}

#[async_trait]
pub trait FrameSink: Send {
    /// 提交一帧；队列超限 MUST 丢本帧并置 loss 标记（01 §8）。
    async fn submit(&mut self, frame: EncodedFrame) -> Result<QueueDepth, MediaError>;
}

#[async_trait]
pub trait FrameSource: Send {
    async fn next(&mut self) -> Option<FrameIn>;
}

/// S5 媒体适配器：把 Channel 绑定成 帧入/帧出 两条路径。
#[async_trait]
pub trait MediaAdapt: Send + Sync {
    fn profile(&self) -> &'static str;
    /// 返回 (发送端, 接收端)；channel 由调用方提供两端的 Arc。
    async fn bind(
        &self,
        tx_side: Arc<dyn Channel>,
        rx_side: Arc<dyn Channel>,
        bus: Arc<EventBus>,
    ) -> anyhow::Result<(Box<dyn FrameSink>, Box<dyn FrameSource>)>;
}

/// 计划中 media.profile（02 §1）与 profile 的对应（01 §8 三种已验证策略）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaProfile {
    /// Moonlight 型：datagram + FEC + 丢帧 + LTR。
    FecShards,
    /// rd 型：可靠流 + 陈旧帧丢弃 + IDR。
    ReliableIdr,
    /// v0.2：控制走流、帧走 datagram+FEC。
    Hybrid,
}

impl MediaProfile {
    /// 该 profile 对 transport 的最低要求（02 §2.3 门控）。
    pub fn transport_requirement(&self, mtu: u16) -> ChannelCaps {
        match self {
            MediaProfile::FecShards | MediaProfile::Hybrid => ChannelCaps {
                datagram: Some(crate::caps::DatagramCaps { max_size: 1024, lossy_ok: true }),
                mtu,
                ..Default::default()
            },
            MediaProfile::ReliableIdr => ChannelCaps {
                streams: Some(crate::caps::StreamCaps {
                    reliable_ordered: true,
                    bidi: true,
                    max_concurrent: 4,
                }),
                mtu,
                ..Default::default()
            },
        }
    }
}

/// 常用：从路径先验构造成本标注（遥测/日志用）。
pub fn path_tag_of(kind: PathKind, via: &str) -> String {
    format!("{kind:?}/{via}")
}
