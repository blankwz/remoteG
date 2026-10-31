//! 计划执行器（规范 02 §2）：按序执行候选路径，首个指纹互认后的 Channel 胜出。

use std::sync::Arc;
use std::time::Duration;

use crate::channel::Channel;
use crate::plan::{budgets, Plan};
use crate::stages::{Discoverer, PunchOutcome, Puncher, Relay, Transport};
use crate::telemetry::EventBus;
use crate::types::PathHandle;

/// 模块解析器：执行器只认模块 id，实现由宿主注入。
pub trait ModuleProvider: Send + Sync {
    fn discoverer(&self, id: &str) -> Option<Arc<dyn Discoverer>>;
    fn puncher(&self, id: &str) -> Option<Arc<dyn Puncher>>;
    fn relay(&self, id: &str) -> Option<Arc<dyn Relay>>;
    fn transport(&self, id: &str) -> Option<Arc<dyn Transport>>;
}

/// 单条路径失败原因（02 §2.1：不许静默）。
#[derive(Debug, Clone)]
pub struct PathFailure {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("全部路径失败：{0:?}")]
    NoPath(Vec<PathFailure>),
}

/// 执行产物。
pub struct Established {
    pub path_name: String,
    /// 02 §2.1 fallback_relay 命中时为 true（码控据此降码率）。
    pub via_relay: bool,
    pub channel: Box<dyn Channel>,
}

/// 执行计划：按序尝试各路径（02 §2.1）。
///
/// v0.1 语义：discovery 产出仅记录（持续订阅 0.2 落地）；punch → transport 或
/// fallback_relay → transport；caps 门控不满足的路径在加载期剔除。
pub async fn execute(
    plan: &Plan,
    prov: &dyn ModuleProvider,
    bus: Arc<EventBus>,
) -> Result<Established, PlanError> {
    let req = plan.transport_requirement(None);
    let mut failures: Vec<PathFailure> = Vec::new();

    for path in &plan.paths {
        match execute_path(path, prov, &req, &bus).await {
            Ok(est) => return Ok(est),
            Err(reason) => failures.push(PathFailure {
                path: path.name.clone(),
                reason,
            }),
        }
    }
    Err(PlanError::NoPath(failures))
}

async fn execute_path(
    path: &crate::plan::PathCfg,
    prov: &dyn ModuleProvider,
    req: &crate::caps::ChannelCaps,
    bus: &Arc<EventBus>,
) -> Result<Established, String> {
    let transport = prov
        .transport(&path.transport)
        .ok_or_else(|| format!("transport 模块未注册: {}", path.transport))?;

    // caps 门控（02 §2.3）
    if !transport.offers().satisfies(req) {
        return Err(format!(
            "transport {} 不满足计划 requirements（offering mtu={}）",
            path.transport,
            transport.offers().mtu
        ));
    }

    // S1 发现（记录候选；失败不阻断——它只是候选来源之一）
    if let Some(id) = &path.discovery {
        match prov.discoverer(id) {
            Some(d) => match d.poll_once().await {
                Ok(cands) => bus.emit_event(
                    "candidates_found",
                    serde_json::json!({ "module": id, "count": cands.len() }),
                ),
                Err(e) => bus.emit_event(
                    "candidates_failed",
                    serde_json::json!({ "module": id, "error": e.to_string() }),
                ),
            },
            None => bus.emit_event("module_missing", serde_json::json!({ "module": id })),
        }
    }

    // S2 打洞 → S3 中继回退（02 §2.1）
    let mut paths: Vec<PathHandle> = Vec::new();
    let mut via_relay = false;
    if let Some(id) = &path.punch {
        if let Some(puncher) = prov.puncher(id) {
            let budget = Duration::from_millis(budgets::PUNCH_MS);
            match puncher.punch(Vec::new(), budget, Arc::clone(bus)).await {
                PunchOutcome::Connected(h) => paths.push(h),
                PunchOutcome::Unsupported => {
                    bus.emit_event(
                        "stage_transition",
                        serde_json::json!({
                            "from": "punch", "to": "relay", "outcome": "unsupported", "module": id
                        }),
                    );
                }
                PunchOutcome::Failed(e) => bus.emit_event(
                    "stage_transition",
                    serde_json::json!({ "from": "punch", "outcome": "failed", "detail": e }),
                ),
            }
        }
    }
    if paths.is_empty() {
        if let Some(relay_id) = &path.fallback_relay {
            let relay = prov
                .relay(relay_id)
                .ok_or_else(|| format!("relay 模块未注册: {relay_id}"))?;
            let budget = Duration::from_millis(budgets::RELAY_MS);
            let handle = relay
                .alloc("peer", budget, Arc::clone(bus))
                .await
                .map_err(|e| format!("中继分配失败: {e}"))?;
            via_relay = handle.kind == crate::types::PathKind::Relay;
            paths.push(handle);
        }
    }

    // S4 传输（02 §5 预算：中继路径放宽）
    let t_budget = if via_relay {
        budgets::TRANSPORT_RELAY_MS
    } else {
        budgets::TRANSPORT_MS
    };
    let _ = t_budget; // v0.1 参考模块瞬时完成；真实模块由自身实现预算

    let channel = transport
        .establish(paths, Arc::clone(bus))
        .await
        .map_err(|e| format!("transport {} 建立失败: {e}", path.transport))?;

    Ok(Established {
        path_name: path.name.clone(),
        via_relay,
        channel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{ChannelCaps, DatagramCaps};
    use crate::modules::loopback::LoopbackPairTransport;

    struct ProvOnly {
        loopback: Arc<LoopbackPairTransport>,
    }

    impl ModuleProvider for ProvOnly {
        fn discoverer(&self, _id: &str) -> Option<Arc<dyn Discoverer>> {
            None
        }
        fn puncher(&self, _id: &str) -> Option<Arc<dyn Puncher>> {
            None
        }
        fn relay(&self, _id: &str) -> Option<Arc<dyn Relay>> {
            None
        }
        fn transport(&self, id: &str) -> Option<Arc<dyn Transport>> {
            (id == "sb.core.loopback").then(|| Arc::clone(&self.loopback) as Arc<dyn Transport>)
        }
    }

    #[tokio::test]
    async fn 计划执行_命中loopback路径() {
        let plan = Plan::from_toml(
            r#"
spec = "switchboard/0.1"
name = "t"

[[path]]
name = "first-missing"
transport = "sb.nope.missing"

[[path]]
name = "loop"
transport = "sb.core.loopback"
"#,
        )
        .unwrap();
        let bus = Arc::new(EventBus::new());
        let prov = ProvOnly {
            loopback: Arc::new(LoopbackPairTransport::new(1200)),
        };
        let est = execute(&plan, &prov, bus).await.unwrap();
        assert_eq!(est.path_name, "loop");
        assert!(!est.via_relay);
    }

    #[tokio::test]
    async fn 全路径失败_输出原因() {
        let plan = Plan::from_toml(
            r#"
spec = "switchboard/0.1"
name = "t"

[[path]]
name = "x"
transport = "sb.nope.missing"
"#,
        )
        .unwrap();
        let bus = Arc::new(EventBus::new());
        let prov = ProvOnly {
            loopback: Arc::new(LoopbackPairTransport::new(1200)),
        };
        match execute(&plan, &prov, bus).await {
            Err(PlanError::NoPath(fs)) => {
                assert_eq!(fs.len(), 1);
                assert!(fs[0].reason.contains("未注册"));
            }
            _ => panic!("期望 NoPath"),
        }
    }

    #[tokio::test]
    async fn caps_门控剔除_不满足mtu的transport() {
        // caps 逻辑复测：mtu 1200 的 offering 不满足 1400 的要求（02 §2.3 门控语义）
        let req = ChannelCaps {
            mtu: 1400,
            ..Default::default()
        };
        let offer = ChannelCaps {
            datagram: Some(DatagramCaps {
                max_size: 1200,
                lossy_ok: true,
            }),
            mtu: 1200,
            ..Default::default()
        };
        assert!(!offer.satisfies(&req));
    }
}
