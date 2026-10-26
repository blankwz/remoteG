//! rd × Switchboard 集成层（M2 #4）。
//!
//! 职责切分（低风险切入，DeviceConn 不动）：
//! - **选路交给 Switchboard**：[`plan_dial`] 用 switchboard-core 的 Plan 执行器
//!   做候选解析/路径排序/回退（LAN 发现 → 直连），产出「选中的对端地址 + 路径名」；
//! - **传输留在 rd**：选路结果交给 rd-transport 的 `connect`（证书指纹钉扎身份
//!   体系保持不变），返回 `Arc<rd_transport::Conn>`。
//!
//! 切入点：`DeviceConn::connect` 在 `RD_PLAN` 环境变量存在时改走
//! [`plan_dial`]（默认关闭，行为不变）；Plan 文件示例见 [`default_plan_toml`]。

use async_trait::async_trait;
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use switchboard_core::caps::{ChannelCaps, CongestionKind, DatagramCaps, StreamCaps};
use switchboard_core::channel::{Channel, ChannelError, RecvError, SendError, Stream, StreamKind};
use switchboard_core::executor::{execute, ModuleProvider};
use switchboard_core::plan::Plan;
use switchboard_core::stages::{Discoverer, Transport};
use switchboard_core::telemetry::{EventBus, PathTag, Stage, StageStats};
use switchboard_core::types::{PathHandle, PathKind};

use rd_transport::{Conn, Fingerprint, Identity};

/// rd 传输的 caps（与 rd-transport 实测对齐：datagram=quinn 不可靠数据报，
/// MTU 钳 1200 = tailscale 1280 经验）。
fn rd_caps() -> ChannelCaps {
    ChannelCaps {
        datagram: Some(DatagramCaps { max_size: 1152, lossy_ok: true }),
        streams: Some(StreamCaps { reliable_ordered: true, bidi: true, max_concurrent: 256 }),
        congestion: vec![CongestionKind::Cubic],
        mtu: 1200,
        multipath: false,
    }
}

fn lan_discovery() -> Arc<dyn Discoverer> {
    Arc::new(switchboard_core::modules::lanbcast::LanBroadcast::default())
}

// ── Transport 适配：rd connect 封装为 Switchboard Transport ──

pub struct RdQuicTransport {
    identity: Arc<Identity>,
    expected: Option<Fingerprint>,
    /// 直连拨号地址（discovery 未提供候选时的用户指定地址，02 §2 语义）。
    default_addr: Mutex<Option<SocketAddr>>,
    caps: ChannelCaps,
    /// establish 产出的 Conn 槽（Box<dyn Channel> 不可下转，plan_dial 经此取回）。
    last_conn: Mutex<Option<Arc<Conn>>>,
}

impl RdQuicTransport {
    pub fn new(identity: Arc<Identity>, expected: Option<Fingerprint>) -> Self {
        Self {
            identity,
            expected,
            default_addr: Mutex::new(None),
            caps: rd_caps(),
            last_conn: Mutex::new(None),
        }
    }

    pub fn set_default_addr(&self, addr: SocketAddr) {
        *self.default_addr.lock().unwrap() = Some(addr);
    }
}

#[async_trait]
impl Transport for RdQuicTransport {
    fn offers(&self) -> &ChannelCaps {
        &self.caps
    }

    async fn establish(
        &self,
        paths: Vec<PathHandle>,
        bus: Arc<EventBus>,
    ) -> anyhow::Result<Box<dyn Channel>> {
        let addr = paths
            .iter()
            .find_map(|p| p.addr)
            .or_else(|| *self.default_addr.lock().unwrap())
            .ok_or_else(|| anyhow::anyhow!("无可用地址（discovery 无候选且未指定 default_addr）"))?;
        let conn = Arc::new(rd_transport::connect(addr, &self.identity, self.expected).await?);
        *self.last_conn.lock().unwrap() = Some(Arc::clone(&conn));
        bus.emit_event(
            "stage_transition",
            serde_json::json!({ "from": "transport", "outcome": "direct", "addr": addr.to_string() }),
        );
        Ok(Box::new(RdChannel::new(conn, addr)))
    }
}

/// sb StreamKind ↔ rd StreamKind 换算（sb Control 无 rd 对应——控制面走 RPC）。
fn to_rd_kind(kind: StreamKind) -> Option<rd_transport::StreamKind> {
    match kind {
        StreamKind::Video => Some(rd_transport::StreamKind::Video),
        StreamKind::Tile => Some(rd_transport::StreamKind::Tile),
        StreamKind::Terminal => Some(rd_transport::StreamKind::Terminal),
        StreamKind::Control | StreamKind::Generic => None,
    }
}

fn from_rd_kind(kind: rd_transport::StreamKind) -> StreamKind {
    match kind {
        rd_transport::StreamKind::Video => StreamKind::Video,
        rd_transport::StreamKind::Tile => StreamKind::Tile,
        rd_transport::StreamKind::Terminal => StreamKind::Terminal,
    }
}

/// rd `Conn` 的 Switchboard Channel 视图（统一遥测；数据面全部委托）。
pub struct RdChannel {
    conn: Arc<Conn>,
    addr_string: String,
    caps: ChannelCaps,
}

impl RdChannel {
    fn new(conn: Arc<Conn>, addr: SocketAddr) -> Self {
        Self { conn, addr_string: addr.to_string(), caps: rd_caps() }
    }

    /// 从 rd Conn 直接构造 Channel 视图（T-027 fec-shards 视频路径：
    /// datagram 走 Conn 数据报面，与可靠流共存于同一条连接）。
    /// `path` 仅用于调用方表达路径语义，Channel 视图本身不携带。
    pub fn from_conn(conn: Arc<Conn>, _path: PathKind) -> Self {
        Self {
            conn,
            addr_string: "fec-datagrams".into(),
            caps: rd_caps(),
        }
    }
}

#[async_trait]
impl Channel for RdChannel {
    fn caps(&self) -> &ChannelCaps {
        &self.caps
    }

    fn conn_id(&self) -> &str {
        &self.addr_string
    }

    async fn send_dgram(&self, b: Bytes) -> Result<(), SendError> {
        match self.conn.send_datagram(b) {
            Ok(()) => Ok(()),
            Err(e) => {
                if e.to_string().contains("closed") {
                    Err(SendError::Closed)
                } else {
                    // 队列/超限类：丢新包（01 §6 MUST 2）
                    Err(SendError::WouldBlock)
                }
            }
        }
    }

    async fn recv_dgram(&self) -> Result<Bytes, RecvError> {
        self.conn.read_datagram().await.map_err(|_| RecvError::Closed)
    }

    async fn open_stream(&self, kind: StreamKind) -> Result<Box<dyn Stream>, ChannelError> {
        let rd_kind = to_rd_kind(kind).ok_or(ChannelError::Unsupported(
            "Control 走 rd RPC 通道，不经 Switchboard 流视图",
        ))?;
        let tx = self.conn.open_data(rd_kind).await.map_err(|_| ChannelError::Closed)?;
        Ok(Box::new(RdStreamEnd { tx: Some(tx), rx: None }))
    }

    async fn accept_stream(&self) -> Result<(StreamKind, Box<dyn Stream>), ChannelError> {
        let (kind, _sid, rx) = self.conn.accept_data().await.map_err(|_| ChannelError::Closed)?;
        Ok((from_rd_kind(kind), Box::new(RdStreamEnd { tx: None, rx: Some(rx) })))
    }

    fn stats(&self) -> StageStats {
        let s = self.conn.stats();
        StageStats {
            v: switchboard_core::TELEMETRY_VERSION,
            ts_ms: 0,
            stage: Stage::Transport.as_str().into(),
            module: "sb.quinn.quic-rd".into(),
            conn: format!("{:p}", &self.conn),
            path: PathTag {
                kind: PathKind::Direct,
                local: None,
                remote: Some(self.addr_string.clone()),
            },
            rtt_ewma_ms: self.conn.rtt().as_millis() as f32,
            loss_ratio: 0.0,
            send_mbps: 0.0,
            recv_mbps: 0.0,
            queue_depth_bytes: 0,
            jitter_ms: 0.0,
            extra: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("cwnd".into(), s.path.cwnd.into());
                m
            },
        }
    }

    fn close(&self) {
        self.conn.close(0x1000, "rd-switch");
    }

    fn is_closed(&self) -> bool {
        // Conn 无同步探测；v0.2 补观察者
        false
    }
}

/// rd 数据流端适配：发送端（DataTx）与接收端（DataRx）至多其一。
struct RdStreamEnd {
    tx: Option<rd_transport::DataTx>,
    rx: Option<rd_transport::DataRx>,
}

#[async_trait]
impl Stream for RdStreamEnd {
    async fn send(&mut self, b: &[u8]) -> Result<(), ChannelError> {
        let tx = self.tx.as_mut().ok_or(ChannelError::Closed)?;
        tx.send_raw(b).await.map_err(|_| ChannelError::Closed)
    }

    async fn recv(&mut self) -> Option<Bytes> {
        let rx = self.rx.as_mut()?;
        loop {
            match rx.recv().await {
                Ok(Some(frame)) => {
                    // Raw 变体承载透明字节；Video/Tile 属 rd 原管线语义（不出现于此）
                    if let rd_transport::DataFrame::Raw(b) = frame {
                        return Some(b);
                    }
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }

    fn finish(&mut self) {
        if let Some(tx) = self.tx.as_mut() {
            let _ = tx.finish();
        }
    }
}

// ── Provider 与拨号入口 ──

struct RdProvider {
    transport: Arc<RdQuicTransport>,
}

impl ModuleProvider for RdProvider {
    fn discoverer(&self, id: &str) -> Option<Arc<dyn Discoverer>> {
        (id == "sb.lan.udp-broadcast").then(lan_discovery)
    }
    fn puncher(&self, _id: &str) -> Option<Arc<dyn switchboard_core::stages::Puncher>> {
        // rd 现阶段：跨网走 tailnet（process 绑定），直连路径为 LAN/指定地址
        None
    }
    fn relay(&self, _id: &str) -> Option<Arc<dyn switchboard_core::stages::Relay>> {
        None
    }
    fn transport(&self, id: &str) -> Option<Arc<dyn Transport>> {
        (id == "sb.quinn.quic-rd").then(|| Arc::clone(&self.transport) as Arc<dyn Transport>)
    }
}

/// Plan 驱动拨号：执行器选路（LAN 发现/回退），rd 原生传输建立连接。
///
/// 返回 (命中路径名, rd Conn)。`addr_hint` 是直连地址（用户输入或 LAN 发现
/// 之外的来源）；Plan 的 discovery 模块仍会运行并记录候选。
pub async fn plan_dial(
    plan_toml: &str,
    identity: Arc<Identity>,
    expected: Option<Fingerprint>,
    addr_hint: SocketAddr,
) -> anyhow::Result<(String, Conn)> {
    let plan = Plan::from_toml(plan_toml)?;
    let transport = Arc::new(RdQuicTransport::new(identity, expected));
    transport.set_default_addr(addr_hint);
    let prov = RdProvider { transport: Arc::clone(&transport) };
    let est = execute(&plan, &prov, Arc::new(EventBus::new())).await?;
    // 解构：先释放执行器侧的 Channel 视图（它持有 Arc<Conn>），再取回唯一引用
    let switchboard_core::executor::Established { path_name, via_relay: _, channel } = est;
    drop(channel);
    let conn_arc = transport
        .last_conn
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow::anyhow!("执行器未经过 rd 传输（Plan 引用了非 rd transport）"))?;
    let conn = Arc::try_unwrap(conn_arc)
        .map_err(|_| anyhow::anyhow!("Conn 仍被引用（计数>1）"))?;
    Ok((path_name, conn))
}

/// 默认 Plan：LAN 发现 → rd 直连（与 DeviceConn 现行为等价的 Plan 表达）。
pub fn default_plan_toml() -> String {
    "spec = \"switchboard/0.1\"\nname = \"rd-default\"\n\n[[path]]\nname = \"lan-direct\"\ndiscovery = \"sb.lan.udp-broadcast\"\ntransport = \"sb.quinn.quic-rd\"\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plan拨号_命中rd直连路径() {
        // 服务端：rd Server 监听（独立身份）
        let server_id = Arc::new(Identity::generate("agent-test").unwrap());
        let server = rd_transport::Server::bind("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move { let _ = server.accept().await; });

        // 客户端：Plan 执行器选路 → rd 原生 connect
        let client_id = Arc::new(Identity::generate("ctrl-test").unwrap());
        let (path, conn) = plan_dial(&default_plan_toml(), client_id, None, addr).await.unwrap();
        assert_eq!(path, "lan-direct");

        // Conn 可用：控制流打开 + 数据报发送（服务端已 accept，握手完成）
        let ctl_tx = conn.open_control().await.expect("控制流打开");
        let _ = ctl_tx; // 发送 Envelope 需 rd-proto 语义，此处仅验证流建立
        let _ = conn.send_datagram(Bytes::from_static(b"probe"));
    }
}
