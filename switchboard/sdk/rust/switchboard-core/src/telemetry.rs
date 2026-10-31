//! 遥测契约（规范 03 篇）：StageStats schema v1 + EventBus。
//!
//! 规则：模块只管 emit，落盘/聚合归 SB-Bench/HUD；必填字段缺失 = 不合规（06 篇 L-A 校验）。

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::types::PathKind;

/// 阶段名（03 §1 取值域）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Discovery,
    Punch,
    Relay,
    Transport,
    Media,
    Codec,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Discovery => "discovery",
            Stage::Punch => "punch",
            Stage::Relay => "relay",
            Stage::Transport => "transport",
            Stage::Media => "media",
            Stage::Codec => "codec",
        }
    }
}

/// 统计帧（03 §1，schema v1）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageStats {
    pub v: u8,
    /// Unix 毫秒。
    pub ts_ms: u64,
    pub stage: String,
    pub module: String,
    pub conn: String,
    pub path: PathTag,
    pub rtt_ewma_ms: f32,
    pub loss_ratio: f32,
    pub send_mbps: f32,
    pub recv_mbps: f32,
    pub queue_depth_bytes: u64,
    pub jitter_ms: f32,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// 03 §1：`path.kind` MUST 如实——谎报 = L-C 一票否决。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathTag {
    pub kind: PathKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// 03 §2 事件流。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    pub v: u8,
    pub ts_ms: u64,
    pub event: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

/// 遥测消息。
#[derive(Clone, Debug)]
pub enum TelemetryMsg {
    Stats(Arc<StageStats>),
    Event(Arc<EventRecord>),
}

/// 遥测总线：扇出、broadcast 通道、背压由接收端 lag 承担（丢弃旧帧符合 03 §4）。
#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<TelemetryMsg>,
    start: std::time::Instant,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            tx,
            start: std::time::Instant::now(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TelemetryMsg> {
        self.tx.subscribe()
    }

    pub fn emit_stats(&self, s: StageStats) {
        let _ = self.tx.send(TelemetryMsg::Stats(Arc::new(s)));
    }

    pub fn emit_event(&self, event: &str, detail: serde_json::Value) {
        let _ = self.tx.send(TelemetryMsg::Event(Arc::new(EventRecord {
            v: crate::TELEMETRY_VERSION,
            ts_ms: self.now_ms(),
            event: event.to_string(),
            detail,
        })));
    }
}

/// 必填字段完整性校验（04 篇 M12 / 06 篇 L-A）。
pub fn stats_complete(json: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) => {
            let need = [
                "v",
                "ts_ms",
                "stage",
                "module",
                "conn",
                "path",
                "rtt_ewma_ms",
                "loss_ratio",
                "queue_depth_bytes",
            ];
            need.iter().all(|k| v.get(k).is_some_and(|x| !x.is_null()))
                && v.get("path").and_then(|p| p.get("kind")).is_some()
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 遥测必填字段校验() {
        let ok = r#"{"v":1,"ts_ms":1,"stage":"transport","module":"m","conn":"c",
            "path":{"kind":"direct"},"rtt_ewma_ms":1.0,"loss_ratio":0.0,"queue_depth_bytes":0}"#;
        assert!(stats_complete(ok));
        let bad = r#"{"v":1,"stage":"transport"}"#;
        assert!(!stats_complete(bad));
    }

    #[test]
    fn 事件广播扇出() {
        let bus = EventBus::new();
        let mut r1 = bus.subscribe();
        let mut r2 = bus.subscribe();
        bus.emit_event("frame_drop", serde_json::json!({"count": 3}));
        assert!(matches!(
            r1.try_recv(),
            Ok(TelemetryMsg::Event(e)) if e.event == "frame_drop"
        ));
        assert!(matches!(r2.try_recv(), Ok(TelemetryMsg::Event(_))));
    }
}
