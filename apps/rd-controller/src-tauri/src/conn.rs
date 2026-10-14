//! 连接层（T-019）：持久化身份 + [`DeviceConn`] 通用连接（RPC / 事件订阅 / 数据流
//! 路由）+ [`ConnManager`]（tauri::State）。
//!
//! # spike 兼容（M0 遗产）
//! `/spike` 页（T-005）继续走旧路径：[`ConnManager::identity`] + [`ConnManager::insert`]
//! （独立于设备连接表的 `HashMap<addr, Arc<rd_transport::Conn>>`），行为不变。
//! 正式设备连接走 [`DeviceConn`] 表，两条路径互不影响。
//! 选择：**保留 spike 旧路径**（不动 spike.rs / video_pump.rs，风险最低）。
//!
//! # 前端帧前缀（05_api §3 之上加 1 字节类型）
//! `[1][VideoHeader][AU]` 视频；`[2][TileHeader][RGBA]` 瓦片（Rust 侧解码）；
//! `[3][TileHeader]` 瓦片失效清除。终端数据无前缀（裸 VT 字节）。
#![allow(dead_code)] // 连接层 API 按任务卡接口清单实现，T-020 ~ T-023 接入

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use rd_proto::v0::{envelope, Codec, Envelope, Hello, HelloAck, PairResult};
use rd_security::{ControllerPairing, PairError, PairStep};
use rd_transport::framing::{encode_tile_header, encode_video_header, TILE_HEADER_LEN, VIDEO_HEADER_LEN};
use rd_transport::{connect, Conn, ControlRx, ControlTx, DataRx, DataTx, Fingerprint, Identity, StreamKind};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Emitter, Manager};
use tokio::sync::{broadcast, oneshot, watch};

/// rd.proto v0。
pub const PROTO_VER: u32 = 0;
/// Agent 默认监听端口（05_api §1 [假设]，可配）。
pub const DEFAULT_AGENT_PORT: u16 = 21118;
/// 单个 RPC 等待响应超时。
pub const RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// 前端帧前缀：视频（`[1][VideoHeader][AU]`）。
pub const FRAME_VIDEO: u8 = 1;
/// 前端帧前缀：瓦片（`[2][TileHeader][RGBA]`，已解码）。
pub const FRAME_TILE: u8 = 2;
/// 前端帧前缀：瓦片失效清除（`[3][TileHeader]`）。
pub const FRAME_INVALIDATE: u8 = 3;
/// TileHeader.fmt：QOI。
pub const TILE_FMT_QOI: u8 = 0;
/// TileHeader.fmt：zstd 压缩的 BGRA。
pub const TILE_FMT_ZSTD_BGRA: u8 = 1;
/// TileHeader.fmt：失效清除（05_api §3）。
pub const TILE_FMT_INVALIDATE: u8 = 255;
/// 协议错误码：未鉴权（05_api §4）。
pub const ERR_CODE_UNAUTH: u32 = 1;
/// 协议错误码：配对码错误/过期（05_api §4）。
pub const ERR_CODE_PAIR: u32 = 2;
/// 事件广播通道容量（积压满时订阅者收到 Lagged）。
const EVENT_CHANNEL_CAP: usize = 64;
/// 终端注册前缓存时长。
const TERM_BUFFER_TTL: Duration = Duration::from_secs(2);
/// 子窗口映射链最大跳数（防御环形映射）。
const MAX_WINDOW_HOPS: usize = 8;

/// 前端帧 Channel（Rust → TS 原始字节）。
pub type FrameChannel = Channel<InvokeResponseBody>;
/// 终端流发送端句柄（accept_bidi 产出，`term_write` 使用）。
pub type TermTx = Arc<tokio::sync::Mutex<DataTx>>;

// ---------------------------------------------------------------------------
// 解码能力（前端探测后经 set_decode_caps 上报，连接时填 Hello）
// ---------------------------------------------------------------------------

/// 本机解码能力（默认 H.264 4:2:0，保证可用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeCaps {
    pub h264_420: bool,
    pub h264_444: bool,
}

impl Default for DecodeCaps {
    fn default() -> Self {
        Self {
            h264_420: true,
            h264_444: false,
        }
    }
}

impl DecodeCaps {
    /// 转 `Hello.decode_caps`（Codec 枚举值数组）与 `supports_444`。
    pub fn to_codecs(self) -> (Vec<i32>, bool) {
        let mut caps = Vec::new();
        if self.h264_420 {
            caps.push(Codec::H264420 as i32);
        }
        if self.h264_444 {
            caps.push(Codec::H264444 as i32);
        }
        (caps, self.h264_444)
    }
}

/// 构造 Hello 消息（`decode_caps` 由前端启动探测后上报）。
pub fn hello_msg(caps: DecodeCaps, device_name: &str) -> envelope::Msg {
    let (decode_caps, supports_444) = caps.to_codecs();
    envelope::Msg::Hello(Hello {
        proto_ver: PROTO_VER,
        device_name: device_name.into(),
        decode_caps,
        supports_444,
    })
}

// ---------------------------------------------------------------------------
// 帧编码（前端前缀格式）与瓦片解码
// ---------------------------------------------------------------------------

/// `[1][VideoHeader][AU]`。
pub fn encode_video_frame(h: &rd_transport::VideoHeader, au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + VIDEO_HEADER_LEN + au.len());
    out.push(FRAME_VIDEO);
    out.extend_from_slice(&encode_video_header(h, au.len()));
    out.extend_from_slice(au);
    out
}

/// `[2][TileHeader][RGBA]`。
pub fn encode_tile_frame(h: &rd_transport::TileHeader, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + TILE_HEADER_LEN + rgba.len());
    out.push(FRAME_TILE);
    out.extend_from_slice(&encode_tile_header(h, rgba.len()));
    out.extend_from_slice(rgba);
    out
}

/// `[3][TileHeader]`（失效清除，无负载）。
pub fn encode_invalidate_frame(h: &rd_transport::TileHeader) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + TILE_HEADER_LEN);
    out.push(FRAME_INVALIDATE);
    out.extend_from_slice(&encode_tile_header(h, 0));
    out
}

/// 瓦片解码为 RGBA（fmt=0 QOI / fmt=1 zstd-BGRA→RGBA）。
pub fn decode_tile_to_rgba(h: &rd_transport::TileHeader, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let expected = u32::from(h.w) as usize * u32::from(h.h) as usize * 4;
    match h.fmt {
        TILE_FMT_QOI => {
            let (header, rgba) =
                qoi::decode_to_vec(data).map_err(|e| anyhow::anyhow!("qoi decode: {e}"))?;
            if header.width != u32::from(h.w) || header.height != u32::from(h.h) {
                anyhow::bail!("qoi 尺寸不符 {}x{}", header.width, header.height);
            }
            if rgba.len() != expected {
                anyhow::bail!("qoi 输出 {} 字节，期望 {}", rgba.len(), expected);
            }
            Ok(rgba)
        }
        TILE_FMT_ZSTD_BGRA => {
            let mut bgra = zstd::stream::decode_all(data).map_err(|e| anyhow::anyhow!("zstd decode: {e}"))?;
            if bgra.len() != expected {
                anyhow::bail!("zstd 输出 {} 字节，期望 {}", bgra.len(), expected);
            }
            for px in bgra.as_chunks_mut::<4>().0 {
                px.swap(0, 2); // BGRA → RGBA
            }
            Ok(bgra)
        }
        other => anyhow::bail!("未知瓦片格式 fmt={other}"),
    }
}

// ---------------------------------------------------------------------------
// RPC 等待表：req_id 分配与响应匹配
// ---------------------------------------------------------------------------

/// `req_id → oneshot` 等待表（id 从 1 起单调分配；0 保留给广播消息）。
#[derive(Debug, Default)]
pub struct ReqTable {
    next: AtomicU32,
    pending: HashMap<u32, oneshot::Sender<Envelope>>,
}

impl ReqTable {
    /// 分配一个未占用、非 0 的 req_id。
    pub fn allocate(&self) -> u32 {
        loop {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            if id != 0 && !self.pending.contains_key(&id) {
                return id;
            }
        }
    }

    /// 登记等待者，返回接收端。
    pub fn register(&mut self, id: u32) -> oneshot::Receiver<Envelope> {
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        rx
    }

    /// 按 req_id 投递响应；无等待者返回 false（响应转交广播）。
    pub fn complete(&mut self, id: u32, env: Envelope) -> bool {
        self.pending.remove(&id).is_some_and(|tx| tx.send(env).is_ok())
    }

    /// 移除等待者（超时/发送失败）；返回是否存在。
    pub fn remove(&mut self, id: u32) -> bool {
        self.pending.remove(&id).is_some()
    }
}

// ---------------------------------------------------------------------------
// 路由注册表：窗口（root/子窗口映射）与终端
// ---------------------------------------------------------------------------

/// 窗口帧路由：任意 window_id（root 或子窗口）→ 前端 Channel。
/// 子窗口经 [`WindowRoutes::map_child`] 映射到 root 的同一 Channel。
pub struct WindowRoutes<C> {
    channels: HashMap<u64, C>,
    child_to_root: HashMap<u64, u64>,
}

impl<C> Default for WindowRoutes<C> {
    fn default() -> Self {
        Self {
            channels: HashMap::new(),
            child_to_root: HashMap::new(),
        }
    }
}

impl<C: Clone> WindowRoutes<C> {
    pub fn register(&mut self, window_id: u64, ch: C) {
        self.channels.insert(window_id, ch);
    }

    /// 注销窗口；同时清除指向它的子窗口映射。
    pub fn unregister(&mut self, window_id: u64) {
        self.channels.remove(&window_id);
        self.child_to_root.retain(|_, root| *root != window_id);
    }

    /// 把子窗口映射到已注册的 root；root 未注册返回 false。
    pub fn map_child(&mut self, child_id: u64, root_id: u64) -> bool {
        if !self.channels.contains_key(&root_id) {
            return false;
        }
        self.child_to_root.insert(child_id, root_id);
        true
    }

    pub fn unmap_child(&mut self, child_id: u64) {
        self.child_to_root.remove(&child_id);
    }

    /// 解析 window_id → Channel：直查注册表，否则沿 child→root 链（有跳数上限，
    /// 环形映射不会死循环）。
    pub fn resolve(&self, window_id: u64) -> Option<C> {
        if let Some(ch) = self.channels.get(&window_id) {
            return Some(ch.clone());
        }
        let mut cur = *self.child_to_root.get(&window_id)?;
        for _ in 0..MAX_WINDOW_HOPS {
            if let Some(ch) = self.channels.get(&cur) {
                return Some(ch.clone());
            }
            cur = *self.child_to_root.get(&cur)?;
        }
        None
    }
}

/// 终端路由：stream_id → 前端 Channel 与 QUIC 发送端（两者独立登记，先到先存）+
/// 注册前字节缓存（[`TERM_BUFFER_TTL`] 过期惰性丢弃）。
pub struct TermRoutes<C, T> {
    channels: HashMap<u64, C>,
    txs: HashMap<u64, T>,
    pending: HashMap<u64, Vec<(Instant, Bytes)>>,
}

impl<C, T> Default for TermRoutes<C, T> {
    fn default() -> Self {
        Self {
            channels: HashMap::new(),
            txs: HashMap::new(),
            pending: HashMap::new(),
        }
    }
}

impl<C: Clone, T: Clone> TermRoutes<C, T> {
    /// accept 到双向终端流时登记发送端（前端注册可能更早/更晚，互不阻塞）。
    pub fn bind_tx(&mut self, stream_id: u64, tx: T) {
        self.txs.insert(stream_id, tx);
    }

    /// 前端注册 Channel；返回需按序补投的缓存字节。
    pub fn register(&mut self, stream_id: u64, ch: C) -> Vec<Bytes> {
        self.prune(Instant::now());
        self.channels.insert(stream_id, ch);
        self.pending
            .remove(&stream_id)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, b)| b)
            .collect()
    }

    /// 返回当前 Channel（已注册则直投）；未注册时缓存字节。
    pub fn buffer(&mut self, stream_id: u64, data: Bytes, now: Instant) -> Option<C> {
        match self.channels.get(&stream_id) {
            Some(ch) => Some(ch.clone()),
            None => {
                self.pending.entry(stream_id).or_default().push((now, data));
                None
            }
        }
    }

    /// 丢弃超过 TTL 的缓存。
    pub fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, v| {
            v.retain(|(at, _)| now.duration_since(*at) < TERM_BUFFER_TTL);
            !v.is_empty()
        });
    }

    /// stream 的发送端（`term_write` 用）。
    pub fn tx_of(&self, stream_id: u64) -> Option<T> {
        self.txs.get(&stream_id).cloned()
    }

    /// 流结束/注销：清理全部状态。
    pub fn remove(&mut self, stream_id: u64) {
        self.channels.remove(&stream_id);
        self.txs.remove(&stream_id);
        self.pending.remove(&stream_id);
    }
}

// ---------------------------------------------------------------------------
// 配对流程（控制端）：包装 ControllerPairing，按 PairResult 推进
// ---------------------------------------------------------------------------

/// 配对状态机的下一步动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairAction {
    /// 发送这条数据（下一帧 `PairRequest.spake2_msg`）。
    Send(Vec<u8>),
    /// 配对完成（双方均确认码与指纹）。
    Done,
    /// 失败（面向用户的提示）。
    Fail(String),
}

/// 控制端配对流程：`start` 出第 1 条 SPAKE2 消息；每收到一条 `PairResult`
/// 调 [`PairingFlow::on_pair_result`] 推进。载体：PairRequest（发）/ PairResult（收）。
pub struct PairingFlow {
    pairing: ControllerPairing,
}

impl PairingFlow {
    /// 用 6 位配对码发起配对，返回状态机与第 1 条 `PairRequest.spake2_msg`。
    pub fn start(code: &str, my_fp: Fingerprint, peer_fp: Fingerprint) -> (Self, Vec<u8>) {
        let (pairing, msg) = ControllerPairing::start(code, &my_fp, &peer_fp);
        (Self { pairing }, msg)
    }

    /// 用一条 PairResult 推进状态机。
    ///
    /// 协议（rd-security pairing.rs）：第 2 条 PairResult 携带 Agent 的 MAC
    /// （`spake2_msg` 字段）；`ok=true` 但载荷为空属协议错误（缺 MAC 确认，
    /// 无法判定码正确性，不得视为成功）。
    pub fn on_pair_result(&mut self, ok: bool, spake2_msg: &[u8], reason: &str) -> PairAction {
        if !ok {
            return PairAction::Fail(if reason.is_empty() {
                "配对失败".into()
            } else {
                format!("配对失败：{reason}")
            });
        }
        if spake2_msg.is_empty() {
            return PairAction::Fail("配对协议错误：对端未回确认数据".into());
        }
        match self.pairing.on_message(spake2_msg) {
            PairStep::Send(msg) => PairAction::Send(msg),
            PairStep::Done => PairAction::Done,
            PairStep::Failed(e) => PairAction::Fail(describe_pair_error(&e)),
        }
    }
}

/// PairError → 面向用户的提示（含锁定剩余时间）。
pub fn describe_pair_error(e: &PairError) -> String {
    match e {
        PairError::WrongCode => "配对码错误".into(),
        PairError::Expired => "配对码已过期，请在被控端重新生成".into(),
        PairError::Locked { until } => {
            let secs = until.saturating_duration_since(Instant::now()).as_secs();
            let mins = secs / 60 + u64::from(secs % 60 > 0);
            format!("失败次数过多，已锁定约 {mins} 分钟")
        }
        PairError::Protocol => "配对协议错误".into(),
    }
}

/// 解析 Agent 对 PairRequest 的回复（PairResult；Error 时转为错误消息）。
pub fn parse_pair_reply(env: Envelope) -> Result<PairResult, String> {
    match env.msg {
        Some(envelope::Msg::PairRes(r)) => Ok(r),
        Some(envelope::Msg::Error(e)) => match e.code {
            ERR_CODE_PAIR => Err(if e.message.is_empty() {
                "配对码错误或已过期".into()
            } else {
                format!("配对失败：{}", e.message)
            }),
            _ => Err(format!("对端错误 {}: {}", e.code, e.message)),
        },
        _ => Err("对端回复了意外的消息类型".into()),
    }
}

// ---------------------------------------------------------------------------
// 地址解析与握手
// ---------------------------------------------------------------------------

/// 解析 `host[:port]`（缺省端口补 [`DEFAULT_AGENT_PORT`]；支持 MagicDNS 域名）。
pub async fn resolve_addr(addr: &str, default_port: u16) -> anyhow::Result<SocketAddr> {
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        anyhow::bail!("地址为空");
    }
    let with_port = if trimmed.contains(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:{default_port}")
    };
    tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("解析地址失败：{with_port}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("地址 {with_port} 未解析出任何结果"))
}

/// Hello 握手错误（`pair_device`/`connect_device` 分别处理 Unauthenticated）。
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// Agent 回 `Error{1}`：尚未信任本机（需配对）。
    #[error("未鉴权：{message}")]
    Unauthenticated { code: u32, message: String },
    /// Agent 回其他 Error。
    #[error("对端错误 {code}: {message}")]
    AgentError { code: u32, message: String },
    /// 回复不是 HelloAck。
    #[error("对端回复了意外的消息类型")]
    UnexpectedReply,
    /// 传输层错误（含指纹不匹配）。
    #[error("{0}")]
    Transport(String),
}

fn handshake_error_from_transport(err: anyhow::Error) -> HandshakeError {
    HandshakeError::Transport(format!("{err:#}"))
}

/// `DeviceConn::handshake` 的 Agent 回复分类。
fn classify_hello_reply(resp: Envelope) -> Result<HelloAck, HandshakeError> {
    match resp.msg {
        Some(envelope::Msg::HelloAck(ack)) => Ok(ack),
        Some(envelope::Msg::Error(e)) => {
            if e.code == ERR_CODE_UNAUTH {
                Err(HandshakeError::Unauthenticated {
                    code: e.code,
                    message: e.message,
                })
            } else {
                Err(HandshakeError::AgentError {
                    code: e.code,
                    message: e.message,
                })
            }
        }
        _ => Err(HandshakeError::UnexpectedReply),
    }
}

/// 本机显示名（Hello.device_name）：Windows COMPUTERNAME，其他平台 HOSTNAME。
pub fn host_display_name() -> String {
    #[cfg(windows)]
    let key = "COMPUTERNAME";
    #[cfg(not(windows))]
    let key = "HOSTNAME";
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "rd-controller".into())
}

// ---------------------------------------------------------------------------
// DeviceConn：通用设备连接
// ---------------------------------------------------------------------------

/// 一条设备连接的共享内部状态（后台循环与前台 API 共用）。
struct Inner {
    device_id: String,
    addr: SocketAddr,
    peer_fp: Fingerprint,
    ctrl_tx: tokio::sync::Mutex<ControlTx>,
    pending: Mutex<ReqTable>,
    events_tx: broadcast::Sender<Envelope>,
    windows: Mutex<WindowRoutes<FrameChannel>>,
    terms: Mutex<TermRoutes<FrameChannel, TermTx>>,
    closed_tx: watch::Sender<bool>,
}

/// 通用设备连接（T-020 ~ T-023 共用）：带 req_id 的控制流 RPC + 广播事件 +
/// 视频/瓦片/终端数据流路由 + datagram。
pub struct DeviceConn {
    inner: Arc<Inner>,
    conn: Conn,
}

impl DeviceConn {
    /// 连接 `host[:port]` 设备：解析地址 → QUIC（`expected` 校验对端指纹）→
    /// open_control → 启动后台读循环与数据流 accept 循环。
    /// 不发 Hello（见 [`DeviceConn::handshake`]；配对流程需要区分 `Error{1}`）。
    pub async fn connect(
        device_id: &str,
        addr: &str,
        identity: &Identity,
        expected: Option<Fingerprint>,
    ) -> anyhow::Result<Arc<Self>> {
        let sock = resolve_addr(addr, DEFAULT_AGENT_PORT).await.context("地址解析失败")?;
        let conn = connect(sock, identity, expected).await?;
        let peer_fp = conn.peer_fingerprint();
        let (ctrl_tx, ctrl_rx) = conn.open_control().await?;
        tracing::info!(
            device = device_id,
            addr = %sock,
            peer = %peer_fp.short_hex(),
            "device connected"
        );
        Ok(Self::build(device_id, sock, conn, ctrl_tx, ctrl_rx, peer_fp))
    }

    fn build(
        device_id: &str,
        sock: SocketAddr,
        conn: Conn,
        ctrl_tx: ControlTx,
        ctrl_rx: ControlRx,
        peer_fp: Fingerprint,
    ) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let (closed_tx, _closed_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            device_id: device_id.to_string(),
            addr: sock,
            peer_fp,
            ctrl_tx: tokio::sync::Mutex::new(ctrl_tx),
            pending: Mutex::new(ReqTable::default()),
            events_tx,
            windows: Mutex::new(WindowRoutes::default()),
            terms: Mutex::new(TermRoutes::default()),
            closed_tx,
        });
        // 读循环 + 数据流 accept 循环；连接断开时整体退出并置 closed
        tauri::async_runtime::spawn(read_loop(inner.clone(), ctrl_rx));
        tauri::async_runtime::spawn(accept_data_loop(conn.clone(), inner.clone()));
        tauri::async_runtime::spawn(accept_bidi_loop(conn.clone(), inner.clone()));
        Arc::new(Self { inner, conn })
    }

    /// Hello 握手：发 Hello（含解码能力）并等待 HelloAck / Error。
    pub async fn handshake(&self, caps: DecodeCaps) -> Result<HelloAck, HandshakeError> {
        let resp = self
            .rpc(hello_msg(caps, &host_display_name()))
            .await
            .map_err(handshake_error_from_transport)?;
        classify_hello_reply(resp)
    }

    /// 发起一个 RPC：分配 req_id → 发送 → 5 s 内等待同 req_id 的响应。
    pub async fn rpc(&self, msg: envelope::Msg) -> anyhow::Result<Envelope> {
        let (req_id, rx) = {
            let mut table = self.inner.pending.lock().expect("req table");
            let id = table.allocate();
            (id, table.register(id))
        };
        let env = Envelope {
            req_id,
            msg: Some(msg),
        };
        if let Err(e) = self.inner.ctrl_tx.lock().await.send(&env).await {
            self.inner.pending.lock().expect("req table").remove(req_id);
            return Err(anyhow::anyhow!("发送 RPC 失败：{e:#}"));
        }
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(anyhow::anyhow!("连接已关闭（等待响应 req_id={req_id}）")),
            Err(_) => {
                self.inner
                    .pending
                    .lock()
                    .expect("req table")
                    .remove(req_id);
                Err(anyhow::anyhow!(
                    "RPC 超时（{} s，req_id={req_id}）",
                    RPC_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// 发送无需响应的消息（req_id=0，如 ViewportInfo / TerminalResize）。
    pub async fn send(&self, msg: envelope::Msg) -> anyhow::Result<()> {
        let env = Envelope {
            req_id: 0,
            msg: Some(msg),
        };
        self.inner
            .ctrl_tx
            .lock()
            .await
            .send(&env)
            .await
            .map_err(|e| anyhow::anyhow!("发送失败：{e:#}"))
    }

    /// 订阅广播事件（无 req_id 的消息：WindowEvent / StatsReport / Error）。
    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.inner.events_tx.subscribe()
    }

    /// 发送 QUIC datagram（鼠标移动）。
    pub fn send_datagram(&self, data: Bytes) -> anyhow::Result<()> {
        self.conn.send_datagram(data)
    }

    /// 注册 root 窗口的帧 Channel（视频/瓦片按帧头 window_id 路由）。
    pub fn register_window(&self, root_id: u64, ch: FrameChannel) {
        self.inner
            .windows
            .lock()
            .expect("window routes")
            .register(root_id, ch);
    }

    /// 子窗口 → root 映射（WindowEvent CHILD_ADDED 后由会话层登记）。
    pub fn map_child_window(&self, child_id: u64, root_id: u64) -> bool {
        self.inner
            .windows
            .lock()
            .expect("window routes")
            .map_child(child_id, root_id)
    }

    pub fn unregister_window(&self, root_id: u64) {
        self.inner
            .windows
            .lock()
            .expect("window routes")
            .unregister(root_id);
    }

    /// 注册终端流的前端 Channel（TerminalOpened 后调用；此前到达的输出先缓存）。
    pub fn register_term(&self, stream_id: u64, ch: FrameChannel) {
        let flush = {
            let mut terms = self.inner.terms.lock().expect("term routes");
            terms.register(stream_id, ch.clone())
        };
        for data in flush {
            let _ = ch.send(InvokeResponseBody::Raw(data.to_vec()));
        }
    }

    /// 写终端输入字节。
    pub async fn term_write(&self, stream_id: u64, data: &[u8]) -> anyhow::Result<()> {
        let tx = {
            let terms = self.inner.terms.lock().expect("term routes");
            terms.tx_of(stream_id)
        };
        let Some(tx) = tx else {
            anyhow::bail!("终端流 {stream_id} 未打开");
        };
        tx.lock().await.send_raw(data).await
    }

    pub fn device_id(&self) -> &str {
        &self.inner.device_id
    }

    pub fn addr(&self) -> SocketAddr {
        self.inner.addr
    }

    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.inner.peer_fp
    }

    /// 主动关闭连接（后台循环随后退出）。
    pub fn close(&self, code: u32, reason: &str) {
        self.conn.close(code, reason);
    }

    /// closed watch；`changed()` 在连接结束（读循环退出）后返回。
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.inner.closed_tx.subscribe()
    }
}

/// 控制流读循环：req_id 响应投递等待者，其余广播；退出时置 closed。
async fn read_loop(inner: Arc<Inner>, mut rx: ControlRx) {
    loop {
        match rx.recv().await {
            Ok(Some(env)) => {
                let delivered = env.req_id != 0 && {
                    let mut table = inner.pending.lock().expect("req table");
                    table.complete(env.req_id, env.clone())
                };
                if delivered {
                    continue;
                }
                let _ = inner.events_tx.send(env);
            }
            Ok(None) => {
                tracing::info!(device = %inner.device_id, "control stream closed by peer");
                break;
            }
            Err(e) => {
                tracing::warn!(device = %inner.device_id, "control stream error: {e:#}");
                break;
            }
        }
    }
    let _ = inner.closed_tx.send(true);
}

/// 单向数据流 accept 循环：视频/瓦片各起一个泵。
async fn accept_data_loop(conn: Conn, inner: Arc<Inner>) {
    loop {
        match conn.accept_data().await {
            Ok((StreamKind::Video, _id, rx)) => {
                tauri::async_runtime::spawn(video_pump(inner.clone(), rx));
            }
            Ok((StreamKind::Tile, _id, rx)) => {
                tauri::async_runtime::spawn(tile_pump(inner.clone(), rx));
            }
            Ok((StreamKind::Terminal, id, _)) => {
                tracing::warn!(device = %inner.device_id, stream = id, "单向终端流（忽略，终端为双向流）");
            }
            Err(e) => {
                tracing::info!(device = %inner.device_id, "data streams ended: {e:#}");
                break;
            }
        }
    }
}

/// 双向数据流 accept 循环：终端。
async fn accept_bidi_loop(conn: Conn, inner: Arc<Inner>) {
    loop {
        match conn.accept_bidi().await {
            Ok((StreamKind::Terminal, stream_id, tx, rx)) => {
                let tx: TermTx = Arc::new(tokio::sync::Mutex::new(tx));
                inner.terms.lock().expect("term routes").bind_tx(stream_id, tx);
                tauri::async_runtime::spawn(term_rx_pump(inner.clone(), stream_id, rx));
            }
            Ok((kind, id, _, _)) => {
                tracing::warn!(device = %inner.device_id, stream = id, kind = ?kind, "非预期双向流（忽略）");
            }
            Err(e) => {
                tracing::info!(device = %inner.device_id, "bidi streams ended: {e:#}");
                break;
            }
        }
    }
}

/// 视频泵：`[1][VideoHeader][AU]` 按 window_id 投递前端。
async fn video_pump(inner: Arc<Inner>, mut rx: DataRx) {
    loop {
        match rx.recv().await {
            Ok(Some(rd_transport::DataFrame::Video(h, au))) => {
                let frame = encode_video_frame(&h, &au);
                deliver_frame(&inner, h.window_id, frame);
            }

            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(device = %inner.device_id, "video pump ended: {e:#}");
                break;
            }
        }
    }
}

/// 瓦片泵：QOI/zstd 解码为 RGBA（`[2][TileHeader][RGBA]`）或转发失效
/// （`[3][TileHeader]`），按 window_id 投递前端。
async fn tile_pump(inner: Arc<Inner>, mut rx: DataRx) {
    loop {
        match rx.recv().await {
            Ok(Some(rd_transport::DataFrame::Tile(h, data))) => {
                if h.fmt == TILE_FMT_INVALIDATE {
                    deliver_frame(&inner, h.window_id, encode_invalidate_frame(&h));
                    continue;
                }
                match decode_tile_to_rgba(&h, &data) {
                    Ok(rgba) => deliver_frame(&inner, h.window_id, encode_tile_frame(&h, &rgba)),
                    Err(e) => {
                        tracing::warn!(device = %inner.device_id, window = h.window_id, "tile decode: {e}")
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(device = %inner.device_id, "tile pump ended: {e:#}");
                break;
            }
        }
    }
}

fn deliver_frame(inner: &Inner, window_id: u64, frame: Vec<u8>) {
    let ch = inner.windows.lock().expect("window routes").resolve(window_id);
    if let Some(ch) = ch {
        let _ = ch.send(InvokeResponseBody::Raw(frame));
    }
}

/// 终端接收泵：已注册直投前端；未注册缓存（2 s TTL）。
async fn term_rx_pump(inner: Arc<Inner>, stream_id: u64, mut rx: DataRx) {
    loop {
        match rx.recv().await {
            Ok(Some(rd_transport::DataFrame::Raw(data))) => {
                let ch = {
                    let mut terms = inner.terms.lock().expect("term routes");
                    terms.prune(Instant::now());
                    terms.buffer(stream_id, data.clone(), Instant::now())
                };
                if let Some(ch) = ch {
                    let _ = ch.send(InvokeResponseBody::Raw(data.to_vec()));
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                inner.terms.lock().expect("term routes").remove(stream_id);
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ConnManager：全局状态（身份 / 两张连接表 / 解码能力 / 数据目录）
// ---------------------------------------------------------------------------

/// 全局连接管理（tauri::State）。
///
/// - 设备表：`HashMap<device_id, Arc<DeviceConn>>`（T-019 正式路径，断线自动移除
///   并发 `device-disconnected` 事件）；
/// - spike 表：`HashMap<addr, Arc<Conn>>`（M0 spike 旧路径，保留兼容）；
/// - 身份：`identity/identity.bin`（rd-security 持久化，重启免重配对）。
pub struct ConnManager {
    identity: Arc<Identity>,
    data_dir: PathBuf,
    device_conns: Mutex<HashMap<String, Arc<DeviceConn>>>,
    spike_conns: Mutex<HashMap<String, Arc<Conn>>>,
    decode_caps: Mutex<DecodeCaps>,
}

impl ConnManager {
    /// 加载（或首次生成）持久化身份并初始化各表。
    ///
    /// 数据目录由应用标识推断（与 tauri `app_data_dir` 一致：
    /// Windows `%APPDATA%\com.rd.controller`），身份存其下 `identity/`。
    pub fn new() -> anyhow::Result<Self> {
        let data_dir = default_data_dir();
        let identity = Arc::new(rd_security::load_or_create_identity(
            &data_dir.join("identity"),
            "rd-controller",
        )?);
        tracing::info!(
            fingerprint = %identity.fingerprint().short_hex(),
            dir = %data_dir.display(),
            "controller identity ready (persisted)"
        );
        Ok(Self {
            identity,
            data_dir,
            device_conns: Mutex::new(HashMap::new()),
            spike_conns: Mutex::new(HashMap::new()),
            decode_caps: Mutex::new(DecodeCaps::default()),
        })
    }

    /// 数据目录（devices.json / trusted.json 所在）。
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// 本机身份（spike 握手与设备连接共用）。
    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    /// 当前解码能力（连接握手时填 Hello）。
    pub fn decode_caps(&self) -> DecodeCaps {
        *self.decode_caps.lock().expect("decode caps")
    }

    /// 前端启动探测后上报解码能力。
    pub fn set_decode_caps(&self, caps: DecodeCaps) {
        *self.decode_caps.lock().expect("decode caps") = caps;
    }

    /// 登记设备连接；旧连接关闭。断线时经 `app` 自动移除并向前端发
    /// `device-disconnected` 事件（主动断开同样触发，前端以此更新离线状态）。
    pub fn put_device(
        &self,
        device_id: String,
        conn: Arc<DeviceConn>,
        app: tauri::AppHandle,
    ) -> Option<Arc<DeviceConn>> {
        let watcher = conn.clone();
        let watched_id = device_id.clone();
        tauri::async_runtime::spawn(async move {
            let mut closed = watcher.closed();
            let _ = closed.changed().await;
            let manager = app.state::<ConnManager>();
            let _removed = manager.take_device(&watched_id);
            let _ = app.emit(
                "device-disconnected",
                serde_json::json!({ "deviceId": watched_id }),
            );
        });
        let old = self
            .device_conns
            .lock()
            .expect("device conns")
            .insert(device_id, conn);
        if let Some(old) = &old {
            old.close(0, "replaced by newer connection");
        }
        old
    }

    pub fn get_device(&self, device_id: &str) -> Option<Arc<DeviceConn>> {
        self.device_conns
            .lock()
            .expect("device conns")
            .get(device_id)
            .cloned()
    }

    /// 移除设备连接（断线清理 / 主动断开），不负责 close。
    pub fn take_device(&self, device_id: &str) -> Option<Arc<DeviceConn>> {
        self.device_conns
            .lock()
            .expect("device conns")
            .remove(device_id)
    }

    // ---- spike 旧路径（M0，保留）----

    /// 登记 spike 连接（按 addr 键），返回被替换的旧连接。
    pub fn insert(&self, device_id: String, conn: Arc<Conn>) -> Option<Arc<Conn>> {
        self.spike_conns
            .lock()
            .expect("conn map must not be poisoned")
            .insert(device_id, conn)
    }
}

/// 应用数据目录（与 tauri identifier `com.rd.controller` 对齐）。
fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("com.rd.controller")
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/com.rd.controller"))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("com.rd.controller")
    }
}

// ---------------------------------------------------------------------------
// 单元测试（任务卡：req_id 分配与响应匹配、子窗口 root 映射；另覆盖配对状态机
// mock 序列、瓦片帧编码/解码、终端缓存 TTL、地址解析、解码能力）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- req_id 分配与响应匹配 ----

    #[test]
    fn req_table_allocates_unique_nonzero_ids() {
        let mut table = ReqTable::default();
        let a = table.allocate();
        let b = table.allocate();
        let c = table.allocate();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(c, 0);
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn req_table_routes_response_to_matching_waiter() {
        let mut table = ReqTable::default();
        let id_a = table.allocate();
        let id_b = table.allocate();
        let mut rx_a = table.register(id_a);
        let mut rx_b = table.register(id_b);

        // 先回 b：只有 b 的等待者收到
        let resp_b = Envelope {
            req_id: id_b,
            msg: None,
        };
        assert!(table.complete(id_b, resp_b));
        assert!(rx_b.try_recv().is_ok());
        assert!(rx_a.try_recv().is_err());

        // 再回 a
        let resp_a = Envelope {
            req_id: id_a,
            msg: None,
        };
        assert!(table.complete(id_a, resp_a));
        assert!(rx_a.try_recv().is_ok());
    }

    #[test]
    fn req_table_unknown_id_is_broadcast() {
        let mut table = ReqTable::default();
        let _ = table.allocate();
        let env = Envelope {
            req_id: 999,
            msg: None,
        };
        // 未知 req_id：complete 失败 → 调用方转交广播
        assert!(!table.complete(999, env));
    }

    #[test]
    fn req_table_timeout_removes_waiter() {
        let mut table = ReqTable::default();
        let id = table.allocate();
        let mut rx = table.register(id);
        assert!(table.remove(id));
        // 移除后 sender drop → receiver 收 Err
        assert!(rx.try_recv().is_err());
        assert!(!table.remove(id));
    }

    // ---- 子窗口 root 映射 ----

    #[test]
    fn child_window_maps_to_root_channel() {
        let mut routes: WindowRoutes<u64> = WindowRoutes::default();
        routes.register(100, 7); // root 100 → 通道标记 7
        assert_eq!(routes.resolve(100), Some(7));
        // 子窗口 101 / 102 都映射到 root 100 的同一通道
        assert!(routes.map_child(101, 100));
        assert!(routes.map_child(102, 100));
        assert_eq!(routes.resolve(101), Some(7));
        assert_eq!(routes.resolve(102), Some(7));
    }

    #[test]
    fn unmapped_window_resolves_to_none() {
        let mut routes: WindowRoutes<u64> = WindowRoutes::default();
        routes.register(100, 7);
        assert_eq!(routes.resolve(404), None);
        // map_child 到未注册 root 失败
        assert!(!routes.map_child(103, 999));
        assert_eq!(routes.resolve(103), None);
    }

    #[test]
    fn unregister_root_clears_child_mapping() {
        let mut routes: WindowRoutes<u64> = WindowRoutes::default();
        routes.register(100, 7);
        assert!(routes.map_child(101, 100));
        routes.unregister(100);
        assert_eq!(routes.resolve(100), None);
        assert_eq!(routes.resolve(101), None);
    }

    #[test]
    fn child_chain_and_cycles_terminate() {
        let mut routes: WindowRoutes<u64> = WindowRoutes::default();
        routes.register(1, 5);
        // 环：2→3→2（不指向注册窗口）——resolve 不应死循环
        routes.child_to_root.insert(2, 3);
        routes.child_to_root.insert(3, 2);
        assert_eq!(routes.resolve(2), None);
        // 长链仍能回溯到 root（跳数上限内）
        let mut prev = 1;
        for id in 10..16 {
            routes.child_to_root.insert(id, prev);
            prev = id;
        }
        assert_eq!(routes.resolve(15), Some(5));
    }

    // ---- 配对状态机：mock Agent 的 Envelope 序列 ----

    #[test]
    fn pairing_full_flow_against_mock_agent() {
        let ctrl_fp = Fingerprint([1u8; 32]);
        let agent_fp = Fingerprint([2u8; 32]);

        // mock Agent（rd-security AgentPairing）：码由 new_code 随机生成，
        // 控制端必须用 current_code() 读出的同一个码
        let mut agent = rd_security::AgentPairing::new();
        let _ = agent.new_code();
        let (code, _) = agent.current_code().expect("有码");

        let (mut flow, msg1) = PairingFlow::start(&code, ctrl_fp, agent_fp);

        let rd_security::PairStep::Send(msg2) = agent.on_message(&msg1, &agent_fp, &ctrl_fp) else {
            panic!("agent 应回第 2 轮 SPAKE2");
        };

        // 控制端收到 PairResult{ok:true, msg2} → 应发 MAC_c
        let PairAction::Send(mac_c) = flow.on_pair_result(true, &msg2, "") else {
            panic!("应推进到发送 MAC");
        };

        // Agent 校验 MAC_c → 回 MAC_a
        let rd_security::PairStep::Send(mac_a) = agent.on_message(&mac_c, &agent_fp, &ctrl_fp)
        else {
            panic!("agent 应回 MAC_a");
        };

        // 控制端校验 MAC_a → Done
        assert_eq!(flow.on_pair_result(true, &mac_a, ""), PairAction::Done);
    }

    #[test]
    fn pairing_rejects_bogus_agent_mac() {
        // 先用同码推进到 AwaitingAgentMac 阶段，再喂垃圾 MAC → Fail("配对码错误")
        let ctrl_fp = Fingerprint([1u8; 32]);
        let agent_fp = Fingerprint([2u8; 32]);
        let mut agent = rd_security::AgentPairing::new();
        let _ = agent.new_code();
        let (code, _) = agent.current_code().expect("有码");
        let (mut flow, msg1) = PairingFlow::start(&code, ctrl_fp, agent_fp);
        let rd_security::PairStep::Send(msg2) = agent.on_message(&msg1, &agent_fp, &ctrl_fp) else {
            panic!("agent 应回第 2 轮");
        };
        let PairAction::Send(_mac_c) = flow.on_pair_result(true, &msg2, "") else {
            panic!("应推进到发送 MAC");
        };
        // Agent 在 MAC 阶段回了信封格式正确但 MAC 值错误的载荷 → Fail("配对码错误")
        // （rd-security 信封：[ver=1][tag=0x02][32B MAC]）
        let mut bogus = vec![1u8, 0x02];
        bogus.extend_from_slice(&[7u8; 32]);
        let action = flow.on_pair_result(true, &bogus, "");
        assert_eq!(action, PairAction::Fail("配对码错误".into()));
    }

    #[test]
    fn pairing_agent_reject_maps_to_fail() {
        let (mut flow, _msg1) =
            PairingFlow::start("000000", Fingerprint([1; 32]), Fingerprint([2; 32]));
        // Agent 拒绝（ok=false）→ Fail 且透传原因
        assert_eq!(
            flow.on_pair_result(false, &[], "locked 600s"),
            PairAction::Fail("配对失败：locked 600s".into())
        );
        // 无原因时用默认文案
        assert_eq!(
            flow.on_pair_result(false, &[], ""),
            PairAction::Fail("配对失败".into())
        );
    }

    #[test]
    fn pairing_empty_payload_ok_is_protocol_error() {
        let (mut flow, _msg1) =
            PairingFlow::start("000000", Fingerprint([1; 32]), Fingerprint([2; 32]));
        // ok=true 但缺 spake2_msg：不得视为成功（缺 MAC 确认）
        assert_eq!(
            flow.on_pair_result(true, &[], ""),
            PairAction::Fail("配对协议错误：对端未回确认数据".into())
        );
    }

    #[test]
    fn pair_error_messages_are_user_friendly() {
        assert_eq!(describe_pair_error(&PairError::WrongCode), "配对码错误");
        assert_eq!(
            describe_pair_error(&PairError::Expired),
            "配对码已过期，请在被控端重新生成"
        );
        let msg = describe_pair_error(&PairError::Locked {
            until: Instant::now() + Duration::from_secs(600),
        });
        assert!(msg.contains("锁定") && msg.contains("10"), "{msg}");
    }

    #[test]
    fn parse_pair_reply_maps_agent_error_codes() {
        let env = Envelope {
            req_id: 1,
            msg: Some(envelope::Msg::Error(rd_proto::v0::Error {
                code: ERR_CODE_PAIR,
                message: String::new(),
            })),
        };
        let err = parse_pair_reply(env).expect_err("code=2 应为错误");
        assert!(err.contains("配对码错误"), "{err}");

        let env = Envelope {
            req_id: 1,
            msg: Some(envelope::Msg::Error(rd_proto::v0::Error {
                code: 5,
                message: "boom".into(),
            })),
        };
        let err = parse_pair_reply(env).expect_err("非 PairResult 应为错误");
        assert!(err.contains('5') && err.contains("boom"), "{err}");
    }

    // ---- 帧编码 / 瓦片解码 ----

    #[test]
    fn frame_prefixes_and_headers() {
        let vh = rd_transport::VideoHeader {
            window_id: 9,
            seq: 1,
            pts_us: 2,
            keyframe: true,
        };
        let v = encode_video_frame(&vh, &[0xAA, 0xBB]);
        assert_eq!(v[0], FRAME_VIDEO);
        let (decoded, len) =
            rd_transport::framing::decode_video_header(&v[1..1 + VIDEO_HEADER_LEN]).expect("头");
        assert_eq!(decoded.window_id, 9);
        assert!(decoded.keyframe);
        assert_eq!(len, 2);
        assert_eq!(&v[1 + VIDEO_HEADER_LEN..], &[0xAA, 0xBB]);

        let th = rd_transport::TileHeader {
            window_id: 3,
            frame_seq: 4,
            x: 1,
            y: 2,
            w: 2,
            h: 2,
            fmt: TILE_FMT_QOI,
        };
        let t = encode_tile_frame(&th, &[0u8; 16]);
        assert_eq!(t[0], FRAME_TILE);
        assert_eq!(t.len(), 1 + TILE_HEADER_LEN + 16);

        let inv = encode_invalidate_frame(&th);
        assert_eq!(inv[0], FRAME_INVALIDATE);
        assert_eq!(inv.len(), 1 + TILE_HEADER_LEN);
    }

    #[test]
    fn tile_qoi_roundtrip_and_size_check() {
        // 2x2 RGBA 图 → QOI → 解码回 RGBA
        let rgba: Vec<u8> = vec![
            10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let encoded = qoi::encode_to_vec(&rgba, 2, 2).expect("encode");
        let th = rd_transport::TileHeader {
            window_id: 1,
            frame_seq: 1,
            x: 0,
            y: 0,
            w: 2,
            h: 2,
            fmt: TILE_FMT_QOI,
        };
        let decoded = decode_tile_to_rgba(&th, &encoded).expect("decode");
        assert_eq!(decoded, rgba);
        // 尺寸不符拒绝
        let th_bad = rd_transport::TileHeader { w: 4, h: 1, ..th };
        assert!(decode_tile_to_rgba(&th_bad, &encoded).is_err());
    }

    #[test]
    fn tile_zstd_bgra_converted_to_rgba() {
        let bgra: Vec<u8> = vec![1, 2, 3, 255, 4, 5, 6, 255]; // 1x2
        let compressed = zstd::stream::encode_all(&bgra[..], 3).expect("compress");
        let th = rd_transport::TileHeader {
            window_id: 1,
            frame_seq: 1,
            x: 0,
            y: 0,
            w: 1,
            h: 2,
            fmt: TILE_FMT_ZSTD_BGRA,
        };
        let rgba = decode_tile_to_rgba(&th, &compressed).expect("decode");
        assert_eq!(rgba, vec![3, 2, 1, 255, 6, 5, 4, 255]);
        // 长度不符拒绝
        let th_bad = rd_transport::TileHeader { w: 2, h: 2, ..th };
        assert!(decode_tile_to_rgba(&th_bad, &compressed).is_err());
    }

    #[test]
    fn tile_unknown_fmt_rejected() {
        let th = rd_transport::TileHeader {
            window_id: 1,
            frame_seq: 1,
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            fmt: 7,
        };
        assert!(decode_tile_to_rgba(&th, &[0, 0, 0, 0]).is_err());
    }

    // ---- 终端缓存 ----

    #[test]
    fn term_register_and_buffer_then_flush_in_order() {
        let mut terms: TermRoutes<u64, u64> = TermRoutes::default();
        terms.bind_tx(5, 500);
        let now = Instant::now();
        assert_eq!(terms.buffer(5, Bytes::from_static(b"a"), now), None);
        assert_eq!(terms.buffer(5, Bytes::from_static(b"b"), now), None);
        // 注册后按序补投缓存
        let flush = terms.register(5, 42);
        let joined: Vec<u8> = flush.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(joined, b"ab");
        // 已注册后新数据直投
        assert_eq!(terms.buffer(5, Bytes::from_static(b"c"), now), Some(42));
        assert_eq!(terms.tx_of(5), Some(500));
    }

    #[test]
    fn term_buffer_expires_after_ttl() {
        let mut terms: TermRoutes<u64, u64> = TermRoutes::default();
        terms.bind_tx(5, 500);
        let now = Instant::now();
        terms.buffer(5, Bytes::from_static(b"old"), now);
        // 超过 2 s TTL 后 prune 丢弃
        terms.prune(now + TERM_BUFFER_TTL + Duration::from_millis(1));
        assert!(terms.register(5, 42).is_empty());
    }

    #[test]
    fn term_remove_clears_all_state() {
        let mut terms: TermRoutes<u64, u64> = TermRoutes::default();
        terms.bind_tx(5, 500);
        terms.buffer(5, Bytes::from_static(b"x"), Instant::now());
        terms.register(5, 42);
        terms.remove(5);
        assert!(terms.tx_of(5).is_none());
        assert_eq!(terms.buffer(5, Bytes::from_static(b"y"), Instant::now()), None);
    }

    // ---- 解码能力 → Hello ----

    #[test]
    fn decode_caps_to_hello_fields() {
        let (caps, s444) = DecodeCaps::default().to_codecs();
        assert_eq!(caps, vec![Codec::H264420 as i32]);
        assert!(!s444);

        let both = DecodeCaps {
            h264_420: true,
            h264_444: true,
        };
        let (caps, s444) = both.to_codecs();
        assert_eq!(caps, vec![Codec::H264420 as i32, Codec::H264444 as i32]);
        assert!(s444);
    }

    // ---- 地址解析 ----

    #[tokio::test]
    async fn resolve_addr_fills_default_port() {
        let a = resolve_addr("127.0.0.1", DEFAULT_AGENT_PORT)
            .await
            .expect("ip 无端口");
        assert_eq!(a.port(), DEFAULT_AGENT_PORT);
        let b = resolve_addr("127.0.0.1:21119", DEFAULT_AGENT_PORT)
            .await
            .expect("ip 带端口");
        assert_eq!(b.port(), 21119);
        assert!(resolve_addr("", DEFAULT_AGENT_PORT).await.is_err());
    }

    // ---- 辅助 ----

    #[test]
    fn host_display_name_is_nonempty() {
        assert!(!host_display_name().is_empty());
    }
}
