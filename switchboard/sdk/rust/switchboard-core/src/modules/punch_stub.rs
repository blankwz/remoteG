//! 研究级桩模块（L-A）：真实适配未落地，但**契约与原理已入规范**。
//!
//! - `sb.rustdesk.punch-v2`：打洞铁律（规范 01 §4.1）来源；实现路线见
//!   docs/cards/rustdesk-punch.md（当前 master v4 路径已修复 #16488；
//!   残留：对称 NAT 直接转中继、打洞 IP 仍取自 TCP 观察地址）。
//! - `sb.derp.self-hk`：headscale+derper 自托管中继，process 绑定（05 §2）。
//! - `sb.kcp.kcp`：kcp-sys new_turbo 直连传输（RustDesk 同款参数）。
//!
//! 桩存在意义：让 Plan/Studio 可以引用这些模块并展示原理卡片，执行时诚实报
//! `Unsupported`（06 §1：不许静默）。

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

use crate::stages::{PunchOutcome, Puncher, Relay};
use crate::telemetry::EventBus;
use crate::types::{EndpointCandidate, PathHandle};

/// RustDesk 打洞 v2（铁律版）——L-A 桩。
pub struct RustDeskPunchStub;

#[async_trait]
impl Puncher for RustDeskPunchStub {
    async fn punch(
        &self,
        _peers: Vec<EndpointCandidate>,
        _budget: Duration,
        bus: Arc<EventBus>,
    ) -> PunchOutcome {
        bus.emit_event(
            "stage_transition",
            serde_json::json!({
                "from": "punch", "outcome": "unsupported",
                "module": "sb.rustdesk.punch-v2",
                "note": "研究级桩：真实适配需 hbbs 会合服务；铁律见 spec/01 §4.1"
            }),
        );
        PunchOutcome::Unsupported
    }
}

/// 自托管 DERP 中继——L-A 桩（process 绑定）。
pub struct DerpRelayStub;

#[async_trait]
impl Relay for DerpRelayStub {
    async fn alloc(
        &self,
        _peer: &str,
        _budget: Duration,
        _bus: Arc<EventBus>,
    ) -> anyhow::Result<PathHandle> {
        anyhow::bail!("sb.derp.self-hk 为 process 绑定桩：需先部署 headscale+derper 并接入 tailscaled（spec/05 §2）")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 打洞桩_诚实报unsupported() {
        let out = RustDeskPunchStub
            .punch(
                Vec::new(),
                Duration::from_secs(1),
                Arc::new(EventBus::new()),
            )
            .await;
        assert!(matches!(out, PunchOutcome::Unsupported));
    }

    #[tokio::test]
    async fn 中继桩_显式报错不静默() {
        let r = DerpRelayStub
            .alloc("peer", Duration::from_secs(1), Arc::new(EventBus::new()))
            .await;
        assert!(r.is_err());
    }
}
