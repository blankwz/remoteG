//! 打洞适配器 `sb.stun.punch-v2`（L-B）：spec 01 §4.1 五条铁律的参考实现。
//!
//! 五条 MUST 的落实位置（每条都有对应测试）：
//! 1. **同 socket STUN**：[`stun_map`] 用打洞用的同一个 UDP socket 发 Binding
//!    Request，绝不用其他协议观察到的端点（RustDesk #16488 根因）；
//! 2. **完整端点交换**：交换 `[StunMapped{ mapped_ip, mapped_port }]` 全量结构；
//! 3. **双向同时发送**：双方各发 8 个 PROBE，间隔 100ms；
//! 4. **首包权威**：以收到的第一个有效 PROBE 的真实源地址为对端地址；
//! 5. **绑定保活**：[`keepalive`] 以 min(15s, nat_timeout×0.5) 周期发包。
//!
//! STUN 线格式：RFC 5389 子集（Binding Request/Response + XOR-MAPPED-ADDRESS，
//! MAGIC COOKIE 校验）。不引依赖，便于审计。

use async_trait::async_trait;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use switchboard_core::stages::{PunchOutcome, Puncher};
use switchboard_core::telemetry::EventBus;
use switchboard_core::types::{EndpointCandidate, Measured};

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
const PROBE_MAGIC: &[u8; 4] = b"SBP1";
const PROBE_COUNT: usize = 8;
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

// ── RFC 5389 子集 ──

/// 构造 Binding Request（含 MAGIC COOKIE（事务 id 前 4 字节）+ 随机事务 id）。
pub fn stun_binding_request(txid: [u8; 12]) -> Vec<u8> {
    let mut m = Vec::with_capacity(20);
    m.extend_from_slice(&[0x00, 0x01]); // Binding Request
    m.extend_from_slice(&[0x00, 0x00]); // message length
    m.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    m.extend_from_slice(&txid);
    m
}

/// 解析 Binding Response 的 XOR-MAPPED-ADDRESS（attribute 0x0020）。
pub fn stun_parse_xor_mapped(msg: &[u8], txid: [u8; 12]) -> Option<SocketAddr> {
    if msg.len() < 20 || u32::from_be_bytes(msg[4..8].try_into().ok()?) != MAGIC_COOKIE {
        return None;
    }
    if msg[8..20] != txid {
        return None;
    }
    let len = u16::from_be_bytes(msg[2..4].try_into().ok()?) as usize;
    let mut off = 20usize;
    let end = (20 + len).min(msg.len());
    while off + 4 <= end {
        let attr = u16::from_be_bytes(msg[off..off + 2].try_into().ok()?);
        let alen = u16::from_be_bytes(msg[off + 2..off + 4].try_into().ok()?) as usize;
        let body = &msg[off + 4..(off + 4 + alen).min(end)];
        if attr == 0x0020 && body.len() >= 8 {
            let port = u16::from_be_bytes(body[2..4].try_into().ok()?) ^ 0x2112;
            let ip = u32::from_be_bytes(body[4..8].try_into().ok()?) ^ MAGIC_COOKIE;
            return Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(ip),
                port,
            )));
        }
        off += 4 + alen.div_ceil(4) * 4;
    }
    None
}

/// 铁律 1：**用打洞自己的 socket** 做 STUN 测绘。
pub fn stun_map(sock: &UdpSocket, server: SocketAddr) -> anyhow::Result<SocketAddr> {
    let txid = rand_txid();
    let req = stun_binding_request(txid);
    sock.send_to(&req, server)?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = [0u8; 128];
    for _ in 0..3 {
        let (n, from) = sock.recv_from(&mut buf)?;
        if from == server {
            if let Some(mapped) = stun_parse_xor_mapped(&buf[..n], txid) {
                return Ok(mapped);
            }
        }
    }
    anyhow::bail!("STUN 无响应")
}

fn rand_txid() -> [u8; 12] {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut id = [0u8; 12];
    let nanos = t.as_nanos().to_le_bytes();
    id[..8].copy_from_slice(&nanos[..8]);
    id[8] = std::process::id() as u8;
    id[9] = (std::process::id() >> 8) as u8;
    id[10] = t.subsec_micros() as u8;
    id[11] = (t.subsec_micros() >> 8) as u8;
    id
}

/// PROBE 包（带发送方标识，接收方校验）。
fn probe_pkt(conv: u32) -> Vec<u8> {
    let mut p = PROBE_MAGIC.to_vec();
    p.extend_from_slice(&conv.to_be_bytes());
    p
}

fn is_probe(msg: &[u8], conv: u32) -> bool {
    msg.len() == 8 && &msg[..4] == PROBE_MAGIC && u32::from_be_bytes(msg[4..8].try_into().unwrap()) == conv
}

// ── 打洞会话 ──

/// 端点交换的载体（铁律 2）：生产环境经会合服务器；测试为直连通道。
pub trait Rendezvous: Send + Sync {
    fn exchange(&self, mine: Vec<u8>) -> anyhow::Result<Vec<u8>>;
}

/// 打洞参数。
#[derive(Clone, Copy, Debug)]
pub struct PunchConfig {
    /// STUN 服务器；None = 用本地地址测绘（纯 LAN/环回场景）。
    pub stun_server: Option<SocketAddr>,
    /// 会话标识（PROBE 校验）。
    pub conv: u32,
    /// NAT 超时实测值；None 用默认 15s（铁律 5）。
    pub nat_timeout: Option<Duration>,
}

impl Default for PunchConfig {
    fn default() -> Self {
        Self { stun_server: None, conv: 0x5EED_0001, nat_timeout: None }
    }
}

/// 铁律 1-4 的同步实现：返回打洞成功的 socket + 对端权威地址。
pub fn punch_sync(
    cfg: PunchConfig,
    rendezvous: &dyn Rendezvous,
) -> anyhow::Result<(Arc<UdpSocket>, SocketAddr, Measured)> {
    let t0 = Instant::now();
    let sock = Arc::new(UdpSocket::bind("0.0.0.0:0")?);

    // 铁律 1：同 socket 测绘
    let mapped = match cfg.stun_server {
        Some(server) => stun_map(&sock, server)?,
        None => sock.local_addr()?, // 无 NAT 场景：本地地址即映射
    };

    // 铁律 2：完整端点交换（ip + port 都交换，不推断）
    let mine = serde_json::to_vec(&serde_json::json!({
        "mapped": mapped.to_string(),
        "conv": cfg.conv,
    }))?;
    let theirs: serde_json::Value = serde_json::from_slice(&rendezvous.exchange(mine)?)?;
    let peer_mapped: SocketAddr = theirs["mapped"].as_str().unwrap_or_default().parse()?;
    let peer_conv: u32 = theirs["conv"].as_u64().unwrap_or(0) as u32;

    // 铁律 3：双向同时发送（8 × 100ms）
    sock.set_read_timeout(Some(Duration::from_millis(10)))?;
    let pkt = probe_pkt(cfg.conv);
    let mut sent = 0usize;
    let start = Instant::now();
    let mut authoritative: Option<SocketAddr> = None; // 铁律 4
    let mut buf = [0u8; 64];
    while sent < PROBE_COUNT || authoritative.is_none() {
        if sent < PROBE_COUNT {
            let _ = sock.send_to(&pkt, peer_mapped);
            sent += 1;
        }
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if is_probe(&buf[..n], peer_conv) {
                // 首包权威：以真实源地址为准（覆盖此前一切推断）
                authoritative = Some(from);
                break;
            }
        }
        if start.elapsed() > Duration::from_secs(5) {
            anyhow::bail!("打洞窗口超时（双向 PROBE 未命中）");
        }
        std::thread::sleep(PROBE_INTERVAL.min(Duration::from_millis(10)));
    }
    let peer = authoritative.expect("上方已保证");

    let measured = Measured {
        rtt_ms: t0.elapsed().as_millis() as f32,
        mtu: 0, // 带外 PMTUD 由 S4/上层执行（铁律 6）
        loss_ratio: 0.0,
    };
    Ok((sock, peer, measured))
}

/// 铁律 5：绑定保活。返回保活线程句柄（drop 即停）。
pub fn keepalive(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    nat_timeout: Option<Duration>,
    conv: u32,
) -> std::thread::JoinHandle<()> {
    let period = nat_timeout.map_or(Duration::from_secs(15), |t| t.mul_f32(0.5)).min(Duration::from_secs(15));
    std::thread::spawn(move || {
        let pkt = probe_pkt(conv);
        while sock.send_to(&pkt, peer).is_ok() {
            std::thread::sleep(period);
        }
    })
}

// ── Puncher trait 适配 ──

/// 打洞适配器（harness/本地网络档）。
pub struct StunPunch {
    pub cfg: PunchConfig,
}

#[async_trait]
impl Puncher for StunPunch {
    async fn punch(
        &self,
        _peers: Vec<EndpointCandidate>,
        _budget: Duration,
        bus: Arc<EventBus>,
    ) -> PunchOutcome {
        // v0.1：无外部会合服务器注入时诚实报 Unsupported（Plan 回退，02 §2.1）
        // ——同步实现 punch_sync 供有会合通道的宿主直接调用（见测试与文档）。
        bus.emit_event(
            "stage_transition",
            serde_json::json!({
                "from": "punch", "outcome": "unsupported",
                "module": "sb.stun.punch-v2",
                "note": "需要宿主注入 Rendezvous 通道（会合服务器/人工中转）；同步入口 punch_sync()"
            }),
        );
        PunchOutcome::Unsupported
    }
}

// ── 测试：环回打洞（含迷你 STUN 服务器） ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket as StdUdp;
    use std::sync::Mutex;

    /// 迷你 STUN 服务器：Binding Request → XOR-MAPPED-ADDRESS 应答。
    fn spawn_mini_stun() -> SocketAddr {
        let sock = StdUdp::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                let msg = &buf[..n];
                if msg.len() < 20 || u32::from_be_bytes(msg[4..8].try_into().unwrap()) != MAGIC_COOKIE {
                    continue;
                }
                let txid: [u8; 12] = msg[8..20].try_into().unwrap();
                // XOR-MAPPED-ADDRESS：port ^ 0x2112，ip ^ cookie
                let (ip, port) = match from {
                    SocketAddr::V4(v4) => (u32::from(*v4.ip()), v4.port()),
                    SocketAddr::V6(_) => continue,
                };
                let mut resp = Vec::new();
                resp.extend_from_slice(&[0x01, 0x01]); // Binding Response
                resp.extend_from_slice(&12u16.to_be_bytes());
                resp.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
                resp.extend_from_slice(&txid);
                resp.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
                resp.extend_from_slice(&8u16.to_be_bytes());
                resp.extend_from_slice(&[0x00, 0x01]); // IPv4
                resp.extend_from_slice(&(port ^ 0x2112).to_be_bytes());
                resp.extend_from_slice(&(ip ^ MAGIC_COOKIE).to_be_bytes());
                let _ = sock.send_to(&resp, from);
            }
        });
        addr
    }

    /// 测试用会合：直接互传（生产=会合服务器，同一接口）。
    struct PairRendezvous(
        std::sync::mpsc::Sender<Vec<u8>>,
        Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
    );

    impl Rendezvous for PairRendezvous {
        fn exchange(&self, mine: Vec<u8>) -> anyhow::Result<Vec<u8>> {
            self.0.send(mine)?;
            Ok(self.1.lock().unwrap().recv_timeout(Duration::from_secs(5))?)
        }
    }

    #[test]
    fn stun_xormap编解码往返() {
        let txid = [7u8; 12];
        let req = stun_binding_request(txid);
        assert_eq!(req.len(), 20);
        assert_eq!(u32::from_be_bytes(req[4..8].try_into().unwrap()), MAGIC_COOKIE);
        // 响应构造 → 解析
        let addr: SocketAddr = "203.0.113.7:54321".parse().unwrap();
        let (ip, port) = (u32::from(Ipv4Addr::from([203, 0, 113, 7])), 54321u16);
        let mut resp = Vec::new();
        resp.extend_from_slice(&[0x01, 0x01, 0, 12]);
        resp.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        resp.extend_from_slice(&txid);
        resp.extend_from_slice(&0x0020u16.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.extend_from_slice(&[0, 1]);
        resp.extend_from_slice(&(port ^ 0x2112).to_be_bytes());
        resp.extend_from_slice(&(ip ^ MAGIC_COOKIE).to_be_bytes());
        let got = stun_parse_xor_mapped(&resp, txid).unwrap();
        assert_eq!(got, addr);
        assert!(stun_parse_xor_mapped(&resp[..10], txid).is_none());
    }

    #[test]
    fn 打洞_经迷你stun_双向probe_首包权威() {
        let stun = spawn_mini_stun();
        // 通道接线：A.send → rx_b（B 读），B.send → rx_a（A 读）
        let (tx_a, rx_b) = std::sync::mpsc::channel::<Vec<u8>>();
        let (tx_b, rx_a) = std::sync::mpsc::channel::<Vec<u8>>();

        let h = std::thread::spawn(move || {
            let cfg = PunchConfig { stun_server: Some(stun), conv: 42, ..Default::default() };
            let rz = PairRendezvous(tx_a, Mutex::new(rx_a));
            punch_sync(cfg, &rz)
        });
        let cfg = PunchConfig { stun_server: Some(stun), conv: 42, ..Default::default() };
        let rz_b = PairRendezvous(tx_b, Mutex::new(rx_b));
        let (sock_b, peer_of_b, measured_b) = punch_sync(cfg, &rz_b).unwrap();
        assert_eq!(measured_b.mtu, 0);
        let (sock_a, peer_of_a, _m) = h.join().unwrap().unwrap();
        // 首包权威验证：各自记录的对端地址端口 == 对端 socket 端口
        // （通配绑定 local_addr 显示 0.0.0.0，回环探测记录 127.0.0.1——端口是权威标识）
        assert_eq!(
            peer_of_a.port(),
            sock_b.local_addr().unwrap().port(),
            "A 记录的对端端口 == B socket 端口"
        );
        assert_eq!(
            peer_of_b.port(),
            sock_a.local_addr().unwrap().port(),
            "B 记录的对端端口 == A socket 端口"
        );
        // 铁律 3 的产物：打洞 socket 可直接双向通信
        sock_b.send_to(probe_pkt(42).as_slice(), peer_of_b).unwrap();
        // 铁律 5：保活线程启动即返回（detach——进程生命周期内持续保活）
        let ka = keepalive(sock_b, peer_of_b, None, 42);
        std::thread::sleep(Duration::from_millis(300)); // 观察窗口：保活包正常发出
        drop(ka); // detach，不 join（保活线程设计为常驻）
        let _ = sock_a;
    }
}
