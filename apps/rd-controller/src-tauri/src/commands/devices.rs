//! 设备管理命令（T-019）：持久化身份 + 设备列表（devices.json）+ 6 位码配对 +
//! 连接/断开/探测 + 解码能力上报。
//!
//! 存储布局（`ConnManager::data_dir`，Windows `%APPDATA%\com.rd.controller`）：
//! - `identity/identity.bin`：本机身份（rd-security，DPAPI 加密）；
//! - `trusted.json`：信任指纹（rd-security `TrustStore`，信任即设备）；
//! - `devices.json`：设备地址/名称等自有信息（`[{id, name, addr, fingerprint_hex, added_at}]`）。

use std::path::Path;
#[cfg(any(windows, target_os = "macos"))]
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(any(windows, target_os = "macos"))]
use rd_agent::session::AgentState;
use rd_proto::v0::{Codec, PairRequest, PairResult, envelope};
use rd_security::TrustStore;
use rd_transport::Fingerprint;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::conn::{
    self, ConnManager, DecodeCaps, DeviceConn, HandshakeError, PairAction, PairingFlow,
};

/// 配对码位数。
const CODE_LEN: usize = 6;
/// probe 连接测试超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// UDP 局域网发现端口（Agent 侧 `discover.rs` 同步定义）。
const DISCOVER_PORT: u16 = 21119;
/// 发现探测报文。
const DISCOVER_QUERY: &[u8] = b"RD_DISCOVER";
/// 收集应答窗口。
const DISCOVER_WINDOW: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------
// UDP 局域网发现（D-013 包1）：免手填 IP
// ---------------------------------------------------------------------------

/// 局域网发现的 Agent（未配对，仅提供"添加设备"建议地址）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredDeviceDto {
    /// Agent 主机名。
    pub name: String,
    /// 建议地址（应答来源 IP:QUIC 端口）。
    pub addr: String,
    /// 对端指纹短 hex（16 字符，辅助辨认，非安全凭据）。
    pub fingerprint_short_hex: String,
}

/// 解析 Agent 应答 `RD_AGENT_v1|主机名|指纹短hex|QUIC端口`，地址取 UDP 来源 IP。
fn parse_discover_reply(buf: &[u8], peer: std::net::SocketAddr) -> Option<DiscoveredDeviceDto> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut parts = text.split('|');
    if parts.next()? != "RD_AGENT_v1" {
        return None;
    }
    let name = parts.next()?.to_string();
    let fp = parts.next()?;
    let port: u16 = parts.next()?.parse().ok()?;
    Some(DiscoveredDeviceDto {
        name,
        addr: format!("{}:{port}", peer.ip()),
        fingerprint_short_hex: fp.chars().take(16).collect(),
    })
}

/// 广播发现局域网内的 rd-agent：向受限广播 + 各私网接口定向广播发送探测，
/// 收集 1.5s 内的应答（发现 ≠ 信任，配对仍需被控端 6 位码）。
#[tauri::command]
pub async fn discover_devices() -> Result<Vec<DiscoveredDeviceDto>, String> {
    use std::collections::{BTreeSet, HashMap};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::net::UdpSocket;
    use tokio::time::{Instant, timeout};

    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("创建发现 socket 失败:{e}"))?;
    socket
        .set_broadcast(true)
        .map_err(|e| format!("启用广播失败:{e}"))?;

    // 受限广播(255.255.255.255)不跨子网;多子网/VPN 场景补各接口定向广播
    let mut targets: BTreeSet<SocketAddr> = BTreeSet::from([SocketAddr::new(
        IpAddr::V4(Ipv4Addr::BROADCAST),
        DISCOVER_PORT,
    )]);
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                if v4.ip.is_loopback() {
                    continue;
                }
                let mask = u32::from(v4.netmask);
                if mask == 0 {
                    continue;
                }
                let broadcast = Ipv4Addr::from(u32::from(v4.ip) | !mask);
                targets.insert(SocketAddr::new(IpAddr::V4(broadcast), DISCOVER_PORT));
            }
        }
    }
    // Tailscale 网段（D-013 包5 用户需求：扫描也要覆盖 tailnet）——广播在
    // 点对点接口无效，改为对本 tailnet 已知节点逐个单播探测（tailscale
    // status --json 的 Peer 列表；未装/未登录时静默跳过）。
    let ts_out = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok();
    let peers: Vec<serde_json::Value> = match ts_out {
        Some(out) => serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()
            .and_then(|js| {
                js.get("Peer")
                    .and_then(|v| v.as_object())
                    .map(|o| o.values().cloned().collect())
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    for peer in &peers {
        let Some(ips) = peer.get("TailscaleIPs").and_then(|v| v.as_array()) else {
            continue;
        };
        for ip in ips {
            if let Some(ip) = ip.as_str().and_then(|s| s.parse::<IpAddr>().ok()) {
                targets.insert(SocketAddr::new(ip, DISCOVER_PORT));
            }
        }
    }
    for target in &targets {
        let _ = socket.send_to(DISCOVER_QUERY, target).await;
    }

    let deadline = Instant::now() + DISCOVER_WINDOW;
    let mut found: HashMap<String, DiscoveredDeviceDto> = HashMap::new();
    let mut buf = [0u8; 256];
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        match timeout(remain, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, peer))) => {
                if let Some(dev) = parse_discover_reply(&buf[..n], peer) {
                    found.entry(dev.addr.clone()).or_insert(dev);
                }
            }
            _ => break,
        }
    }
    let mut list: Vec<DiscoveredDeviceDto> = found.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name).then(a.addr.cmp(&b.addr)));
    Ok(list)
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// 设备条目（前端卡片）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceDto {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub fingerprint_hex: String,
    pub added_at: u64,
    pub online: bool,
    /// 物理网卡 MAC（WOL 远程开机用；空 = 未采集过，前端置灰按钮）。
    pub mac_addrs: Vec<String>,
    /// 原生中继地址（T-032；直连失败时自动回退）。
    pub relay: Option<String>,
}

/// connect_device 返回的 Agent 能力。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAckDto {
    pub proto_ver: u32,
    pub host_name: String,
    pub encode_caps: Vec<String>,
    pub host_version: String,
    /// 物理网卡 MAC（v0.2 T-031，WOL 用）。
    pub mac_addrs: Vec<String>,
}

/// 前端启动探测出的解码能力。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodeCapsDto {
    pub h264_baseline_420: bool,
    pub h264_high_444: bool,
    #[serde(default)]
    pub hevc: bool,
    #[serde(default)]
    pub av1: bool,
}

impl From<DecodeCapsDto> for DecodeCaps {
    fn from(dto: DecodeCapsDto) -> Self {
        Self {
            h264_420: dto.h264_baseline_420,
            h264_444: dto.h264_high_444,
            hevc: dto.hevc,
            av1: dto.av1,
        }
    }
}

fn codec_label(v: i32) -> String {
    match Codec::try_from(v) {
        Ok(Codec::H264420) => "H264_420".into(),
        Ok(Codec::H264444) => "H264_444".into(),
        Ok(Codec::Hevc444) => "HEVC_444".into(),
        Ok(Codec::Av1) => "AV1".into(),
        _ => format!("unknown({v})"),
    }
}

// ---------------------------------------------------------------------------
// devices.json
// ---------------------------------------------------------------------------

/// 设备列表条目（任务卡结构：`[{id, name, addr, fingerprint_hex, added_at}]`；
/// v0.2 T-031 增 `mac_addrs`——旧文件缺字段按空反序列化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub fingerprint_hex: String,
    pub added_at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mac_addrs: Vec<String>,
    /// 原生中继地址（T-032；serde default 兼容旧文件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
}

/// 设备列表存储（与 TrustStore 同款原子写：tmp + fsync + rename）。
pub struct DeviceStore {
    path: std::path::PathBuf,
    devices: Vec<DeviceEntry>,
}

impl DeviceStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let devices = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("读取设备列表失败：{e}"))?;
            serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("解析设备列表失败：{e}"))?
        } else {
            Vec::new()
        };
        Ok(Self {
            path: path.to_path_buf(),
            devices,
        })
    }

    pub fn list(&self) -> Vec<DeviceEntry> {
        self.devices.clone()
    }

    pub fn by_id(&self, id: &str) -> Option<&DeviceEntry> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// 指纹相同 → 更新名称/地址（保留 id 与加入时间）；否则新增。
    pub fn upsert(&mut self, entry: DeviceEntry) -> anyhow::Result<()> {
        if let Some(existing) = self
            .devices
            .iter_mut()
            .find(|d| d.fingerprint_hex == entry.fingerprint_hex)
        {
            existing.name = entry.name;
            existing.addr = entry.addr;
            existing.relay = entry.relay;
        } else {
            self.devices.push(entry);
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> anyhow::Result<Option<DeviceEntry>> {
        let Some(pos) = self.devices.iter().position(|d| d.id == id) else {
            return Ok(None);
        };
        let removed = self.devices.remove(pos);
        self.save()?;
        Ok(Some(removed))
    }

    /// 记录设备 MAC（握手后采集，T-031 WOL 用）。无变化不写盘。
    /// 返回是否有变化。
    pub fn set_mac_addrs(
        &mut self,
        fingerprint_hex: &str,
        macs: Vec<String>,
    ) -> anyhow::Result<bool> {
        let Some(existing) = self
            .devices
            .iter_mut()
            .find(|d| d.fingerprint_hex == fingerprint_hex)
        else {
            return Ok(false);
        };
        if existing.mac_addrs == macs {
            return Ok(false);
        }
        existing.mac_addrs = macs;
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(self.path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|e| anyhow::anyhow!("创建数据目录失败：{e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&self.devices)
            .map_err(|e| anyhow::anyhow!("序列化设备列表失败：{e}"))?;
        let mut file =
            std::fs::File::create(&tmp).map_err(|e| anyhow::anyhow!("写入临时文件失败：{e}"))?;
        file.write_all(json.as_bytes())
            .map_err(|e| anyhow::anyhow!("写入临时文件失败：{e}"))?;
        file.sync_all()
            .map_err(|e| anyhow::anyhow!("fsync 失败：{e}"))?;
        drop(file);
        std::fs::rename(&tmp, &self.path).map_err(|e| anyhow::anyhow!("原子替换设备列表失败：{e}"))
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn open_store(state: &ConnManager) -> Result<DeviceStore, String> {
    DeviceStore::open(&state.data_dir().join("devices.json")).map_err(|e| format!("{e:#}"))
}

fn open_trust(state: &ConnManager) -> Result<TrustStore, String> {
    TrustStore::open(&state.data_dir().join("trusted.json")).map_err(|e| format!("{e:#}"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_fingerprint(hex: &str) -> Result<Fingerprint, String> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("设备指纹数据损坏（非 64 位 hex）".into());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("设备指纹数据损坏：{e}"))?;
    }
    Ok(Fingerprint(out))
}

fn fingerprint_of_hex(hex: &str) -> Fingerprint {
    // open_store 校验过的数据；解析失败兜底为全 0（连接必失败并报指纹不匹配）
    parse_fingerprint(hex).unwrap_or(Fingerprint([0; 32]))
}

fn to_dto(entry: &DeviceEntry, online: bool) -> DeviceDto {
    DeviceDto {
        id: entry.id.clone(),
        name: entry.name.clone(),
        addr: entry.addr.clone(),
        fingerprint_hex: entry.fingerprint_hex.clone(),
        added_at: entry.added_at,
        online,
        mac_addrs: entry.mac_addrs.clone(),
        relay: entry.relay.clone(),
    }
}

/// 传输层连接失败 → 用户可读提示（含 DoD 要求的"设备指纹不匹配"）。
fn describe_connect_error(err: &anyhow::Error) -> String {
    let chain = format!("{err:#}");
    if chain.contains("fingerprint mismatch") {
        "设备指纹不匹配：对端证书已变化（可能已重装），请删除该设备后重新配对".into()
    } else {
        format!("无法连接设备：{chain}")
    }
}

fn describe_handshake_error(e: HandshakeError) -> String {
    match e {
        HandshakeError::Unauthenticated { message, .. } => {
            format!("设备尚未信任本机，请先配对（{message}）")
        }
        HandshakeError::AgentError { code, message } => format!("对端错误 {code}: {message}"),
        HandshakeError::UnexpectedReply => "对端回复了意外的消息类型".into(),
        HandshakeError::Transport(s) => format!("连接失败：{s}"),
    }
}

/// 保存（或更新）设备：devices.json + trusted.json。
fn save_device(
    state: &ConnManager,
    fp: Fingerprint,
    addr: &str,
    name: &str,
) -> Result<DeviceEntry, String> {
    let fp_hex = fp.hex();
    let mut store = open_store(state)?;
    let id = store
        .list()
        .iter()
        .find(|d| d.fingerprint_hex == fp_hex)
        .map(|d| d.id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let entry = DeviceEntry {
        id,
        name: name.to_string(),
        addr: addr.to_string(),
        fingerprint_hex: fp_hex,
        mac_addrs: Vec::new(),
        relay: None,
        added_at: unix_now(),
    };
    store.upsert(entry.clone()).map_err(|e| format!("{e:#}"))?;
    let mut trust = open_trust(state)?;
    trust
        .add(fp, name)
        .map_err(|e| format!("写入信任列表失败：{e:#}"))?;
    Ok(entry)
}

/// 发送一条 PairRequest 并解析 PairResult 回复。
async fn pair_step(conn: &DeviceConn, payload: Vec<u8>) -> Result<PairResult, String> {
    let reply = conn
        .rpc(envelope::Msg::PairReq(PairRequest {
            spake2_msg: payload,
        }))
        .await
        .map_err(|e| format!("配对通信失败：{e:#}"))?;
    conn::parse_pair_reply(reply)
}

/// 配对核心（pair_device 主体）：Hello → Error{1} → 两步 SPAKE2/MAC → 保存。
async fn run_pairing(
    conn: &DeviceConn,
    code: &str,
    addr: &str,
    name: &str,
    state: &ConnManager,
) -> Result<DeviceEntry, String> {
    match conn.handshake(state.decode_caps()).await {
        // Agent 已信任本机（如本机重装但 Agent 侧信任仍在）：免码直接登记
        Ok(_ack) => return save_device(state, conn.peer_fingerprint(), addr, name),
        Err(HandshakeError::Unauthenticated { .. }) => {}
        Err(e) => return Err(describe_handshake_error(e)),
    }

    let my_fp = state.identity().fingerprint();
    let (mut flow, msg1) = PairingFlow::start(code, my_fp, conn.peer_fingerprint());

    // 第 1 步：发 SPAKE2 消息，收 Agent 的 SPAKE2 消息
    let reply1 = pair_step(conn, msg1).await?;
    let mac_c = match flow.on_pair_result(reply1.ok, &reply1.spake2_msg, &reply1.reason) {
        PairAction::Send(mac) => mac,
        PairAction::Done => return Err("对端提前完成配对（协议错误）".into()),
        PairAction::Fail(msg) => return Err(msg),
    };

    // 第 2 步：发 MAC_c，收 Agent 的 MAC_a
    let reply2 = pair_step(conn, mac_c).await?;
    match flow.on_pair_result(reply2.ok, &reply2.spake2_msg, &reply2.reason) {
        PairAction::Done => save_device(state, conn.peer_fingerprint(), addr, name),
        PairAction::Send(_) => Err("对端要求继续配对（协议错误）".into()),
        PairAction::Fail(msg) => Err(msg),
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令（签名与 lib.rs 注册一致）
// ---------------------------------------------------------------------------

/// 列出已配对设备（在线状态取自连接表）。
#[tauri::command]
pub async fn list_devices(state: State<'_, ConnManager>) -> Result<Vec<DeviceDto>, String> {
    let store = open_store(&state)?;
    Ok(store
        .list()
        .iter()
        .map(|entry| {
            let online = state.get_device(&entry.id).is_some();
            to_dto(entry, online)
        })
        .collect())
}

/// 地址规范化：无端口时自动补默认 21118（2026-10-23 用户需求——输入可省
/// 端口号）。`[v6]:p` 带方括号视为已带端口；裸多冒号地址无法区分 host:port
/// 与 IPv6，按已含端口原样保留。
fn normalize_addr(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with('[') || raw.matches(':').count() >= 2 {
        return raw.to_string();
    }
    if raw.contains(':') {
        return raw.to_string();
    }
    format!("{raw}:{}", crate::conn::DEFAULT_AGENT_PORT)
}

/// 用被控端托盘显示的 6 位配对码配对新设备；成功后保存设备并写入信任列表。
#[tauri::command]
pub async fn pair_device(
    addr: String,
    code: String,
    name: String,
    state: State<'_, ConnManager>,
) -> Result<DeviceDto, String> {
    let code = code.trim().to_string();
    if code.len() != CODE_LEN || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err("配对码必须是 6 位数字".into());
    }
    let addr = normalize_addr(addr.trim());
    if addr.is_empty() {
        return Err("设备地址不能为空".into());
    }
    let name = {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            addr.split(':').next().unwrap_or("未命名设备").to_string()
        } else {
            trimmed.to_string()
        }
    };

    // 首连不校验指纹（对端尚未进信任列表），配对成功后按实测指纹保存
    let conn = DeviceConn::connect(
        "pairing",
        &addr,
        state.identity(),
        None,
        crate::commands::devices::media_fec_setting(state.data_dir()),
        None,
        Vec::new(),
    )
    .await
    .map_err(|e| describe_connect_error(&e))?;
    let result = run_pairing(&conn, &code, &addr, &name, &state).await;
    conn.close(0, "pairing done");
    let entry = result?;
    Ok(to_dto(&entry, false))
}

/// 从设备列表与信任列表移除设备；若在线则断开。
#[tauri::command]
pub async fn remove_device(id: String, state: State<'_, ConnManager>) -> Result<(), String> {
    let mut store = open_store(&state)?;
    let removed = store.remove(&id).map_err(|e| format!("{e:#}"))?;
    if let Some(entry) = removed
        && let Ok(fp) = parse_fingerprint(&entry.fingerprint_hex)
    {
        let mut trust = open_trust(&state)?;
        trust
            .remove(&fp)
            .map_err(|e| format!("移除信任失败：{e:#}"))?;
    }
    if let Some(conn) = state.take_device(&id) {
        conn.close(0, "removed");
    }
    Ok(())
}

/// 连接指定设备：QUIC（校验指纹）→ Hello 握手 → 登记连接表。
#[tauri::command]
pub async fn connect_device(
    id: String,
    app: AppHandle,
    state: State<'_, ConnManager>,
) -> Result<HelloAckDto, String> {
    let entry = {
        let store = open_store(&state)?;
        store
            .by_id(&id)
            .cloned()
            .ok_or_else(|| "设备不存在".to_string())?
    };
    let fp = fingerprint_of_hex(&entry.fingerprint_hex);
    // EasyTier 转发入口（T-043，no_tun 组网）：本机节点配置了转发规则时，
    // 直连失败快试 127.0.0.1:转发端口 → mesh → 对端 rd
    let extra_addrs = easytier_forward_addrs();
    // 握手超时（D-013 包2）：纯局域网直连 3s 快速失败；Tailscale (100.x) 放宽到 8s
    // 容纳 DERP/WireGuard 冷启动；配置了原生中继或 EasyTier 转发时放宽到 30s
    let has_relay = entry.relay.as_deref().is_some_and(|s| !s.trim().is_empty());
    let is_tailscale = entry.addr.starts_with("100.");
    let secs = if has_relay || !extra_addrs.is_empty() {
        30
    } else if is_tailscale {
        8
    } else {
        3
    };
    let conn = tokio::time::timeout(
        Duration::from_secs(secs),
        DeviceConn::connect(
            &id,
            &entry.addr,
            state.identity(),
            Some(fp),
            crate::commands::devices::media_fec_setting(state.data_dir()),
            entry.relay.clone(),
            extra_addrs,
        ),
    )
    .await
    .map_err(|_| format!("连接 {} 超时（{secs}s）", entry.addr))?
    .map_err(|e| describe_connect_error(&e))?;
    let ack = match conn.handshake(state.decode_caps()).await {
        Ok(ack) => ack,
        Err(e) => {
            let msg = describe_handshake_error(e);
            tracing::warn!(device = %id, addr = %entry.addr, "握手失败: {msg}");
            conn.close(0, "handshake failed");
            return Err(msg);
        }
    };
    state.put_device(id, conn, app);
    if let Ok(mut trust) = open_trust(&state) {
        let _ = trust.touch(&fp); // 失败不影响连接
    }
    // MAC 采集（T-031 WOL）：握手时被控端上报物理网卡 MAC，存入 devices.json，
    // 供设备关机后「远程开机」。失败不影响连接。
    let mac_addrs = split_macs(&ack.mac_addrs);
    if !mac_addrs.is_empty()
        && let Ok(mut store) = open_store(&state)
    {
        let _ = store.set_mac_addrs(&entry.fingerprint_hex, mac_addrs.clone());
    }
    Ok(HelloAckDto {
        proto_ver: ack.proto_ver,
        host_name: ack.host_name,
        encode_caps: ack.encode_caps.iter().map(|c| codec_label(*c)).collect(),
        host_version: ack.host_version,
        mac_addrs,
    })
}

/// 解析 `HelloAck.mac_addrs`（逗号分隔）→ 去空白的 MAC 列表。
fn split_macs(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 构造 WOL 魔术包：6×0xFF + 16×目标 MAC（T-031）。
fn magic_packet(mac: &str) -> Result<Vec<u8>, String> {
    let mut bytes = [0u8; 6];
    for (i, part) in mac.split(':').enumerate() {
        if i >= 6 {
            return Err(format!("MAC 格式异常（段数多于 6）：{mac}"));
        }
        if part.len() != 2 {
            return Err(format!("MAC 格式异常（段应为两位 hex）：{mac}"));
        }
        bytes[i] = u8::from_str_radix(part, 16).map_err(|_| format!("MAC 格式异常：{mac}"))?;
    }
    if mac.split(':').count() != 6 {
        return Err(format!("MAC 格式异常：{mac}"));
    }
    let mut packet = Vec::with_capacity(102);
    packet.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    Ok(packet)
}

/// 远程开机（T-031 WOL）：向本网段广播该设备的魔术包。
///
/// 局限（诚实边界）：仅限控制端与目标设备**同网段**时有效——跨网段/跨
/// Tailscale 的唤醒需要路由器辅助转发，不在本命令能力内（设备卡片按钮
/// 仅在采集过 MAC 时可用）。
#[tauri::command]
pub async fn wake_device(id: String, state: State<'_, ConnManager>) -> Result<(), String> {
    // 去抖（review L6）：3s 内对同一设备重复点击直接拦下——魔术包冗余发送
    // 无害但刷网，前端连点不再产生 40+ 包风暴
    static LAST_WAKE: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let mut last = LAST_WAKE.lock().expect("wake 去抖锁中毒");
        if let Some(t) = last.get(&id)
            && t.elapsed() < std::time::Duration::from_secs(3)
        {
            return Err("刚发送过唤醒包，请等几秒再看设备是否上线".to_string());
        }
        last.insert(id.clone(), std::time::Instant::now());
    }
    let macs = {
        let store = open_store(&state)?;
        let entry = store.by_id(&id).ok_or_else(|| "设备不存在".to_string())?;
        if entry.mac_addrs.is_empty() {
            return Err("未采集到该设备的网卡 MAC（需至少成功连接过一次）".to_string());
        }
        entry.mac_addrs.clone()
    };
    let packets: Vec<Vec<u8>> = macs
        .iter()
        .map(|m| magic_packet(m))
        .collect::<Result<_, _>>()?;
    let sock = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
        .await
        .map_err(|e| format!("打开唤醒 socket 失败：{e}"))?;
    sock.set_broadcast(true)
        .map_err(|e| format!("开启广播失败：{e}"))?;
    // 每网卡 5 包 × 双端口（9/7），间隔留出网卡处理时间——厂商建议冗余发送
    for packet in &packets {
        for port in [9u16, 7u16] {
            for _ in 0..5 {
                sock.send_to(packet, ("255.255.255.255", port))
                    .await
                    .map_err(|e| format!("发送唤醒包失败：{e}"))?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    tracing::info!(device = %id, macs = ?macs, "WOL 魔术包已发送");
    Ok(())
}

/// 取在线连接（本模块私有，同 session.rs/term.rs 的同名辅助）。
fn get_conn(state: &ConnManager, device_id: &str) -> Result<std::sync::Arc<DeviceConn>, String> {
    state
        .get_device(device_id)
        .ok_or_else(|| "设备未连接".to_string())
}

/// 发送文本到被控端剪贴板（v0.1/D-013 包4）。fire-and-forget；失败由
/// Agent 回 Error{50}（订阅路径日志）。
#[tauri::command]
pub async fn send_clipboard(
    device_id: String,
    text: String,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let conn = get_conn(&state, &device_id)?;
    conn.send(envelope::Msg::Clipboard(rd_proto::v0::ClipboardData {
        write: true,
        text,
    }))
    .await
    .map_err(|e| format!("发送剪贴板失败:{e}"))
}

/// 读取被控端剪贴板文本（RPC 等回读应答；超时 2s）。
#[tauri::command]
pub async fn request_clipboard(
    device_id: String,
    state: State<'_, ConnManager>,
) -> Result<String, String> {
    let conn = get_conn(&state, &device_id)?;
    let env = tokio::time::timeout(
        Duration::from_secs(2),
        conn.rpc(envelope::Msg::Clipboard(rd_proto::v0::ClipboardData {
            write: false,
            text: String::new(),
        })),
    )
    .await
    .map_err(|_| "读取被控端剪贴板超时".to_string())?
    .map_err(|e| format!("读取被控端剪贴板失败:{e}"))?;
    match env.msg {
        Some(envelope::Msg::Clipboard(rd_proto::v0::ClipboardData { text, .. })) => Ok(text),
        Some(envelope::Msg::Error(e)) => Err(e.message),
        _ => Err("被控端剪贴板应答格式异常".into()),
    }
}

/// 断开指定设备。
#[tauri::command]
pub async fn disconnect_device(id: String, state: State<'_, ConnManager>) -> Result<(), String> {
    let conn = state
        .take_device(&id)
        .ok_or_else(|| "设备未连接".to_string())?;
    conn.close(0, "disconnected by user");
    Ok(())
}

/// 探测设备可达性（2 s 超时的连接测试，不发 Hello）。
#[tauri::command]
pub async fn probe_device(id: String, state: State<'_, ConnManager>) -> Result<bool, String> {
    let entry = {
        let store = open_store(&state)?;
        store
            .by_id(&id)
            .cloned()
            .ok_or_else(|| "设备不存在".to_string())?
    };
    let fp = fingerprint_of_hex(&entry.fingerprint_hex);
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        DeviceConn::connect(
            &id,
            &entry.addr,
            state.identity(),
            Some(fp),
            crate::commands::devices::media_fec_setting(state.data_dir()),
            None,
            easytier_forward_addrs(),
        ),
    )
    .await
    {
        Ok(Ok(conn)) => {
            conn.close(0, "probe done");
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// 设置解码能力（前端启动时探测后上报；连接握手时填入 Hello）。
#[tauri::command]
pub async fn set_decode_caps(
    caps: DecodeCapsDto,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    state.set_decode_caps(caps.into());
    tracing::info!(?caps, "decode caps updated");
    Ok(())
}

// ---------------------------------------------------------------------------
// 融合模式被控状态（D-013 包5 UI 化：本机地址/配对码/已被谁连接）
// ---------------------------------------------------------------------------

/// 被控端在线状态（融合版设备页「本机被控」卡）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatusDto {
    /// 本机可达地址（QUIC 监听，含回环外全部）。
    pub listen_addrs: Vec<String>,
    /// 当前在线控制端（谁在控制我）。
    pub peers: Vec<PeerBriefDto>,
    /// 共享是否已暂停。
    pub paused: bool,
}

/// 在线控制端简报。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBriefDto {
    pub name: String,
    pub since_secs: u64,
}

/// 被控端状态快照（融合版：Windows/macOS 下 agent core 与控制端同进程；移动端返回纯控制端提示）。
#[cfg(any(windows, target_os = "macos"))]
#[tauri::command]
pub fn host_status(state: tauri::State<'_, Arc<AgentState>>) -> Result<HostStatusDto, String> {
    use std::sync::atomic::Ordering;
    Ok(HostStatusDto {
        listen_addrs: state
            .listen_addrs
            .lock()
            .expect("listen_addrs 锁中毒")
            .iter()
            .map(|a| a.to_string())
            .collect(),
        peers: state
            .peers
            .lock()
            .expect("peers 锁中毒")
            .values()
            .map(|p| PeerBriefDto {
                name: p.name.clone(),
                since_secs: p
                    .since
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            })
            .collect(),
        paused: state.paused.load(Ordering::SeqCst),
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
#[tauri::command]
pub fn host_status() -> Result<HostStatusDto, String> {
    Err("当前平台仅作为控制端运行（本机被控服务支持 Windows / macOS）".into())
}

/// 暂停/恢复本机被控共享（UX 安全出口：设备页「切断」按钮；与托盘 ID_PAUSE 同语义）。
#[cfg(any(windows, target_os = "macos"))]
#[tauri::command]
pub fn set_host_paused(
    paused: bool,
    state: tauri::State<'_, Arc<AgentState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    use tauri::Emitter;
    state.paused.store(paused, Ordering::SeqCst);
    if paused {
        rd_agent::term_handler::pause_all(&state);
        let _ = app.emit("session-state", 2i32);
        tracing::info!(origin = "controller-ui", "共享已暂停(设备页切断按钮)");
    } else {
        let _ = app.emit("session-state", 3i32);
        tracing::info!(origin = "controller-ui", "共享已恢复");
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
#[tauri::command]
pub fn set_host_paused(_paused: bool) -> Result<(), String> {
    Err("当前平台仅作为控制端运行".into())
}

/// 生成 6 位配对码并返回建议地址（UI 弹卡展示；5 分钟内有效、一次性）。
#[cfg(any(windows, target_os = "macos"))]
#[tauri::command]
pub fn generate_pair_code(state: tauri::State<'_, Arc<AgentState>>) -> Result<PairCodeDto, String> {
    use std::sync::atomic::Ordering;
    let code = {
        let mut pairing = state.pairing.lock().expect("pairing 锁中毒");
        pairing.new_code()
    };
    tracing::info!("UI 生成新配对码(5 分钟内有效)");
    Ok(PairCodeDto {
        code,
        addrs: state
            .listen_addrs
            .lock()
            .expect("listen_addrs 锁中毒")
            .iter()
            .filter(|a| !a.ip().is_loopback())
            .map(|a| a.to_string())
            .collect(),
        paused: state.paused.load(Ordering::SeqCst),
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
#[tauri::command]
pub fn generate_pair_code() -> Result<PairCodeDto, String> {
    Err("当前平台仅作为控制端运行，无本机配对码".into())
}

/// 配对码 + 建议地址。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairCodeDto {
    pub code: String,
    pub addrs: Vec<String>,
    pub paused: bool,
}

/// 编辑已配对设备（名称/地址；指纹与 id 不变，改后需重连生效）。
#[tauri::command]
pub fn edit_device(
    id: String,
    name: String,
    addr: String,
    relay: Option<String>,
    state: State<'_, ConnManager>,
) -> Result<DeviceDto, String> {
    let addr = normalize_addr(addr.trim());
    if addr.is_empty() {
        return Err("设备地址不能为空".into());
    }
    let name = name.trim().to_string();
    let relay = relay
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(raw) = &relay {
        rd_transport::parse_relay_addr(raw).map_err(|e| format!("{e:#}"))?;
    }
    let mut store = open_store(&state)?;
    let mut entry = store
        .by_id(&id)
        .cloned()
        .ok_or_else(|| "设备不存在".to_string())?;
    if !name.is_empty() {
        entry.name = name;
    }
    entry.addr = addr;
    entry.relay = relay;
    store
        .upsert(entry.clone())
        .map_err(|e| format!("保存设备失败:{e}"))?;
    let online = state.get_device(&id).is_some();
    Ok(to_dto(&entry, online))
}

/// Tailscale 链路测试：`tailscale ping --c 3 <ip>` 抓取直达/DERP 中继与延迟。
#[tauri::command]
pub fn tailscale_ping(ip: String) -> Result<String, String> {
    let out = std::process::Command::new("tailscale")
        .args(["ping", "--c", "3", &ip])
        .output()
        .map_err(|e| format!("无法运行 tailscale 命令（本机未安装或不在 PATH）:{e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(text.trim_end().to_string())
}

/// 授权状态快照（设置页「版本与授权」展示用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseStatusDto {
    /// "pro" | "free"
    pub plan: String,
    pub name: String,
    pub devices_max: u32,
}

fn license_dto_from(lic: Option<rd_security::License>) -> LicenseStatusDto {
    match lic {
        Some(l) if l.plan == rd_security::Plan::Pro => LicenseStatusDto {
            plan: "pro".into(),
            name: l.name,
            devices_max: l.devices_max,
        },
        _ => LicenseStatusDto {
            plan: "free".into(),
            name: String::new(),
            devices_max: rd_security::FREE_DEVICE_MAX,
        },
    }
}

/// 读取当前授权状态（`<app_config_dir>/license.key`）。
#[tauri::command]
pub fn get_license_status() -> LicenseStatusDto {
    let dir = crate::platform::app_config_dir();
    license_dto_from(rd_security::load_license(&dir))
}

/// 粘贴授权码激活专业版：先离线 Ed25519 验签，通过后再落盘 license.key。
#[tauri::command]
pub fn activate_license(code: String) -> Result<LicenseStatusDto, String> {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return Err("请先粘贴授权码".into());
    }
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let lic = rd_security::verify_license(trimmed, now).map_err(|e| e.to_string())?;
    let dir = crate::platform::app_config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    let path = dir.join("license.key");
    std::fs::write(&path, format!("{trimmed}\n")).map_err(|e| format!("保存授权文件失败：{e}"))?;
    tracing::info!(plan = ?lic.plan, devices_max = lic.devices_max, "已通过设置页激活授权码");
    Ok(license_dto_from(Some(lic)))
}

#[cfg(test)]
mod addr_tests {
    use super::normalize_addr;

    #[test]
    fn 无端口自动补默认() {
        assert_eq!(normalize_addr("192.168.6.111"), "192.168.6.111:21118");
        assert_eq!(normalize_addr(" 100.81.205.0 "), "100.81.205.0:21118");
        assert_eq!(normalize_addr("rd.example.com"), "rd.example.com:21118");
    }

    #[test]
    fn 已带端口原样保留() {
        assert_eq!(normalize_addr("192.168.6.111:21119"), "192.168.6.111:21119");
        assert_eq!(normalize_addr("[::1]:21118"), "[::1]:21118");
        assert_eq!(normalize_addr("fe80::1"), "fe80::1");
    }
}

#[cfg(test)]
mod wol_tests {
    use super::{DeviceEntry, magic_packet, split_macs};

    #[test]
    fn 魔术包结构_6xff加16次mac() {
        let p = magic_packet("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(p.len(), 102);
        assert!(p[..6].iter().all(|b| *b == 0xFF));
        for chunk in p[6..].as_chunks::<6>().0 {
            assert_eq!(chunk, &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        }
    }

    #[test]
    fn 魔术包拒绝非法mac() {
        assert!(magic_packet("ZZ:BB:CC:DD:EE:FF").is_err());
        assert!(magic_packet("AA:BB:CC").is_err());
        assert!(magic_packet("AA:BB:CC:DD:EE:FF:00").is_err());
    }

    #[test]
    fn mac列表解析_去空白转大写() {
        assert_eq!(
            split_macs(" aa:bb:cc:dd:ee:ff , 11:22:33:44:55:66,"),
            vec!["AA:BB:CC:DD:EE:FF", "11:22:33:44:55:66"]
        );
        assert!(split_macs("").is_empty());
    }

    #[test]
    fn 旧版devices_json缺mac字段可反序列化() {
        let raw =
            r#"{"id":"a","name":"n","addr":"1.2.3.4:21118","fingerprint_hex":"f","added_at":1}"#;
        let entry: DeviceEntry = serde_json::from_str(raw).unwrap();
        assert!(entry.mac_addrs.is_empty());
    }
}

// ---------------------------------------------------------------------------
// 控制端本机剪贴板（2026-10-27 手测修复）：WebView 的 navigator.clipboard
// 在 Tauri v2/WebView2 下默认无权限（readText/writeText 全部 reject），
// 剪贴板互传因此全部无效——改走 Rust 侧 arboard，权限与浏览器无关。
// ---------------------------------------------------------------------------

/// 读控制端本机剪贴板文本（剪贴板互传「复制到远端」的第一步）。
#[tauri::command]
pub async fn local_clipboard_read() -> Result<String, String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        // arboard 非线程安全句柄：每次新建（剪贴板操作低频，开销可忽略）
        tokio::task::spawn_blocking(|| {
            arboard::Clipboard::new()
                .and_then(|mut c| c.get_text())
                .map_err(|e| format!("读取本机剪贴板失败:{e}"))
        })
        .await
        .map_err(|e| format!("剪贴板任务失败:{e}"))?
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        Err("移动端请直接使用系统输入法粘贴".into())
    }
}

/// 写控制端本机剪贴板文本（「从远端复制」的最后一步）。
#[tauri::command]
pub async fn local_clipboard_write(_text: String) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        tokio::task::spawn_blocking(move || {
            arboard::Clipboard::new()
                .and_then(|mut c| c.set_text(_text))
                .map_err(|e| format!("写入本机剪贴板失败:{e}"))
        })
        .await
        .map_err(|e| format!("剪贴板任务失败:{e}"))?
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        Err("移动端请直接使用系统剪贴板".into())
    }
}

// ---------------------------------------------------------------------------
// 方案实验室（Studio 内嵌，2026-10-28 融合）：独立窗口加载本进程内嵌的
// Studio（方案测试/跑分对比），不再单独开浏览器/daemon。
// ---------------------------------------------------------------------------

/// 打开（或聚焦已打开的）原生方案实验室窗口（v0.4 极客台：直接加载 Tauri App `#/lab`）。
#[tauri::command]
pub async fn open_studio(_app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        use tauri::{Manager, WebviewUrl};
        if let Some(win) = _app.get_webview_window("studio") {
            let _ = win.set_focus();
            return Ok(());
        }
        tauri::WebviewWindowBuilder::new(&_app, "studio", WebviewUrl::App("index.html#/lab".into()))
            .title("RD Geek Lab · 方案实验室")
            .inner_size(1280.0, 860.0)
            .min_inner_size(960.0, 640.0)
            .build()
            .map(|_| ())
            .map_err(|e| format!("打开实验室窗口失败:{e}"))
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        Ok(())
    }
}

/// fec 高清链路开关（Studio「方案」落到远程控制的落地开关）：写
/// `settings.json`，连接设备时生效（RS 纠错分片通道，弱网更稳、开销 +20%）。
/// 原子写（临时文件 + rename，与信任列表同一纪律）——写一半崩溃不损坏配置。
#[tauri::command]
pub async fn set_media_fec(enabled: bool, state: State<'_, ConnManager>) -> Result<(), String> {
    let dir = state.data_dir();
    let path = dir.join("settings.json");
    let tmp = dir.join("settings.json.tmp");
    let body = serde_json::json!({ "media_fec": enabled }).to_string();
    std::fs::write(&tmp, body).map_err(|e| format!("写入设置失败:{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("提交设置失败:{e}"))?;
    tracing::info!(enabled, path = %path.display(), "fec 开关已更新");
    Ok(())
}

/// 读 fec 开关当前值（前端展示勾选态；缺省 = 关）。
#[tauri::command]
pub async fn get_media_fec(state: State<'_, ConnManager>) -> Result<bool, String> {
    Ok(media_fec_setting(state.data_dir()))
}

// ---------------------------------------------------------------------------
// 原生中继（T-033）：设置页「远程访问」——写入被控端 agent.toml 的 relay_addr
// （融合版目录 rd_agent::default_config_dir()/app），重启 rd 后生效。
// ---------------------------------------------------------------------------

fn agent_config_file() -> std::path::PathBuf {
    crate::platform::app_config_dir().join("agent.toml")
}

/// 跨平台轻量 agent.toml 视图（仅解析控制端关心的 relay_addr 与 [easytier]，不依赖 Windows-only `rd-agent`）。
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct AgentConfigSubset {
    relay_addr: Option<String>,
    easytier: EasyTierSettingsDto,
}

/// 读当前原生中继地址（未配置 = None）。
#[tauri::command]
pub fn get_relay_addr() -> Result<Option<String>, String> {
    let path = agent_config_file();
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 agent.toml 失败：{e}"))?;
    let cfg: AgentConfigSubset =
        toml::from_str(&raw).map_err(|e| format!("解析 agent.toml 失败：{e}"))?;
    Ok(cfg.relay_addr)
}

/// 写原生中继地址（空串 = 清除）；保留 agent.toml 其他字段，原子写。
#[tauri::command]
pub fn set_relay_addr(relay: String) -> Result<(), String> {
    let trimmed = relay.trim().to_string();
    if !trimmed.is_empty() {
        rd_transport::parse_relay_endpoint(&trimmed).map_err(|e| format!("{e:#}"))?;
    }
    let path = agent_config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let mut value: toml::Value = if path.exists() {
        let raw =
            std::fs::read_to_string(&path).map_err(|e| format!("读取 agent.toml 失败：{e}"))?;
        toml::from_str(&raw).unwrap_or_else(|_| toml::Value::Table(Default::default()))
    } else {
        toml::Value::Table(Default::default())
    };
    if let Some(table) = value.as_table_mut() {
        if trimmed.is_empty() {
            table.remove("relay_addr");
        } else {
            table.insert("relay_addr".into(), toml::Value::String(trimmed));
        }
    }
    let body = toml::to_string_pretty(&value).map_err(|e| format!("序列化配置失败：{e}"))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("写入配置失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换配置失败：{e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// EasyTier 组网（T-043）：设置页「EasyTier」——写入 agent.toml 的 [easytier]
// 节（融合版 rd-agent 托管本机节点，重启 rd 后生效）。与 relay_addr 同模式。
// ---------------------------------------------------------------------------

/// EasyTier 设置 DTO（前端往返；secret 原文回传，本机文件本就含它）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EasyTierSettingsDto {
    pub enabled: bool,
    pub binary_path: String,
    pub cli_path: String,
    pub network_name: String,
    pub network_secret: String,
    pub peer_urls: Vec<String>,
    pub virtual_ip: String,
    pub no_tun: bool,
    pub forwards: Vec<String>,
}

impl Default for EasyTierSettingsDto {
    fn default() -> Self {
        Self {
            enabled: false,
            binary_path: String::new(),
            cli_path: String::new(),
            network_name: String::new(),
            network_secret: String::new(),
            peer_urls: Vec::new(),
            virtual_ip: String::new(),
            no_tun: true,
            forwards: Vec::new(),
        }
    }
}

/// 读 agent.toml 的 [easytier] 节（缺文件/缺节 = 默认关闭）。
pub fn load_easytier_settings() -> EasyTierSettingsDto {
    let Some(cfg) = read_agent_config() else {
        return EasyTierSettingsDto::default();
    };
    EasyTierSettingsDto {
        enabled: cfg.easytier.enabled,
        binary_path: cfg.easytier.binary_path.clone(),
        cli_path: cfg.easytier.cli_path.clone(),
        network_name: cfg.easytier.network_name.clone(),
        network_secret: cfg.easytier.network_secret.clone(),
        peer_urls: cfg.easytier.peer_urls.clone(),
        virtual_ip: cfg.easytier.virtual_ip.clone(),
        no_tun: cfg.easytier.no_tun,
        forwards: cfg.easytier.forwards.clone(),
    }
}

/// 读当前 EasyTier 设置。
#[tauri::command]
pub fn get_easytier() -> Result<EasyTierSettingsDto, String> {
    Ok(load_easytier_settings())
}

/// 写 EasyTier 设置（整体替换 [easytier] 节）；转发规则格式即时校验。
/// 重启 rd 后生效（rd-agent 托管节点在 start 时读取）。
#[tauri::command]
pub fn set_easytier(settings: EasyTierSettingsDto) -> Result<(), String> {
    if settings.enabled {
        if settings.network_name.trim().is_empty() {
            return Err("启用 EasyTier 必须填网络名".into());
        }
        if settings.binary_path.trim().is_empty() {
            return Err("启用 EasyTier 必须填 easytier-core 路径".into());
        }
        for f in &settings.forwards {
            if crate::platform::parse_forward_rule(f).is_none() {
                return Err(format!(
                    "转发规则格式错误：{f}（应为 \"对端虚拟IP:端口@本地端口\"，如 10.126.126.3:21118@21130）"
                ));
            }
        }
    }
    write_agent_config(|table| {
        if settings.enabled {
            let mut sec = toml::value::Table::new();
            sec.insert("enabled".into(), toml::Value::Boolean(true));
            sec.insert(
                "binary_path".into(),
                toml::Value::String(settings.binary_path.clone()),
            );
            sec.insert(
                "cli_path".into(),
                toml::Value::String(settings.cli_path.clone()),
            );
            sec.insert(
                "network_name".into(),
                toml::Value::String(settings.network_name.clone()),
            );
            sec.insert(
                "network_secret".into(),
                toml::Value::String(settings.network_secret.clone()),
            );
            sec.insert(
                "peer_urls".into(),
                toml::Value::Array(
                    settings
                        .peer_urls
                        .iter()
                        .map(|s| toml::Value::String(s.clone()))
                        .collect(),
                ),
            );
            sec.insert(
                "virtual_ip".into(),
                toml::Value::String(settings.virtual_ip.clone()),
            );
            sec.insert("no_tun".into(), toml::Value::Boolean(settings.no_tun));
            sec.insert(
                "forwards".into(),
                toml::Value::Array(
                    settings
                        .forwards
                        .iter()
                        .map(|s| toml::Value::String(s.clone()))
                        .collect(),
                ),
            );
            table.insert("easytier".into(), toml::Value::Table(sec));
        } else {
            table.remove("easytier");
        }
    })
}

/// EasyTier 转发入口地址（连接候选用）：[easytier] enabled 且有 forwards
/// 时返回 127.0.0.1:port 列表；未启用/读失败返回空（零影响）。
pub fn easytier_forward_addrs() -> Vec<std::net::SocketAddr> {
    let Some(cfg) = read_agent_config() else {
        return Vec::new();
    };
    if !cfg.easytier.enabled {
        return Vec::new();
    }
    crate::platform::forward_bind_addrs(&cfg.easytier.forwards)
}

/// 读 agent.toml（缺文件 = None；解析失败 = None，降级）。
fn read_agent_config() -> Option<AgentConfigSubset> {
    let path = agent_config_file();
    let raw = std::fs::read_to_string(path).ok()?;
    toml::from_str(&raw).ok()
}

/// 原子改写 agent.toml（读现有内容 → `edit` 闭包改根表 → tmp+rename 提交）。
fn write_agent_config(edit: impl FnOnce(&mut toml::value::Table)) -> Result<(), String> {
    let path = agent_config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let mut value: toml::Value = if path.exists() {
        let raw =
            std::fs::read_to_string(&path).map_err(|e| format!("读取 agent.toml 失败：{e}"))?;
        toml::from_str(&raw).unwrap_or_else(|_| toml::Value::Table(Default::default()))
    } else {
        toml::Value::Table(Default::default())
    };
    if let Some(table) = value.as_table_mut() {
        edit(table);
    }
    let body = toml::to_string_pretty(&value).map_err(|e| format!("序列化配置失败：{e}"))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("写入配置失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换配置失败：{e}"))?;
    Ok(())
}

/// fec 生效值：env 覆盖 > settings.json > 默认关（T-027 语义保留）。
pub fn media_fec_setting(data_dir: &Path) -> bool {
    if std::env::var("RD_MEDIA_FEC").as_deref() == Ok("1") {
        return true;
    }
    std::fs::read_to_string(data_dir.join("settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("media_fec").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 方案 B：EasyTier 一键组网口令（rd-et://...）+ 本机环境检测 + 在线成员实时查询
// ---------------------------------------------------------------------------

const B64_URL_TABLE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_URL_TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_URL_TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_URL_TABLE[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64_URL_TABLE[(n & 0x3f) as usize] as char);
        }
    }
    out
}

fn b64url_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut vals = Vec::with_capacity(input.len());
    for b in input.trim().trim_end_matches('=').bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err("组网口令包含非法字符".into()),
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        if chunk.len() < 2 {
            return Err("组网口令长度不完整".into());
        }
        let n0 = (chunk[0] as u32) << 18;
        let n1 = (chunk[1] as u32) << 12;
        let n2 = if chunk.len() > 2 {
            (chunk[2] as u32) << 6
        } else {
            0
        };
        let n3 = if chunk.len() > 3 { chunk[3] as u32 } else { 0 };
        let n = n0 | n1 | n2 | n3;
        out.push(((n >> 16) & 0xff) as u8);
        if chunk.len() > 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if chunk.len() > 3 {
            out.push((n & 0xff) as u8);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EasyTierInvitePayload {
    /// network_name
    n: String,
    /// network_secret
    s: String,
    /// peer_urls
    p: Vec<String>,
}

fn encode_easytier_invite(name: &str, secret: &str, peers: &[String]) -> Result<String, String> {
    let payload = EasyTierInvitePayload {
        n: name.to_string(),
        s: secret.to_string(),
        p: peers.to_vec(),
    };
    let json = serde_json::to_vec(&payload).map_err(|e| format!("编码口令失败：{e}"))?;
    Ok(format!("rd-et://{}", b64url_encode(&json)))
}

fn decode_easytier_invite(raw: &str) -> Result<EasyTierInvitePayload, String> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("rd-et://")
        .or_else(|| trimmed.strip_prefix("RD-ET://"))
        .ok_or_else(|| "口令格式错误，应以 rd-et:// 开头".to_string())?;
    let bytes = b64url_decode(body)?;
    let payload: EasyTierInvitePayload =
        serde_json::from_slice(&bytes).map_err(|e| format!("解析口令失败：{e}"))?;
    if payload.n.trim().is_empty() {
        return Err("口令缺少网络名称".into());
    }
    Ok(payload)
}

fn detect_easytier_binaries(
    configured_core: &str,
    configured_cli: &str,
) -> (Option<String>, Option<String>) {
    let core_name = if cfg!(windows) {
        "easytier-core.exe"
    } else {
        "easytier-core"
    };
    let cli_name = if cfg!(windows) {
        "easytier-cli.exe"
    } else {
        "easytier-cli"
    };

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if !configured_core.trim().is_empty() {
        candidates.push(std::path::PathBuf::from(configured_core.trim()));
    }
    if let Some(parent) = agent_config_file().parent() {
        candidates.push(parent.join("tools").join(core_name));
        candidates.push(parent.join(core_name));
    }
    #[cfg(windows)]
    {
        candidates.push(std::path::PathBuf::from(r"C:\tools\easytier-core.exe"));
        candidates.push(std::path::PathBuf::from(
            r"C:\Program Files\EasyTier\easytier-core.exe",
        ));
    }
    #[cfg(not(windows))]
    {
        candidates.push(std::path::PathBuf::from("/opt/homebrew/bin/easytier-core"));
        candidates.push(std::path::PathBuf::from("/usr/local/bin/easytier-core"));
        candidates.push(std::path::PathBuf::from("/usr/bin/easytier-core"));
    }

    let mut found_core = candidates.into_iter().find(|p| p.is_file());
    if found_core.is_none() {
        let cmd = if cfg!(windows) { "where" } else { "which" };
        if let Ok(out) = std::process::Command::new(cmd).arg(core_name).output()
            && out.status.success()
            && let Some(line) = String::from_utf8_lossy(&out.stdout).lines().next()
        {
            let p = std::path::PathBuf::from(line.trim());
            if p.is_file() {
                found_core = Some(p);
            }
        }
    }

    let mut found_cli = if !configured_cli.trim().is_empty() {
        let p = std::path::PathBuf::from(configured_cli.trim());
        p.is_file().then_some(p)
    } else {
        None
    };
    if found_cli.is_none()
        && let Some(ref core) = found_core
        && let Some(dir) = core.parent()
    {
        let sibling = dir.join(cli_name);
        if sibling.is_file() {
            found_cli = Some(sibling);
        }
    }

    (
        found_core.map(|p| p.to_string_lossy().to_string()),
        found_cli.map(|p| p.to_string_lossy().to_string()),
    )
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EasyTierPeerDto {
    pub hostname: String,
    pub ipv4: String,
    pub cost: String,
    pub latency_ms: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EasyTierStatusDto {
    pub installed: bool,
    pub binary_path: String,
    pub enabled: bool,
    pub network_name: String,
    pub virtual_ip: String,
    pub peer_urls: Vec<String>,
    pub invite_code: Option<String>,
    pub peers: Vec<EasyTierPeerDto>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EasyTierInviteResultDto {
    pub ok: bool,
    pub invite_code: String,
    pub network_name: String,
    pub peer_urls: Vec<String>,
    pub binary_found: bool,
    pub binary_path: String,
    pub hint: String,
}

/// 检测本机 EasyTier 安装状态、当前组网配置、当前口令与实时在线对端列表（10.126.x.x）。
#[tauri::command]
pub async fn easytier_detect_status() -> Result<EasyTierStatusDto, String> {
    let cfg = load_easytier_settings();
    let (core, cli) = detect_easytier_binaries(&cfg.binary_path, &cfg.cli_path);
    let invite_code = if cfg.enabled && !cfg.network_name.trim().is_empty() {
        encode_easytier_invite(&cfg.network_name, &cfg.network_secret, &cfg.peer_urls).ok()
    } else {
        None
    };

    let mut peers = Vec::new();
    let mut local_vip = cfg.virtual_ip.clone();
    if let Some(cli_bin) = cli {
        let mut cmd = tokio::process::Command::new(&cli_bin);
        cmd.args(["-o", "json", "peer"]);
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        if let Ok(Ok(out)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), cmd.output()).await
            && out.status.success()
            && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            && let Some(arr) = val.as_array()
        {
            for item in arr {
                let ipv4 = item
                    .get("ipv4")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let hostname = item
                    .get("hostname")
                    .and_then(|v| v.as_str())
                    .unwrap_or("EasyTier 设备")
                    .to_string();
                let cost = item
                    .get("cost")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let lat = item.get("lat_ms").and_then(|v| {
                    v.as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .or_else(|| v.as_f64())
                });
                if cost.eq_ignore_ascii_case("Local") && !ipv4.is_empty() {
                    local_vip = ipv4;
                } else if !ipv4.is_empty() {
                    peers.push(EasyTierPeerDto {
                        hostname,
                        ipv4,
                        cost: if cost.is_empty() { "P2P".into() } else { cost },
                        latency_ms: lat,
                    });
                }
            }
        }
    }

    Ok(EasyTierStatusDto {
        installed: core.is_some(),
        binary_path: core.unwrap_or_default(),
        enabled: cfg.enabled,
        network_name: cfg.network_name,
        virtual_ip: local_vip,
        peer_urls: cfg.peer_urls,
        invite_code,
        peers,
    })
}

/// 一键生成 EasyTier 组网口令（自动创建随机网络名+密钥，绑定最快的公共引导节点并写入本机配置）。
#[tauri::command]
pub fn easytier_generate_invite(peer_urls: Vec<String>) -> Result<EasyTierInviteResultDto, String> {
    let cleaned_peers: Vec<String> = peer_urls
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(4)
        .collect();
    let final_peers = if cleaned_peers.is_empty() {
        vec!["tcp://public.easytier.top:11010".to_string()]
    } else {
        cleaned_peers
    };

    let u1 = uuid::Uuid::new_v4().simple().to_string();
    let u2 = uuid::Uuid::new_v4().simple().to_string();
    let network_name = format!("rd-mesh-{}", &u1[..6]);
    let network_secret = u2[..16].to_string();

    let cur = load_easytier_settings();
    let (core, cli) = detect_easytier_binaries(&cur.binary_path, &cur.cli_path);
    let binary_found = core.is_some();
    let binary_path = core.unwrap_or_default();
    let cli_path = cli.unwrap_or_default();

    write_agent_config(|table| {
        let mut sec = toml::value::Table::new();
        sec.insert("enabled".into(), toml::Value::Boolean(true));
        sec.insert(
            "binary_path".into(),
            toml::Value::String(binary_path.clone()),
        );
        sec.insert("cli_path".into(), toml::Value::String(cli_path.clone()));
        sec.insert(
            "network_name".into(),
            toml::Value::String(network_name.clone()),
        );
        sec.insert(
            "network_secret".into(),
            toml::Value::String(network_secret.clone()),
        );
        sec.insert(
            "peer_urls".into(),
            toml::Value::Array(
                final_peers
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        // 留空由 EasyTier 内置 DHCP 自动分配唯一 10.126.126.x 虚拟内网 IP（避免双端冲突）
        sec.insert("virtual_ip".into(), toml::Value::String(String::new()));
        sec.insert("no_tun".into(), toml::Value::Boolean(false));
        sec.insert("forwards".into(), toml::Value::Array(Vec::new()));
        table.insert("easytier".into(), toml::Value::Table(sec));
    })?;

    let invite_code = encode_easytier_invite(&network_name, &network_secret, &final_peers)?;
    let hint = if binary_found {
        "已写入本机 EasyTier 组网配置。请复制口令发给对方粘贴加入，然后双方重启本软件即可通过 10.126.x.x 直连！".into()
    } else {
        "口令已生成并保存！本机尚未检测到 easytier-core，请先安装 easytier-core（或放进 rd/tools 目录）后重启生效。".into()
    };

    Ok(EasyTierInviteResultDto {
        ok: true,
        invite_code,
        network_name,
        peer_urls: final_peers,
        binary_found,
        binary_path,
        hint,
    })
}

/// 粘贴对方发来的 `rd-et://...` 组网口令，一键加入同一个 EasyTier 网络（自动 DHCP 分配不冲突的 10.126.x.x）。
#[tauri::command]
pub fn easytier_join_invite(invite_code: String) -> Result<EasyTierInviteResultDto, String> {
    let payload = decode_easytier_invite(&invite_code)?;
    let cur = load_easytier_settings();
    let (core, cli) = detect_easytier_binaries(&cur.binary_path, &cur.cli_path);
    let binary_found = core.is_some();
    let binary_path = core.unwrap_or_default();
    let cli_path = cli.unwrap_or_default();

    write_agent_config(|table| {
        let mut sec = toml::value::Table::new();
        sec.insert("enabled".into(), toml::Value::Boolean(true));
        sec.insert(
            "binary_path".into(),
            toml::Value::String(binary_path.clone()),
        );
        sec.insert("cli_path".into(), toml::Value::String(cli_path.clone()));
        sec.insert(
            "network_name".into(),
            toml::Value::String(payload.n.clone()),
        );
        sec.insert(
            "network_secret".into(),
            toml::Value::String(payload.s.clone()),
        );
        sec.insert(
            "peer_urls".into(),
            toml::Value::Array(
                payload
                    .p
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        sec.insert("virtual_ip".into(), toml::Value::String(String::new()));
        sec.insert("no_tun".into(), toml::Value::Boolean(false));
        sec.insert("forwards".into(), toml::Value::Array(Vec::new()));
        table.insert("easytier".into(), toml::Value::Table(sec));
    })?;

    let hint = if binary_found {
        format!(
            "已加入组网「{}」！重启本软件后将自动分配 10.126.x.x 内网 IP 并与对方打洞直连。",
            payload.n
        )
    } else {
        format!(
            "已保存组网「{}」，但本机未检测到 easytier-core，安装后重启即可上线。",
            payload.n
        )
    };

    Ok(EasyTierInviteResultDto {
        ok: true,
        invite_code: invite_code.trim().to_string(),
        network_name: payload.n,
        peer_urls: payload.p,
        binary_found,
        binary_path,
        hint,
    })
}

// ---------------------------------------------------------------------------
// 方案 A：RustDesk 风格 9 位设备 ID 码（基于本机证书指纹派生 + rd-relay 在线查址）
// ---------------------------------------------------------------------------

fn derive_numeric_id_from_fp(fp: &[u8; 32]) -> u32 {
    let raw = u64::from_le_bytes(fp[..8].try_into().expect("fp 前 8 字节"));
    100_000_000 + (raw % 900_000_000) as u32
}

fn format_9digit_id(id: u32) -> String {
    format!(
        "{:03} {:03} {:03}",
        id / 1_000_000,
        (id / 1_000) % 1_000,
        id % 1_000
    )
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayIdStatusDto {
    pub numeric_id: String,
    pub raw_id: u32,
    pub relay_addr: Option<String>,
    pub fingerprint_short: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayResolveIdResultDto {
    pub ok: bool,
    pub online: bool,
    pub numeric_id: String,
    pub rtt_ms: Option<f64>,
    pub peer_fingerprint_short: Option<String>,
    pub detail: String,
}

/// 返回本机真实派生的 9 位设备 ID 码与当前配置的中转地址。
#[tauri::command]
pub fn relay_id_status(state: State<'_, ConnManager>) -> Result<RelayIdStatusDto, String> {
    let fp = state.identity().fingerprint();
    let raw_id = derive_numeric_id_from_fp(&fp.0);
    let relay_addr = read_agent_config().and_then(|c| c.relay_addr);
    Ok(RelayIdStatusDto {
        numeric_id: format_9digit_id(raw_id),
        raw_id,
        relay_addr,
        fingerprint_short: fp.short_hex().to_string(),
    })
}

/// 向指定自建 `rd-relay` 发起真实 9 位 ID 在线查询（TYPE_LOOKUP_ID = 7），验证对方是否已注册到该中转服务器。
#[tauri::command]
pub async fn relay_resolve_id(
    relay_addr: String,
    numeric_id: String,
) -> Result<RelayResolveIdResultDto, String> {
    let digits: String = numeric_id.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 9 {
        return Ok(RelayResolveIdResultDto {
            ok: false,
            online: false,
            numeric_id: digits,
            rtt_ms: None,
            peer_fingerprint_short: None,
            detail: "请输入 9 位数字设备 ID（如 482 916 305）".into(),
        });
    }
    let id_num: u32 = digits.parse().map_err(|_| "设备 ID 格式无效".to_string())?;
    let formatted = format_9digit_id(id_num);

    let trimmed = relay_addr.trim();
    if trimmed.is_empty() {
        return Ok(RelayResolveIdResultDto {
            ok: false,
            online: false,
            numeric_id: formatted,
            rtt_ms: None,
            peer_fingerprint_short: None,
            detail: "请先选择或填写自建中转服务器地址（如 45.x.x.x:21118）".into(),
        });
    }
    let host_port = if trimmed.contains(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:21118")
    };
    let target = match crate::conn::resolve_addr(&host_port, 21118).await {
        Ok(s) => s,
        Err(e) => {
            return Ok(RelayResolveIdResultDto {
                ok: false,
                online: false,
                numeric_id: formatted,
                rtt_ms: None,
                peer_fingerprint_short: None,
                detail: format!("中转地址解析失败：{e}"),
            });
        }
    };

    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("绑定本地 UDP 失败：{e}"))?;

    let mut req = [0u8; 13];
    req[8] = 7; // TYPE_LOOKUP_ID
    req[9..13].copy_from_slice(&id_num.to_le_bytes());

    let t0 = std::time::Instant::now();
    sock.send_to(&req, target)
        .await
        .map_err(|e| format!("发送 ID 查询失败：{e}"))?;

    let mut buf = [0u8; 64];
    match tokio::time::timeout(
        std::time::Duration::from_millis(2500),
        sock.recv_from(&mut buf),
    )
    .await
    {
        Ok(Ok((n, _))) if n >= 10 && buf[8] == 8 => {
            let rtt = (t0.elapsed().as_secs_f64() * 10000.0).round() / 10.0;
            if buf[9] == 1 && n >= 42 {
                let fp_hex: String = buf[10..18].iter().map(|b| format!("{b:02x}")).collect();
                Ok(RelayResolveIdResultDto {
                    ok: true,
                    online: true,
                    numeric_id: formatted,
                    rtt_ms: Some(rtt),
                    peer_fingerprint_short: Some(fp_hex.clone()),
                    detail: format!(
                        "对方 ID 在线！中转延迟 {rtt:.1}ms · 指纹 {fp_hex}（支持 P2P 打洞 + 中转兜底）"
                    ),
                })
            } else {
                Ok(RelayResolveIdResultDto {
                    ok: true,
                    online: false,
                    numeric_id: formatted,
                    rtt_ms: Some(rtt),
                    peer_fingerprint_short: None,
                    detail: format!(
                        "中转可达（{rtt:.1}ms），但该 9 位 ID 当前未上线（请确认对方也填了同一中转地址并已启动）"
                    ),
                })
            }
        }
        Ok(Ok(_)) => Ok(RelayResolveIdResultDto {
            ok: false,
            online: false,
            numeric_id: formatted,
            rtt_ms: None,
            peer_fingerprint_short: None,
            detail: "收到非 rd-relay 应答（请确认目标运行的是最新版 rd-relay）".into(),
        }),
        Ok(Err(e)) => Ok(RelayResolveIdResultDto {
            ok: false,
            online: false,
            numeric_id: formatted,
            rtt_ms: None,
            peer_fingerprint_short: None,
            detail: format!("查询失败：{e}"),
        }),
        Err(_) => Ok(RelayResolveIdResultDto {
            ok: false,
            online: false,
            numeric_id: formatted,
            rtt_ms: None,
            peer_fingerprint_short: None,
            detail: "查询超时（>2500ms，请检查 VPS UDP 21118 放行与 rd-relay 进程）".into(),
        }),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EasyTierAutoInstallDto {
    pub ok: bool,
    pub version: String,
    pub binary_path: String,
    pub tools_dir: String,
    pub detail: String,
}

/// 一键自动从 GitHub Releases 下载并解压当前平台对应的官方 `easytier-core` 到配置目录 `tools/`。
#[tauri::command]
pub async fn easytier_auto_install() -> Result<EasyTierAutoInstallDto, String> {
    let target_prefix = if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "easytier-windows-x86_64-"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "easytier-windows-arm64-"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "easytier-macos-aarch64-"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "easytier-macos-x86_64-"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "easytier-linux-x86_64-"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "easytier-linux-aarch64-"
    } else {
        return Err("当前系统架构暂不支持自动下载，请点击链接手动下载 EasyTier".into());
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("rd-controller")
        .build()
        .map_err(|e| format!("初始化下载客户端失败：{e}"))?;

    let raw = client
        .get("https://api.github.com/repos/EasyTier/EasyTier/releases/latest")
        .send()
        .await
        .map_err(|e| format!("访问 GitHub Releases 失败（可尝试手动下载链接）：{e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub API 返回异常：{e}"))?
        .text()
        .await
        .map_err(|e| format!("读取 Release 列表失败：{e}"))?;
    let rel: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("解析 Release 列表失败：{e}"))?;

    let version = rel
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("latest")
        .to_string();

    let assets = rel
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Release 缺少资源列表".to_string())?;

    let asset = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.starts_with(target_prefix) && n.ends_with(".zip"))
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("未在 {version} 中找到匹配 {target_prefix}*.zip 的安装包"))?;

    let download_url = asset
        .get("browser_download_url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| "缺少下载地址".to_string())?;

    let tools_dir = agent_config_file()
        .parent()
        .ok_or_else(|| "无法定位配置目录".to_string())?
        .join("tools");
    std::fs::create_dir_all(&tools_dir).map_err(|e| format!("创建 tools 目录失败：{e}"))?;

    let zip_path = tools_dir.join("easytier-download.zip");
    let bytes = client
        .get(download_url)
        .send()
        .await
        .map_err(|e| format!("下载 EasyTier 失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("下载返回异常：{e}"))?
        .bytes()
        .await
        .map_err(|e| format!("读取安装包失败：{e}"))?;

    std::fs::write(&zip_path, &bytes).map_err(|e| format!("保存压缩包失败：{e}"))?;

    let extract_dir = tools_dir.join("_extract_tmp");
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir).map_err(|e| format!("创建临时目录失败：{e}"))?;

    #[cfg(windows)]
    {
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                zip_path.display(),
                extract_dir.display()
            ),
        ]);
        cmd.creation_flags(0x0800_0000);
        let out = cmd
            .output()
            .await
            .map_err(|e| format!("调用 PowerShell 解压失败：{e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("解压失败：{err}"));
        }
    }
    #[cfg(not(windows))]
    {
        let out = tokio::process::Command::new("unzip")
            .args([
                "-o",
                &zip_path.to_string_lossy(),
                "-d",
                &extract_dir.to_string_lossy(),
            ])
            .output()
            .await
            .map_err(|e| format!("调用 unzip 解压失败：{e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("解压失败：{err}"));
        }
    }

    // 递归把解压出的核心文件平铺拷贝到 tools/
    let mut stack = vec![extract_dir.clone()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Some(fname) = p.file_name() {
                    let dst = tools_dir.join(fname);
                    let _ = std::fs::copy(&p, &dst);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let s = fname.to_string_lossy();
                        if s.starts_with("easytier-") {
                            let _ = std::fs::set_permissions(
                                &dst,
                                std::fs::Permissions::from_mode(0o755),
                            );
                        }
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(&zip_path);

    let core_name = if cfg!(windows) {
        "easytier-core.exe"
    } else {
        "easytier-core"
    };
    let core_path = tools_dir.join(core_name);
    if !core_path.is_file() {
        return Err(format!("解压完成但未找到 {core_name}，请手动检查压缩包"));
    }

    Ok(EasyTierAutoInstallDto {
        ok: true,
        version: version.clone(),
        binary_path: core_path.to_string_lossy().to_string(),
        tools_dir: tools_dir.to_string_lossy().to_string(),
        detail: format!(
            "已自动下载并安装 EasyTier {version} 到 {}！现在可以直接生成/加入组网口令。",
            tools_dir.display()
        ),
    })
}

#[cfg(test)]
mod easytier_invite_tests {
    use super::*;

    #[test]
    fn easytier_invite_roundtrip() {
        let code = encode_easytier_invite(
            "rd-mesh-abc123",
            "secret9876543210",
            &["tcp://public.easytier.top:11010".into()],
        )
        .unwrap();
        assert!(code.starts_with("rd-et://"));
        let decoded = decode_easytier_invite(&code).unwrap();
        assert_eq!(decoded.n, "rd-mesh-abc123");
        assert_eq!(decoded.s, "secret9876543210");
        assert_eq!(decoded.p, vec!["tcp://public.easytier.top:11010"]);
    }
}
