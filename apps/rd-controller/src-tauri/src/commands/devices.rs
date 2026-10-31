//! 设备管理命令（T-019）：持久化身份 + 设备列表（devices.json）+ 6 位码配对 +
//! 连接/断开/探测 + 解码能力上报。
//!
//! 存储布局（`ConnManager::data_dir`，Windows `%APPDATA%\com.rd.controller`）：
//! - `identity/identity.bin`：本机身份（rd-security，DPAPI 加密）；
//! - `trusted.json`：信任指纹（rd-security `TrustStore`，信任即设备）；
//! - `devices.json`：设备地址/名称等自有信息（`[{id, name, addr, fingerprint_hex, added_at}]`）。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
}

impl From<DecodeCapsDto> for DecodeCaps {
    fn from(dto: DecodeCapsDto) -> Self {
        Self {
            h264_420: dto.h264_baseline_420,
            h264_444: dto.h264_high_444,
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
    // 握手 3s 超时（D-013 包2）：对不可达地址 QUIC 握手可拖到 ~10s，
    // 自动重连循环会被单次慢失败拖住——快速失败让退避节奏生效。
    let conn = tokio::time::timeout(
        Duration::from_secs(3),
        DeviceConn::connect(
            &id,
            &entry.addr,
            state.identity(),
            Some(fp),
            crate::commands::devices::media_fec_setting(state.data_dir()),
        ),
    )
    .await
    .map_err(|_| format!("连接 {} 超时（3s）", entry.addr))?
    .map_err(|e| describe_connect_error(&e))?;
    let ack = match conn.handshake(state.decode_caps()).await {
        Ok(ack) => ack,
        Err(e) => {
            conn.close(0, "handshake failed");
            return Err(describe_handshake_error(e));
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

/// 被控端状态快照（融合版：agent core 与控制端同进程）。
#[tauri::command]
pub fn host_status(state: tauri::State<'_, Arc<AgentState>>) -> HostStatusDto {
    use std::sync::atomic::Ordering;
    HostStatusDto {
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
    }
}

/// 暂停/恢复本机被控共享（UX 安全出口：设备页「切断」按钮；与托盘 ID_PAUSE 同语义）。
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

/// 生成 6 位配对码并返回建议地址（UI 弹卡展示；5 分钟内有效、一次性）。
#[tauri::command]
pub fn generate_pair_code(state: tauri::State<'_, Arc<AgentState>>) -> PairCodeDto {
    use std::sync::atomic::Ordering;
    let code = {
        let mut pairing = state.pairing.lock().expect("pairing 锁中毒");
        pairing.new_code()
    };
    tracing::info!("UI 生成新配对码(5 分钟内有效)");
    PairCodeDto {
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
    }
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
    state: State<'_, ConnManager>,
) -> Result<DeviceDto, String> {
    let addr = normalize_addr(addr.trim());
    if addr.is_empty() {
        return Err("设备地址不能为空".into());
    }
    let name = name.trim().to_string();
    let mut store = open_store(&state)?;
    let mut entry = store
        .by_id(&id)
        .cloned()
        .ok_or_else(|| "设备不存在".to_string())?;
    if !name.is_empty() {
        entry.name = name;
    }
    entry.addr = addr;
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
    // arboard 非线程安全句柄：每次新建（剪贴板操作低频，开销可忽略）
    tokio::task::spawn_blocking(|| {
        arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .map_err(|e| format!("读取本机剪贴板失败:{e}"))
    })
    .await
    .map_err(|e| format!("剪贴板任务失败:{e}"))?
}

/// 写控制端本机剪贴板文本（「从远端复制」的最后一步）。
#[tauri::command]
pub async fn local_clipboard_write(text: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text))
            .map_err(|e| format!("写入本机剪贴板失败:{e}"))
    })
    .await
    .map_err(|e| format!("剪贴板任务失败:{e}"))?
}

// ---------------------------------------------------------------------------
// 方案实验室（Studio 内嵌，2026-10-28 融合）：独立窗口加载本进程内嵌的
// Studio（方案测试/跑分对比），不再单独开浏览器/daemon。
// ---------------------------------------------------------------------------

/// 打开（或聚焦已打开的）方案实验室窗口。
#[tauri::command]
pub async fn open_studio(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl};
    if let Some(win) = app.get_webview_window("studio") {
        let _ = win.set_focus();
        return Ok(());
    }
    let url: tauri::Url = "http://127.0.0.1:7500"
        .parse()
        .map_err(|e| format!("实验室地址解析失败:{e}"))?;
    tauri::WebviewWindowBuilder::new(&app, "studio", WebviewUrl::External(url))
        .title("方案实验室 · Switchboard Studio")
        .inner_size(1150.0, 820.0)
        .build()
        .map(|_| ())
        .map_err(|e| format!("打开实验室窗口失败:{e}"))
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
