//! 经原生中继（[`rd_relay`]）的连接：打标 UDP 虚拟链路（T-032）。
//!
//! ## 原理
//! quinn 需要一个 UDP socket；我们把同一个 tokio UDP socket 包成
//! [`RelayUdpSocket`]（实现 `quinn::AsyncUdpSocket`）：
//! - 发：`[u64 tag][QUIC 包]` 发往中继（quinn 以为在对端地址发，目的地址被忽略）；
//! - 收：只接受来自中继、tag 匹配的数据报，剥 tag 后按 `RecvMeta.addr = 中继`
//!   交给 quinn——quinn 全程以为在和「中继地址」上的对端说 QUIC。
//!
//! QUIC 包在中继上**只被转发、永不解密**：rd 的双向证书认证、指纹钉扎、
//! 配对流程与直连完全一致。会话标号 = 对端指纹前 8 字节（rd-relay 协议）。
//!
//! NAT 保活：quinn keep-alive 5s 持续出包，中继按最新来源地址回包，NAT
//! 绑定自然维持；握手期 REG/DIAL 重试同样起保活作用。
//!
//! ## 打洞（T-034，relay 协调 + 双端探测 + 竞速）
//! DIAL ACK 尾部携带被控端在中继处的 NAT 地址（旧中继没有尾巴 → 自动退化为
//! 纯中继路径）。控制端据此与被控端互发探测包开洞，从被控端探测包的源地址
//! 学到其直连 socket 的公网映射，再**用同一个探测 socket**（同端口=同 NAT
//! 映射，兼容端口受限型 NAT）对它直接 QUIC；与中继 QUIC 路径竞速，谁先完成
//! 握手用谁。配对/指纹钉扎/E2E 加密两条路完全一致。

use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::io::Interest;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::conn::{self, Conn};
use crate::direct::DirectUdpSocket;
use crate::identity::{Fingerprint, Identity};
use rd_relay::{
    decode_control, decode_control_ack_tail, decode_dial_ack_agent_addrs, encode_control,
    encode_reg, DEFAULT_PORT, PUNCH_PROBE,
};

/// 握手超时（覆盖「被控端还没注册」的等待窗口；REG/DIAL 每 500ms 重试一次）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// 握手重发间隔。
const HANDSHAKE_RETRY: Duration = Duration::from_millis(500);
/// 打洞窗口：等被控端探测包的最长时间（内含控制端侧持续探测保活）。
#[cfg_attr(test, allow(dead_code))] // 测试用 punch_window() 缩短，常量仅生产构建引用
const PUNCH_WINDOW: Duration = Duration::from_secs(10);
/// 探测发包间隔（控制端侧，兼作 recv 轮询周期）。
const PROBE_INTERVAL: Duration = Duration::from_millis(300);
/// 被控端中继保活周期（< 家宽/运营商 NAT 30s UDP 超时，且远小于中继 60s 会话回收阈值）。
const AGENT_REG_KEEPALIVE: Duration = Duration::from_secs(15);

/// 打洞窗口（测试缩短到 800ms，免得回退路径用例等 10s）。
#[cfg(test)]
fn punch_window() -> Duration {
    Duration::from_millis(800)
}
#[cfg(not(test))]
fn punch_window() -> Duration {
    PUNCH_WINDOW
}

/// 中继地址便捷补端口（`1.2.3.4` → `1.2.3.4:21118`）。
pub fn parse_relay_addr(raw: &str) -> anyhow::Result<SocketAddr> {
    let s = raw.trim();
    let s = if s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{DEFAULT_PORT}")
    };
    s.parse()
        .map_err(|e| anyhow::anyhow!("中继地址格式错误（示例 1.2.3.4:21118）: {e}"))
}

/// 打洞总开关（`RD_PUNCH=0` 关闭；默认开）——专业定制的总闸。
pub fn punch_enabled() -> bool {
    std::env::var("RD_PUNCH").as_deref() != Ok("0")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// agent：REG 注册（我是指纹为 X 的被控端）
    Agent,
    /// controller：DIAL（我要找指纹为 X 的被控端）
    Controller,
}

fn is_public_unicast_v4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    let is_rfc1918 =
        o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168);
    let is_cgnat = o[0] == 100 && (64..=127).contains(&o[1]);
    let is_benchmark_tun = o[0] == 198 && (o[1] == 18 || o[1] == 19);
    !v4.is_loopback()
        && !v4.is_private()
        && !is_rfc1918
        && !is_cgnat
        && !is_benchmark_tun
        && !v4.is_link_local()
        && !v4.is_broadcast()
        && !v4.is_multicast()
        && !v4.is_unspecified()
}

/// 若本机开启了 Clash / Mihomo 等虚拟 TUN 网卡（`198.18.0.0/15` 或 `tun`/`meta`/`ninja`），
/// 且目标为公网 IPv4（如海外自建中继），返回本机物理网卡 IPv4 列表用于直连绕行——
/// 避免 `0.0.0.0:0` UDP 被代理节点劫持导致 `REG_ACK` 丢包、RTT 翻倍及 P2P 映射地址变成机场出口 IP。
fn preferred_physical_ipv4s_when_tun_active(remote: SocketAddr) -> Vec<std::net::Ipv4Addr> {
    let SocketAddr::V4(dst) = remote else {
        return Vec::new();
    };
    if !is_public_unicast_v4(*dst.ip()) {
        return Vec::new();
    }
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };

    let has_proxy_tun = ifaces.iter().any(|iface| {
        if iface.is_loopback() {
            return false;
        }
        if let std::net::IpAddr::V4(v4) = iface.ip() {
            let o = v4.octets();
            if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
                return true;
            }
        }
        let n = iface.name.to_ascii_lowercase();
        ["ninja", "meta", "clash", "mihomo", "sing-box", "tun"]
            .iter()
            .any(|kw| n.contains(kw))
    });
    if !has_proxy_tun {
        return Vec::new();
    }

    let mut primary = Vec::new();
    let mut secondary = Vec::new();
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        let std::net::IpAddr::V4(ip) = iface.ip() else {
            continue;
        };
        let o = ip.octets();
        if (o[0] == 169 && o[1] == 254)
            || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
            || (o[0] == 100 && (64..=127).contains(&o[1]))
        {
            continue;
        }
        let n = iface.name.to_ascii_lowercase();
        let is_virtual = [
            "tun",
            "tap",
            "meta",
            "ninja",
            "clash",
            "mihomo",
            "tailscale",
            "wsl",
            "vethernet",
            "hyper-v",
            "vmware",
            "vbox",
            "virtual",
            "docker",
            "loopback",
        ]
        .iter()
        .any(|kw| n.contains(kw));
        if is_virtual {
            continue;
        }
        if (o[0] == 192 && o[1] == 168) || o[0] == 10 {
            if !primary.contains(&ip) {
                primary.push(ip);
            }
        } else if !secondary.contains(&ip) {
            secondary.push(ip);
        }
    }
    primary.extend(secondary);
    primary
}

/// 绑定面向公网目标（中继或打洞对端）的 UDP socket：若本机存在代理虚拟 TUN 则优先绑定物理网卡 IPv4。
pub async fn bind_udp_bypassing_tun(remote: SocketAddr) -> std::io::Result<UdpSocket> {
    for ip in preferred_physical_ipv4s_when_tun_active(remote) {
        if let Ok(sock) = UdpSocket::bind(SocketAddr::from((ip, 0))).await {
            return Ok(sock);
        }
    }
    let fallback: SocketAddr = if remote.is_ipv6() {
        "[::]:0".parse().expect("ipv6 unspecified")
    } else {
        "0.0.0.0:0".parse().expect("ipv4 unspecified")
    };
    UdpSocket::bind(fallback).await
}

fn build_handshake_msgs(
    fp: &Fingerprint,
    id: &Identity,
    role: Role,
    candidates: &[SocketAddr],
) -> anyhow::Result<Vec<Vec<u8>>> {
    match role {
        Role::Agent => {
            use ed25519_dalek::pkcs8::DecodePrivateKey;
            use ed25519_dalek::Signer;
            let signing = ed25519_dalek::SigningKey::from_pkcs8_der(&id.key_der)
                .map_err(|e| anyhow::anyhow!("解析本机身份私钥失败: {e}"))?;
            let sig = signing
                .sign(&rd_relay::signed_message(rd_relay::TYPE_REG2, &fp.0))
                .to_bytes();
            Ok(vec![
                encode_reg(&fp.0, candidates),
                rd_relay::encode_signed_control(
                    rd_relay::TYPE_REG2,
                    &fp.0,
                    &id.cert_der,
                    &sig,
                    candidates,
                ),
            ])
        }
        Role::Controller => Ok(vec![encode_control(rd_relay::TYPE_DIAL, &fp.0)]),
    }
}

/// 在 `sock` 上完成 REG/DIAL 握手，返回 (会话 tag, 地址列表)。
/// 列表仅 Controller 角色且中继支持打洞协调时非空：`[中继处映射地址,
/// 直连候选...]`。`candidates` 仅 Agent 角色生效（随 REG 上报直连候选）。
/// 带重试与 NAT 保活语义。
///
/// 每周期 REG 同时发 **legacy + 签名** 两份（安全审计 F4）：旧中继只认
/// legacy（签名报文被当未知类型静默丢弃）、新中继认签名（legacy 在已建立
/// 签名会话后被忽略）——应答格式一致，握手侧无需区分被哪种受理。DIAL 只发
/// legacy（签名 DIAL 语义不成立，见 rd-relay 模块注释「残余风险」）。
#[cfg(test)]
async fn relay_handshake(
    sock: &UdpSocket,
    relay: SocketAddr,
    fp: &Fingerprint,
    id: &Identity,
    role: Role,
    candidates: &[SocketAddr],
    timeout: Duration,
) -> anyhow::Result<(u64, Vec<SocketAddr>)> {
    let msgs = build_handshake_msgs(fp, id, role, candidates)?;
    relay_handshake_with_msgs(sock, relay, fp, role, &msgs, timeout).await
}

async fn relay_handshake_with_msgs(
    sock: &UdpSocket,
    relay: SocketAddr,
    fp: &Fingerprint,
    role: Role,
    msgs: &[Vec<u8>],
    timeout: Duration,
) -> anyhow::Result<(u64, Vec<SocketAddr>)> {
    let ack_kind = match role {
        Role::Agent => rd_relay::TYPE_REG_ACK,
        Role::Controller => rd_relay::TYPE_DIAL_ACK,
    };
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(match role {
                Role::Agent => anyhow::anyhow!(
                    "连接中继失败（15s）：请检查中继地址/端口，以及 VPS 安全组是否放行 UDP"
                ),
                Role::Controller => anyhow::anyhow!(
                    "被控端不在线或中继不可达（15s）：确认被控端已配置同一中继并重启，\
                     以及 VPS 安全组 UDP 放行"
                ),
            });
        }
        for msg in msgs {
            sock.send_to(msg, relay)
                .await
                .map_err(|e| anyhow::anyhow!("发送握手包失败: {e}"))?;
        }
        // 等应答最多一个重发周期；缓冲需容下 DIAL ACK 全尾巴（50B + 4×19B 地址）
        let mut buf = [0u8; 256];
        loop {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(HANDSHAKE_RETRY);
            if wait.is_zero() {
                break; // 该重发了
            }
            let recv = tokio::time::timeout(wait, sock.recv_from(&mut buf)).await;
            let Ok(Ok((n, from))) = recv else {
                break; // 超时/网络错：重发
            };
            if from != relay {
                continue; // 只信中继
            }
            let Some((kind, ack_fp)) = decode_control(&buf[..n]) else {
                continue;
            };
            if kind != ack_kind || ack_fp != fp.0 {
                continue;
            }
            let Some((status, tag)) = decode_control_ack_tail(&buf[..n]) else {
                continue;
            };
            match status {
                rd_relay::STATUS_OK => {
                    // 打洞协调（T-034/T-035）：DIAL ACK 尾巴的地址列表
                    // `[中继处映射地址, 直连候选...]`（旧中继无尾巴 → 空）
                    let agent_addrs = match role {
                        Role::Controller => decode_dial_ack_agent_addrs(&buf[..n]),
                        Role::Agent => Vec::new(),
                    };
                    return Ok((tag, agent_addrs));
                }
                // 被控端还没注册上：继续等（重发 DIAL）
                rd_relay::STATUS_NOT_FOUND => break,
                rd_relay::STATUS_CONFLICT => anyhow::bail!(
                    "中继拒绝：会话标号冲突（极小概率的指纹前缀碰撞）。\
                     删除 %LOCALAPPDATA%\\rd 重新生成身份后重配对即可"
                ),
                other => anyhow::bail!("中继未知应答状态 {other}"),
            }
        }
    }
}

/// 绑定 UDP 并完成中继握手：若检测到本机虚拟 TUN 网卡，优先绑定物理网卡 IPv4 直连，失败再自动回退 `0.0.0.0:0`。
async fn bind_and_relay_handshake(
    relay: SocketAddr,
    fp: &Fingerprint,
    id: &Identity,
    role: Role,
    candidates: &[SocketAddr],
    timeout: Duration,
) -> anyhow::Result<(UdpSocket, u64, Vec<SocketAddr>, Vec<Vec<u8>>)> {
    let msgs = build_handshake_msgs(fp, id, role, candidates)?;
    let started = Instant::now();
    for ip in preferred_physical_ipv4s_when_tun_active(relay) {
        if let Ok(sock) = UdpSocket::bind(SocketAddr::from((ip, 0))).await {
            let fast_try = Duration::from_millis(3200).min(timeout);
            match relay_handshake_with_msgs(&sock, relay, fp, role, &msgs, fast_try).await {
                Ok((tag, addrs)) => {
                    tracing::info!(
                        local_ip = %ip,
                        %relay,
                        "检测到虚拟 TUN 网卡，中继 UDP 已自动绑定物理网卡直连"
                    );
                    return Ok((sock, tag, addrs, msgs));
                }
                Err(e) => {
                    tracing::debug!(local_ip = %ip, %relay, err = %e, "物理网卡中继握手未通，回退系统默认路由");
                }
            }
        }
    }
    let fallback_bind: SocketAddr = if relay.is_ipv6() {
        "[::]:0".parse().expect("ipv6 unspecified")
    } else {
        "0.0.0.0:0".parse().expect("ipv4 unspecified")
    };
    let sock = UdpSocket::bind(fallback_bind).await?;
    let remain = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_secs(8));
    let (tag, addrs) = relay_handshake_with_msgs(&sock, relay, fp, role, &msgs, remain).await?;
    Ok((sock, tag, addrs, msgs))
}

/// quinn 的 UDP socket 包装：中继打标链路（原理见模块注释）。
///
/// 收包经内部**泵任务**路由（T-034）：控制报（tag=0，来自中继）→ 打洞事件
/// 通道；本会话数据报（tag 匹配）→ 数据通道供 [`AsyncUdpSocket::poll_recv`]。
/// 泵任务在数据通道关闭（quinn endpoint drop）后自然退出。
#[derive(Debug)]
struct RelayUdpSocket {
    inner: Arc<tokio::net::UdpSocket>,
    tag: u64,
    relay: SocketAddr,
    /// 泵路由出的本会话数据报（含 8 字节 tag 头，poll_recv 就地剥头）。
    data_rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// 持有保活停止信号：socket 随 Server/Conn 释放时自动停止后台 15s REG 保活任务。
    _keepalive_stop: tokio::sync::watch::Sender<bool>,
}

impl RelayUdpSocket {
    /// 组装 socket 并启动收包泵（及可选的被控端周期 REG 保活）。
    /// `punch_tx` 为 Some 时（agent 侧），来自中继的 PUNCH_NOTIFY 携带的控制端地址被路由到该通道。
    fn new(
        inner: Arc<tokio::net::UdpSocket>,
        tag: u64,
        relay: SocketAddr,
        punch_tx: Option<mpsc::UnboundedSender<SocketAddr>>,
        keepalive_msgs: Option<Vec<Vec<u8>>>,
    ) -> Self {
        let (data_tx, data_rx) = mpsc::unbounded_channel();
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);

        if let Some(msgs) = keepalive_msgs {
            let weak_sock = Arc::downgrade(&inner);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(AGENT_REG_KEEPALIVE) => {}
                        _ = stop_rx.changed() => break,
                    }
                    let Some(sock) = weak_sock.upgrade() else {
                        break;
                    };
                    for msg in &msgs {
                        let _ = sock.send_to(msg, relay).await;
                    }
                }
            });
        }

        {
            let inner = Arc::clone(&inner);
            let events = punch_tx;
            tokio::spawn(async move {
                let mut buf = vec![0u8; rd_relay::MAX_DATAGRAM + 64];
                loop {
                    let (n, from) = match inner.recv_from(&mut buf).await {
                        Ok(x) => x,
                        // Windows Winsock UDP 默认开启 SIO_UDP_CONNRESET：收到瞬时 ICMP 不可达会报 10054，
                        // 必须忽略继续收包，否则会导致被控端中继收包泵永久退出失联。
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                        Err(_) => break, // socket 关闭 → 泵退出
                    };
                    if n < 8 {
                        continue;
                    }
                    let pkt_tag = u64::from_le_bytes(buf[..8].try_into().expect("8 字节"));
                    if from != relay {
                        // 非中继来源：只可能是对端打洞探测包（T-034）——源地址
                        // 就是对端探测 socket 的公网映射，交给打洞响应者回探测。
                        // 其余非中继包一律丢弃（防伪造）。
                        if &buf[..n] == PUNCH_PROBE {
                            if let Some(tx) = &events {
                                let _ = tx.send(from);
                            }
                        }
                        continue;
                    }
                    if pkt_tag == rd_relay::TAG_CONTROL {
                        // 控制报：只关心 PUNCH_NOTIFY（打洞协调），其余丢弃
                        if let Some(tx) = &events {
                            if let Some((_, initiator)) = rd_relay::decode_punch_notify(&buf[..n]) {
                                let _ = tx.send(initiator);
                            }
                        }
                        continue;
                    }
                    if pkt_tag != tag {
                        continue; // 中继转发的其他会话：不归本端点
                    }
                    // 数据通道关闭（接收端 drop）→ 泵退出、释放 socket Arc
                    if data_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            inner,
            tag,
            relay,
            data_rx: Mutex::new(data_rx),
            _keepalive_stop: stop_tx,
        }
    }

    /// 组装 `[tag][quic 包]`。
    fn frame(&self, contents: &[u8]) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(8 + contents.len());
        pkt.extend_from_slice(&self.tag.to_le_bytes());
        pkt.extend_from_slice(contents);
        pkt
    }
}

/// 写就绪 poller：tokio socket 自身管理读注册，这里只负责写 readiness。
#[derive(Debug)]
struct RelayPoller {
    inner: Arc<tokio::net::UdpSocket>,
}

impl UdpPoller for RelayPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().inner.poll_send_ready(cx)
    }
}

impl AsyncUdpSocket for RelayUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(RelayPoller {
            inner: Arc::clone(&self.inner),
        })
    }

    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        // 目的地址被忽略：恒发中继（见模块注释）
        let pkt = self.frame(transmit.contents);
        self.inner.try_io(Interest::WRITABLE, || {
            self.inner.try_send_to(&pkt, self.relay).map(|_| ())
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        let mut got = 0usize;
        let mut rx = self.data_rx.lock().expect("data_rx 锁中毒");
        while got < bufs.len() {
            let Some(pkt) = (match rx.poll_recv(cx) {
                Poll::Ready(Some(pkt)) => Some(pkt),
                Poll::Ready(None) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "中继收包泵已退出",
                    )))
                }
                Poll::Pending => None,
            }) else {
                break;
            };
            let n = pkt.len();
            if n <= 8 {
                continue;
            }
            let payload_len = n - 8;
            bufs[got][..payload_len].copy_from_slice(&pkt[8..]);
            meta[got] = RecvMeta {
                // quinn 眼中的对端地址恒为中继——连接身份稳定，NAT 换口无感
                addr: self.relay,
                dst_ip: None,
                ecn: None,
                len: payload_len,
                stride: payload_len,
            };
            got += 1;
        }
        if got > 0 {
            Poll::Ready(Ok(got))
        } else {
            Poll::Pending
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn may_fragment(&self) -> bool {
        false // MTU 已钳 1200 + 8B tag，不会 IP 分片
    }
}

/// 竞速结果：命中路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnPath {
    /// 经中继打标转发（打洞失败/不可用的兜底）。
    Relay,
    /// 直连命中（反射打洞或候选地址直连，T-034/T-035）。
    Direct,
}

/// 控制端拨号：经中继连接指纹为 `target_fp` 的被控端（E2E 加密与直连一致）。
pub async fn connect_via_relay(
    relay: SocketAddr,
    target_fp: Fingerprint,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    connect_via_relay_punch(relay, target_fp, id, expected)
        .await
        .map(|(_, conn)| conn)
}

/// 控制端拨号（打洞竞速版，T-034/T-035）：DIAL 拿到被控端地址列表后，
/// 中继 QUIC 与全部直连路（反射打洞 + 各候选直连）**竞速**，谁先完成握手
/// 用谁；输者直接丢弃。列表为空（旧版 relay）时自动退化为纯中继。
pub async fn connect_via_relay_punch(
    relay: SocketAddr,
    target_fp: Fingerprint,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<(ConnPath, Conn)> {
    let (sock, tag, agent_addrs, _) = bind_and_relay_handshake(
        relay,
        &target_fp,
        id,
        Role::Controller,
        &[],
        HANDSHAKE_TIMEOUT,
    )
    .await?;
    let direct_addrs = if punch_enabled() {
        agent_addrs
    } else {
        Vec::new()
    };
    tracing::info!(
        %relay,
        tag,
        candidates = direct_addrs.len(),
        "中继握手完成（DIAL），开始多路竞速"
    );

    // 路 A：中继打标转发（sock 已完成握手，直接交给虚拟 socket）
    let relay_fut = connect_relay_session(sock, tag, relay, id, expected);
    // 路 B：全部直连路竞速（反射打洞 + 候选直连；空列表或 RD_PUNCH=0 → 立即走纯中继）
    let direct_fut = connect_direct_racing(relay, direct_addrs, id, expected);

    tokio::pin!(relay_fut, direct_fut);
    let mut relay_err = None;
    let mut direct_err = None;
    let conn = loop {
        tokio::select! {
            r = &mut relay_fut, if relay_err.is_none() => match r {
                Ok(conn) => break (ConnPath::Relay, conn),
                Err(e) => relay_err = Some(e),
            },
            p = &mut direct_fut, if direct_err.is_none() => match p {
                Ok(conn) => break (ConnPath::Direct, conn),
                Err(e) => {
                    tracing::info!(err = %e, "直连各路未成，等待中继路径");
                    direct_err = Some(e);
                }
            },
            else => {
                // 两条路都失败：中继错误优先返回（诊断价值更高）
                let e = relay_err
                    .take()
                    .or(direct_err.take())
                    .expect("loop 退出必有错误");
                return Err(e);
            }
        }
    };
    match conn.0 {
        ConnPath::Relay => tracing::info!("竞速结果：走中继"),
        ConnPath::Direct => tracing::info!("竞速结果：直连命中"),
    }
    Ok(conn)
}

/// 中继会话 QUIC：已握手的 sock 包成虚拟 socket 后连接（路 A）。
/// QUIC 的「远端地址」用中继地址占位：虚拟 socket 忽略目的地址、恒发中继。
async fn connect_relay_session(
    sock: UdpSocket,
    tag: u64,
    relay: SocketAddr,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    let socket = RelayUdpSocket::new(Arc::new(sock), tag, relay, None, None);
    conn::connect_over(socket, relay, id, expected).await
}

/// 全部直连路竞速（路 B，T-035）：`candidates[0]`（中继处映射地址）走反射
/// 打洞（互发探测学映射），其余候选直接对它发起 QUIC（候选 = 本机网卡
/// 地址/UPnP 外部映射等已可达入口，无需探测）。任一路先完成即胜出。
async fn connect_direct_racing(
    relay: SocketAddr,
    candidates: Vec<SocketAddr>,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    if candidates.is_empty() {
        anyhow::bail!("中继不支持打洞协调（旧版），走纯中继");
    }
    // 各路并发成独立任务，结果经通道汇流；首个 Ok 胜出，其余随任务结束丢弃
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut dials: Vec<(&str, SocketAddr)> = vec![("punch-reflex", candidates[0])];
    dials.extend(candidates.iter().skip(1).map(|c| ("candidate", *c)));
    let mut pending = 0usize;
    for (kind, addr) in dials {
        let tx = tx.clone();
        let id = id.clone();
        pending += 1;
        tokio::spawn(async move {
            let r = match kind {
                "punch-reflex" => punch_direct_connect(relay, addr, &id, expected).await,
                _ => {
                    tracing::info!(%addr, "候选直连：直接发起 QUIC");
                    connect_candidate(addr, &id, expected).await
                }
            };
            let _ = tx.send(r);
        });
    }
    drop(tx);
    let mut last_err = None;
    while pending > 0 {
        match rx.recv().await {
            Some(Ok(conn)) => return Ok(conn),
            Some(Err(e)) => {
                tracing::debug!(err = %e, "一条直连路失败");
                last_err = Some(e);
                pending -= 1;
            }
            None => break,
        }
    }
    Err(last_err.expect("至少一条直连路"))
}

/// 候选地址直连：对「已可达入口」（IPv6 全局地址、UPnP 映射、同网段地址等）
/// 直接发起 QUIC，窗口 [`PUNCH_WINDOW`] 内握手不成即放弃。
async fn connect_candidate(
    cand: SocketAddr,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    let bind_any: SocketAddr = if cand.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let sock = crate::direct::bind_udp_socket(bind_any).await?;
    let fut = conn::connect_over(DirectUdpSocket::new(sock), cand, id, expected);
    tokio::time::timeout(punch_window(), fut)
        .await
        .map_err(|_| anyhow::anyhow!("候选直连超时（{cand}）"))?
}

/// 反射打洞：与被控端互发探测包开洞 → 从被控端探测包源地址学到其
/// 直连 socket 的公网映射 → 用同一个 socket（同端口=同 NAT 映射，兼容端口
/// 受限型 NAT）对它发起 QUIC。窗口 [`PUNCH_WINDOW`] 内学不到映射即失败。
async fn punch_direct_connect(
    relay: SocketAddr,
    agent_relay_addr: SocketAddr,
    id: &Identity,
    expected: Option<Fingerprint>,
) -> anyhow::Result<Conn> {
    let sock = Arc::new(bind_udp_bypassing_tun(agent_relay_addr).await?);
    // 持续向被控端「在中继处的地址」发探测：目的只是让本侧 NAT 对被控端 IP
    // 开洞（地址受限型 NAT 只看对端 IP），使被控端发来的探测包能进来。
    // 被控端不回也无所谓——洞已开，它发的包照进。
    let deadline = Instant::now() + punch_window();
    let peer;
    {
        let mut buf = [0u8; 64];
        loop {
            if Instant::now() >= deadline {
                anyhow::bail!("打洞窗口超时（{:?} 内未收到被控端探测包）", punch_window());
            }
            let _ = sock.send_to(PUNCH_PROBE, agent_relay_addr).await;
            match tokio::time::timeout(PROBE_INTERVAL, sock.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) if from != relay && &buf[..n] == PUNCH_PROBE => {
                    peer = from;
                    break;
                }
                _ => continue,
            }
        }
    }
    tracing::info!(%peer, %agent_relay_addr, "打洞：学到被控端直连映射，QUIC 直连");
    conn::connect_over(DirectUdpSocket::new(sock), peer, id, expected).await
}

/// 被控端的中继监听句柄：QUIC [`Server`](crate::Server) + 打洞请求通道。
pub struct RelayServer {
    pub server: crate::conn::Server,
    /// 收到即「某控制端请求打洞」，值为该控制端在中继处的地址。
    pub punch_req: mpsc::UnboundedReceiver<SocketAddr>,
}

/// 被控端监听（完整版）：向中继注册（REG，附带 `candidates` 直连候选供
/// DIAL ACK 下发），后续连接经中继进来；同时拿到打洞请求通道——
/// [`punch_respond`] 消费它向控制端发探测包开洞。
pub async fn bind_via_relay_ex(
    relay: SocketAddr,
    id: &Identity,
    candidates: Vec<SocketAddr>,
) -> anyhow::Result<RelayServer> {
    let local_fp = id.fingerprint();
    let (sock, tag, _, reg_msgs) = bind_and_relay_handshake(
        relay,
        &local_fp,
        id,
        Role::Agent,
        &candidates,
        HANDSHAKE_TIMEOUT,
    )
    .await?;
    tracing::info!(
        %relay,
        tag,
        candidates = candidates.len(),
        "中继握手完成（REG），QUIC 监听改走中继（已启动 15s 周期保活）"
    );
    let (punch_tx, punch_req) = mpsc::unbounded_channel();
    let socket = RelayUdpSocket::new(Arc::new(sock), tag, relay, Some(punch_tx), Some(reg_msgs));
    let server = crate::conn::Server::bind_over(socket, id)?;
    Ok(RelayServer { server, punch_req })
}

/// 被控端监听（兼容版）：只要 QUIC Server，不要打洞通道与候选上报。
pub async fn bind_via_relay(
    relay: SocketAddr,
    id: &Identity,
) -> anyhow::Result<crate::conn::Server> {
    Ok(bind_via_relay_ex(relay, id, Vec::new()).await?.server)
}

/// 被控端打洞响应：通道里的每个地址都从所有直连 QUIC socket（与 accept 共
/// 端口——控制端学到的就是这个映射）回发探测包开洞。地址有两个来源：
/// PUNCH_NOTIFY（控制端中继握手地址，尽力而为）与**探测包源地址**（对端
/// 探测 socket 的真实映射，主路径——控制端监听的正是这个 socket）。
/// 急发 6×500ms（打洞窗口内高命中）后转慢相 20s×1/2s（覆盖控制端稍后才
/// 竞速的重连场景）。同一地址只响应一轮（重复通知不重复起任务）。
pub async fn punch_respond(
    mut rx: mpsc::UnboundedReceiver<SocketAddr>,
    socks: Vec<Arc<UdpSocket>>,
) {
    let mut last: Option<SocketAddr> = None;
    while let Some(ctrl_addr) = rx.recv().await {
        if last == Some(ctrl_addr) {
            continue;
        }
        last = Some(ctrl_addr);
        tracing::info!(%ctrl_addr, "收到打洞请求，开始向控制端发探测包");
        let socks = socks.clone();
        tokio::spawn(async move {
            let burst = async {
                for _ in 0..6 {
                    for s in &socks {
                        let _ = s.send_to(PUNCH_PROBE, ctrl_addr).await;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            };
            let slow = async {
                for _ in 0..10 {
                    for s in &socks {
                        let _ = s.send_to(PUNCH_PROBE, ctrl_addr).await;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            };
            tokio::join!(burst, slow);
        });
    }
}

// ─────────────────────────────────────────────────────────────
// e2e：真实 relay + 真实 QUIC 双向认证 + 控制流回环
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use rd_proto::v0::{envelope, Hello};
    use rd_relay::spawn as relay_spawn;

    /// quinn 视角日志（握手挂起排查用）：RUST_LOG=quinn=debug,quinn_proto=debug
    static LOG_INIT: std::sync::Once = std::sync::Once::new();
    fn init_log() {
        LOG_INIT.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                        tracing_subscriber::EnvFilter::new("quinn=debug,quinn_proto=debug")
                    }),
                )
                .with_writer(std::io::stderr)
                .try_init();
        });
    }

    /// 经中继的完整链路：REG/DIAL 握手 → QUIC 双向认证（指纹钉扎）→
    /// 控制流 Envelope 往返。relay 是真实 rd_relay 实例（127.0.0.1 随机端口）。
    ///
    /// #[ignore]（2026-11-02）：非确定性竞态——多数运行通过（分层实验
    /// layered_relay_udp_raw_quinn 确定性通过），但偶发「client Initial 经
    /// 中继送达 agent 后，server Initial 不再发出」的挂起。已排除：中继转发
    /// （探针证明双向全通）、抽象 socket 用法（minimal_abstract_quinn 通过）、
    /// endpoint 生命周期（钉住实验）。疑似 quinn 驱动唤醒竞态，待专项排查。
    /// 手动复现：cargo test -p rd-transport --lib 经中继 -- --test-threads 1
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "非确定性握手竞态待专项排查（证据见上方注释）"]
    async fn 经中继的完整握手与控制流回环() -> anyhow::Result<()> {
        init_log();
        let relay_addr = relay_spawn("127.0.0.1:0".parse()?).await?;
        let server_id = Identity::generate("relay-e2e-server")?;
        let client_id = Identity::generate("relay-e2e-client")?;
        let server_fp = server_id.fingerprint();
        let client_fp = client_id.fingerprint();

        // 被控端：经中继监听，accept 后回 HelloAck
        let server = bind_via_relay(relay_addr, &server_id).await?;
        let accepter = tokio::spawn(async move {
            let conn = server.accept().await?;
            assert_eq!(conn.peer_fingerprint(), client_fp);
            let (mut tx, mut rx) = conn.accept_control().await?;
            let Some(env) = rx.recv().await? else {
                anyhow::bail!("连接已关闭");
            };
            let Some(envelope::Msg::Hello(h)) = env.msg else {
                anyhow::bail!("期望 Hello");
            };
            // 注意：任务结束时 rd Conn 被 drop → quinn implicit_close 整条连接
            // （ConnectionRef::drop 语义）。因此【应答必须在任务内完成回读验证】
            // 由客户端侧负责读，本任务只发。
            tx.send(&rd_proto::v0::Envelope {
                req_id: 1,
                msg: Some(envelope::Msg::HelloAck(rd_proto::v0::HelloAck {
                    proto_ver: 0,
                    host_name: format!("echo-{}", h.device_name),
                    ..Default::default()
                })),
            })
            .await?;
            // 保活：连接生命周期 = 本任务生命周期。任务结束会 drop rd Conn
            // → quinn implicit_close(0,"") 关闭整条连接（ConnectionRef::drop
            // 语义）——曾把客户端的响应读掐死，故必须撑到客户端读毕关闭。
            loop {
                match rx.recv().await {
                    Ok(Some(_)) => continue,
                    Ok(None) => break,
                    Err(_) => break, // 客户端隐式关连接（预期收尾）
                }
            }
            anyhow::Ok(())
        });

        // 控制端：经中继拨号 + 指纹钉扎 + Envelope 往返
        let conn = connect_via_relay(relay_addr, server_fp, &client_id, Some(server_fp)).await?;
        assert_eq!(conn.peer_fingerprint(), server_fp, "E2E 指纹必须钉扎成功");
        let (mut tx, mut rx) = conn.open_control().await?;
        tx.send(&rd_proto::v0::Envelope {
            req_id: 1,
            msg: Some(envelope::Msg::Hello(Hello {
                proto_ver: 0,
                device_name: "relay-e2e".into(),
                ..Default::default()
            })),
        })
        .await?;
        let Some(env) = rx.recv().await? else {
            anyhow::bail!("连接已关闭");
        };
        let Some(envelope::Msg::HelloAck(ack)) = env.msg else {
            anyhow::bail!("期望 HelloAck");
        };
        assert_eq!(ack.host_name, "echo-relay-e2e");
        // 读毕响应 → 主动关闭 → 服务端保活循环自然收尾
        conn.close(0, "e2e done");
        accepter.await??;
        Ok(())
    }

    /// 打洞全链路（确定性，不经竞速）：REG → DIAL（ACK 带地址尾巴）→
    /// 控制端向被控端发探测 → 被控端泵从探测包源地址学到控制端探测 socket →
    /// punch_respond 从直连 socket 回探测 → 控制端学到映射并对它 QUIC 直连
    /// （指纹钉扎断言）。
    #[tokio::test(flavor = "multi_thread")]
    async fn 打洞_直连路径_学到映射并握手() -> anyhow::Result<()> {
        let relay_addr = relay_spawn("127.0.0.1:0".parse()?).await?;
        let server_id = Identity::generate("punch-server")?;
        let client_id = Identity::generate("punch-client")?;
        let server_fp = server_id.fingerprint();
        let client_fp = client_id.fingerprint();

        // 被控端：直连 QUIC server（探测与 accept 共端口）+ 中继注册 + 真实响应者
        let direct_sock = crate::direct::bind_udp_socket("127.0.0.1:0".parse().unwrap()).await?;
        let direct_server = crate::Server::bind_socket(Arc::clone(&direct_sock), &server_id)?;
        let relay_server = bind_via_relay_ex(relay_addr, &server_id, Vec::new()).await?;
        tokio::spawn(punch_respond(
            relay_server.punch_req,
            vec![Arc::clone(&direct_sock)],
        ));

        // 控制端：完成中继握手，拿到被控端在中继处的地址
        let ctrl_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let (_, agent_addrs) = relay_handshake(
            &ctrl_sock,
            relay_addr,
            &server_fp,
            &client_id,
            Role::Controller,
            &[],
            HANDSHAKE_TIMEOUT,
        )
        .await?;
        let agent_addr = agent_addrs
            .first()
            .copied()
            .expect("新版中继 DIAL ACK 应带地址尾巴");

        // 被控端 accept（指纹钉扎断言）
        let accepter = tokio::spawn(async move {
            let conn = direct_server.accept().await?;
            assert_eq!(conn.peer_fingerprint(), client_fp, "E2E 指纹必须钉扎成功");
            anyhow::Ok(())
        });

        // 控制端：打洞直连（路 B 单独驱动，不经竞速 → 结果确定）
        let conn =
            punch_direct_connect(relay_addr, agent_addr, &client_id, Some(server_fp)).await?;
        assert_eq!(conn.peer_fingerprint(), server_fp, "E2E 指纹必须钉扎成功");
        accepter.await??;
        conn.close(0, "punch e2e done");
        Ok(())
    }

    /// 候选直连竞速（T-035）：agent 经 REG 上报直连候选（一个可达 QUIC
    /// server + 一个死地址），不响应打洞请求（反射路超时）→ 竞速命中候选。
    #[tokio::test(flavor = "multi_thread")]
    async fn 候选直连_竞速命中可达候选() -> anyhow::Result<()> {
        let relay_addr = relay_spawn("127.0.0.1:0".parse()?).await?;
        let server_id = Identity::generate("cand-server")?;
        let client_id = Identity::generate("cand-client")?;
        let server_fp = server_id.fingerprint();
        let client_fp = client_id.fingerprint();

        // 被控端：可达候选 = 独立的 QUIC server（模拟 IPv6/UPnP 入口）；
        // 死地址 = 不可达候选（竞速里应被自动淘汰）
        let cand_sock = crate::direct::bind_udp_socket("127.0.0.1:0".parse().unwrap()).await?;
        let cand_server = crate::Server::bind_socket(Arc::clone(&cand_sock), &server_id)?;
        let cand_addr = cand_sock.local_addr()?;
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();

        // 注册（带候选）但不消费 punch_req：反射路必然超时
        let _server = bind_via_relay_ex(relay_addr, &server_id, vec![cand_addr, dead])
            .await?
            .server;
        let accepter = tokio::spawn(async move {
            let conn = cand_server.accept().await?;
            assert_eq!(conn.peer_fingerprint(), client_fp, "E2E 指纹必须钉扎成功");
            anyhow::Ok(())
        });

        let (path, conn) =
            connect_via_relay_punch(relay_addr, server_fp, &client_id, Some(server_fp)).await?;
        assert_eq!(path, ConnPath::Direct, "可达候选应先于中继命中");
        assert_eq!(conn.peer_fingerprint(), server_fp, "E2E 指纹必须钉扎成功");
        accepter.await??;
        conn.close(0, "candidate e2e done");
        Ok(())
    }

    /// 竞速回退：被控端只注册中继、不响应打洞请求 → 打洞路在窗口内失败，
    /// 竞速自动落在中继路径，连接仍然建成（自动化降级的兜底断言）。
    #[tokio::test(flavor = "multi_thread")]
    async fn 打洞_无响应时竞速回退中继() -> anyhow::Result<()> {
        let relay_addr = relay_spawn("127.0.0.1:0".parse()?).await?;
        let server_id = Identity::generate("punch-fallback-server")?;
        let client_id = Identity::generate("punch-fallback-client")?;
        let server_fp = server_id.fingerprint();
        let client_fp = client_id.fingerprint();

        // 被控端：只经中继监听，不消费 punch_req（= 不打洞）
        let server = bind_via_relay(relay_addr, &server_id).await?;
        let accepter = tokio::spawn(async move {
            let conn = server.accept().await?;
            assert_eq!(conn.peer_fingerprint(), client_fp);
            anyhow::Ok(())
        });

        let (path, conn) =
            connect_via_relay_punch(relay_addr, server_fp, &client_id, Some(server_fp)).await?;
        assert_eq!(path, ConnPath::Relay, "打洞无响应应回退中继");
        assert_eq!(conn.peer_fingerprint(), server_fp, "E2E 指纹必须钉扎成功");
        accepter.await??;
        conn.close(0, "fallback e2e done");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn env_live_relay_quic_e2e() -> anyhow::Result<()> {
        let raw = std::env::var("RD_TEST_LIVE_RELAY").expect("set RD_TEST_LIVE_RELAY");
        let relay_addr: SocketAddr = raw.parse()?;
        let server_id = Identity::generate("live-relay-server")?;
        let client_id = Identity::generate("live-relay-client")?;
        let server_fp = server_id.fingerprint();
        let client_fp = client_id.fingerprint();

        let relay_srv = bind_via_relay_ex(relay_addr, &server_id, Vec::new()).await?;
        let accepter = tokio::spawn(async move {
            let conn = relay_srv.server.accept().await?;
            assert_eq!(conn.peer_fingerprint(), client_fp);
            let (mut tx, mut rx) = conn.accept_control().await?;
            let Some(env) = rx.recv().await? else {
                anyhow::bail!("control closed");
            };
            let Some(envelope::Msg::Hello(h)) = env.msg else {
                anyhow::bail!("expected Hello");
            };
            tx.send(&rd_proto::v0::Envelope {
                req_id: env.req_id,
                msg: Some(envelope::Msg::HelloAck(rd_proto::v0::HelloAck {
                    proto_ver: 0,
                    host_name: format!("live-ok-{}", h.device_name),
                    ..Default::default()
                })),
            })
            .await?;
            while let Ok(Some(_)) = rx.recv().await {}
            anyhow::Ok(())
        });

        let (path, conn) =
            connect_via_relay_punch(relay_addr, server_fp, &client_id, Some(server_fp)).await?;
        println!("LIVE RELAY CONNECTED! path={path:?}, rtt={:?}", conn.rtt());
        let (mut tx, mut rx) = conn.open_control().await?;
        tx.send(&rd_proto::v0::Envelope {
            req_id: 42,
            msg: Some(envelope::Msg::Hello(Hello {
                proto_ver: 0,
                device_name: "cloud-test".into(),
                ..Default::default()
            })),
        })
        .await?;
        let Some(reply) = rx.recv().await? else {
            anyhow::bail!("no HelloAck");
        };
        let Some(envelope::Msg::HelloAck(ack)) = reply.msg else {
            anyhow::bail!("unexpected reply");
        };
        assert_eq!(ack.host_name, "live-ok-cloud-test");
        println!("LIVE RELAY HELLO_ACK OK: {}", ack.host_name);
        conn.close(0, "live e2e ok");
        accepter.await??;
        Ok(())
    }
}
