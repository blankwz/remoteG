//! rd 原生中继（T-032）：UDP 打标转发器。
//!
//! ## 设计（一句话）
//! 双方跑原生 QUIC（加密/配对/指纹校验全部不变），每个 UDP 数据报前加
//! 8 字节会话标号；中继按标号把数据报转发给对端——**只转发密文，永不解密**。
//!
//! ## 协议（全部小端）
//! ```text
//! 数据报:  [u64 tag][QUIC 原始包]                —— tag != 0
//! 控制报:  [u64 0][u8 type][32B fingerprint][...]
//!   type 1 = REG(agent 注册)    应答 type 2: [u8 status][u64 tag]
//!   type 3 = DIAL(controller)   应答 type 4: [u8 status][u64 tag][对端地址?]
//!   type 5 = PUNCH_NOTIFY(relay→agent): [32B 指纹][发起方地址]
//!   type 6 = REG2（签名注册，安全审计 F4）应答 type 2 同上
//!   status: 1=ok 2=not_found 3=conflict（fp→tag 冲突）
//! tag = fingerprint 前 8 字节（自证书指纹派生，无需协商；碰撞概率可忽略）
//!
//! 签名控制报（type 6）:
//!   [u64 0][u8 type][32B fp][u16LE cert_len][证书 DER][64B Ed25519 签名][候选尾巴?]
//!   签名 = Ed25519(身份私钥, [`REG2_DOMAIN`] ‖ fp)。
//!   中继验两项：SHA-256(证书)==fp 且签名可由证书内公钥验证——**指纹持有证明**。
//!
//! ## 签名会话规则（安全审计 F4：控制面 PoP）
//! - 签名 REG2 验证通过才建立/升级「签名会话」：只有指纹持有者能成为该
//!   tag 的被控端（防 fp 知晓者抢注会话、截收被控端方向密文与 PUNCH_NOTIFY）；
//! - 签名会话不可被 legacy REG 刷新/降级（静默忽略）；
//! - 未签名（旧端）会话行为与旧版完全一致（兼容旧中继/旧端自动退化）；
//! - 新 agent 同时发 legacy + 签名两份 REG：旧中继认 legacy、新中继认签名。
//!
//! ## 残余风险（如实记录）
//! DIAL 无法做等价的指纹持有证明：DIAL 声明的 fp 是**对端**（被控端）的，
//! 签名者证书是**本端**（控制端）的，两者不相等，PoP 语义不成立；且「谁有
//! 权拨某 tag」的授权发生在 QUIC 层，中继无从知晓授权列表。因此知晓完整
//! 32 字节指纹者仍可通过反复 DIAL 翻动 ctrl 槽（拿到的只是密文 + ACK 里的
//! 地址元数据，无法冒充任一端——双向指纹钉扎不受影响）。指纹在此协议里
//! 就是 bearer 能力：不把完整指纹透露给不可信方是运营者纪律（指纹不进
//! 日志/发现应答只出前 8 字节 tag）。
//!
//! ## 每源 IP 会话上限（安全审计 F4：防会话表塞满）
//! 同一来源 IP 最多注册 [`MAX_SESSIONS_PER_IP`] 个会话，超出拒绝（防垃圾
//! 注册把 [`MAX_SESSIONS`] 表塞满、饿死真实被控端）。
//!
//! ## 打洞协调（T-034/T-035）
//! DIAL 成功时中继顺带做两件事：
//! 1. DIAL ACK 尾部附上被控端地址列表（`[family][port][ip]` 逐个连排）：
//!    首个 = 被控端在中继处的 NAT 映射（反射打洞的探测目标），其余 =
//!    被控端随 REG 上报的直连候选（本机网卡地址/UPnP 外部映射等，上限
//!    [`MAX_CANDIDATES`]）——旧客户端只读首个，忽略其余；
//! 2. 向被控端发 PUNCH_NOTIFY（携带控制端在中继处的地址）。
//! 控制端拿到列表后**多路竞速**：对映射地址做反射打洞（互发
//! [`PUNCH_PROBE`] 开洞学映射）、对候选直接发起 QUIC、与中继路径同场竞速，
//! 谁先握手完成用谁。旧中继（不回地址/不发通知）→ 自然退化为纯中继路径。
//! ```
//!
//! ## 为什么不是"流拼接"
//! 流拼接需要中继终结两端 QUIC，即能看到明文——破坏「中转无法窥屏」的
//! 核心承诺。打标转发让 QUIC 包原样穿越中继，加密层次零变化。
//!
//! ## 容量与限制
//! 单会话 = 一个 fingerprint（rd 免费版本就 1 并发；Pro 多控后续扩展）；
//! 会话空闲 >60s 回收（rd QUIC keep-alive 5s，远大于此）；数据报上限
//! 1500B（rd MTU 钳 1200，留余量）。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, VerifyingKey, SIGNATURE_LENGTH};
use sha2::{Digest, Sha256};
use tokio::net::UdpSocket;

/// 中继默认监听端口（与 rd 的 QUIC 端口一致，运维只记一个数字）。
pub const DEFAULT_PORT: u16 = 21118;
/// 数据报大小上限（rd MTU 钳 1200 + 裕量）。
pub const MAX_DATAGRAM: usize = 1500;
/// 会话空闲回收阈值（> rd 的 QUIC idle timeout 20s，活跃会话不会被误删）。
pub const SESSION_IDLE: Duration = Duration::from_secs(60);
/// 最大并发会话（免费版 1 并发的中继侧放大裕量）。
pub const MAX_SESSIONS: usize = 512;
/// 同一来源 IP 的会话数上限（安全审计 F4：防垃圾注册塞满会话表）。
pub const MAX_SESSIONS_PER_IP: usize = 16;

pub const TAG_CONTROL: u64 = 0;
pub const TYPE_REG: u8 = 1;
pub const TYPE_REG_ACK: u8 = 2;
pub const TYPE_DIAL: u8 = 3;
pub const TYPE_DIAL_ACK: u8 = 4;
/// PUNCH_NOTIFY（relay→agent）：携带 DIAL 发起方（控制端）在中继处的地址。
pub const TYPE_PUNCH_NOTIFY: u8 = 5;
/// 签名 REG（安全审计 F4）：指纹持有证明的注册。
pub const TYPE_REG2: u8 = 6;
pub const STATUS_OK: u8 = 1;
pub const STATUS_NOT_FOUND: u8 = 2;
pub const STATUS_CONFLICT: u8 = 3;
/// 控制报固定长度：tag(8) + type(1) + fp(32) = 41
const CTRL_LEN: usize = 41;
/// DIAL ACK 基础长度：41 + status(1) + tag(8) = 50（地址尾巴若有则从 50 起）。
const ACK_BASE_LEN: usize = CTRL_LEN + 1 + 8;
/// 双端打洞探测包的固定载荷（非 QUIC；QUIC 端点收到会静默丢弃）。
pub const PUNCH_PROBE: &[u8] = b"RDPUNCH1";
/// 被控端上报的直连候选地址上限（控制报 ≤1500B：REG ≈ 41+4×19B，余量充足）。
pub const MAX_CANDIDATES: usize = 4;
/// 签名控制报固定头长度：tag(8) + type(1) + fp(32) + cert_len(2) + sig(64)。
const SIGNED_HDR_LEN: usize = 8 + 1 + 32 + 2 + SIGNATURE_LENGTH;
/// REG2 签名域（域分离，防跨协议重放签名）。
pub const REG2_DOMAIN: &[u8] = b"rd-relay-reg2-v1";

/// 会话：一个被控端指纹 ↔ 双端最新 NAT 地址 + 被控端上报的直连候选（T-035）。
struct Session {
    fp: [u8; 32],
    agent: Option<SocketAddr>,
    agent_seen: Instant,
    ctrl: Option<SocketAddr>,
    ctrl_seen: Instant,
    /// 被控端直连候选（本机网卡地址/UPnP 映射等，agent 采集后随 REG 上报）。
    candidates: Vec<SocketAddr>,
    /// 注册是否经过签名验证（安全审计 F4）。签名会话拒绝 legacy REG 的
    /// 刷新/降级——只有指纹持有者能成为该 tag 的被控端。
    signed: bool,
}

/// 状态表 + GC 共享句柄。
#[derive(Default)]
struct Table {
    by_tag: HashMap<u64, Session>,
}

impl Table {
    fn tag_of(fp: &[u8; 32]) -> u64 {
        u64::from_le_bytes(fp[..8].try_into().expect("fp 前 8 字节"))
    }
}

/// 握手控制报编码（REG/DIAL 共用形状）。
pub fn encode_control(kind: u8, fp: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CTRL_LEN);
    out.extend_from_slice(&TAG_CONTROL.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(fp);
    out
}

/// 握手应答编码。
pub fn encode_control_ack(kind: u8, fp: &[u8; 32], status: u8, tag: u64) -> Vec<u8> {
    let mut out = encode_control(kind, fp);
    out.push(status);
    out.extend_from_slice(&tag.to_le_bytes());
    out
}

/// 解码控制报（tag 必须为 0）。
pub fn decode_control(buf: &[u8]) -> Option<(u8, [u8; 32])> {
    if buf.len() < CTRL_LEN {
        return None;
    }
    if u64::from_le_bytes(buf[..8].try_into().ok()?) != TAG_CONTROL {
        return None;
    }
    let kind = buf[8];
    let fp: [u8; 32] = buf[9..41].try_into().ok()?;
    Some((kind, fp))
}

/// 解码控制应答尾部的 (status, tag)。
pub fn decode_control_ack_tail(buf: &[u8]) -> Option<(u8, u64)> {
    if buf.len() < ACK_BASE_LEN {
        return None;
    }
    let status = buf[CTRL_LEN];
    let tag = u64::from_le_bytes(buf[CTRL_LEN + 1..CTRL_LEN + 9].try_into().ok()?);
    Some((status, tag))
}

// ── 地址编解码（打洞协调用）：[family u8][port u16 le][ip 4|16B] ──

/// 编码 SocketAddr 追加到 `out`。
pub fn encode_addr(out: &mut Vec<u8>, addr: &SocketAddr) {
    match addr {
        SocketAddr::V4(a) => {
            out.push(4);
            out.extend_from_slice(&a.port().to_le_bytes());
            out.extend_from_slice(&a.ip().octets());
        }
        SocketAddr::V6(a) => {
            out.push(6);
            out.extend_from_slice(&a.port().to_le_bytes());
            out.extend_from_slice(&a.ip().octets());
        }
    }
}

/// 从 `buf[off..]` 解码一个地址，返回 (地址, 消费字节数)。
pub fn decode_addr(buf: &[u8], off: usize) -> Option<(SocketAddr, usize)> {
    let family = *buf.get(off)?;
    let port = u16::from_le_bytes(buf.get(off + 1..off + 3)?.try_into().ok()?);
    let octets = buf.get(off + 3..)?;
    Some(match family {
        4 => {
            let ip: [u8; 4] = octets.get(..4)?.try_into().ok()?;
            (SocketAddr::from((ip, port)), 3 + 4)
        }
        6 => {
            let ip: [u8; 16] = octets.get(..16)?.try_into().ok()?;
            (SocketAddr::new(ip.into(), port), 3 + 16)
        }
        _ => return None,
    })
}

/// DIAL ACK 编码：基础应答 + 可选地址尾巴（打洞协调）：`[中继处映射地址]
/// [被控端直连候选...]`。首个为被控端在中继处的映射地址（探测/反射学习
/// 目标），其余为被控端上报的直连候选。旧客户端只读首个，行为不变。
pub fn encode_dial_ack(fp: &[u8; 32], status: u8, tag: u64, agent_addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = encode_control_ack(TYPE_DIAL_ACK, fp, status, tag);
    for addr in agent_addrs {
        encode_addr(&mut out, addr);
    }
    out
}

/// 读取 DIAL ACK 尾巴里的地址列表（空 = 旧中继 → 不打洞）。
pub fn decode_dial_ack_agent_addrs(buf: &[u8]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let mut off = ACK_BASE_LEN;
    while off < buf.len() {
        match decode_addr(buf, off) {
            Some((addr, used)) => {
                out.push(addr);
                off += used;
            }
            None => break, // 坏尾巴：保留已解出的部分
        }
    }
    out
}

/// REG 编码：`[tag0][type1][fp32][候选地址...]`——被控端把直连候选（本机
/// 网卡地址、UPnP 外部映射等）随注册上报，中继存进会话供 DIAL ACK 下发。
pub fn encode_reg(fp: &[u8; 32], candidates: &[SocketAddr]) -> Vec<u8> {
    let mut out = encode_control(TYPE_REG, fp);
    for addr in candidates.iter().take(MAX_CANDIDATES) {
        encode_addr(&mut out, addr);
    }
    out
}

/// 解码 REG 尾巴里的直连候选（无尾巴 = 旧 agent，兼容）。
pub fn decode_reg_candidates(buf: &[u8]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let mut off = CTRL_LEN;
    while off < buf.len() && out.len() < MAX_CANDIDATES {
        match decode_addr(buf, off) {
            Some((addr, used)) => {
                out.push(addr);
                off += used;
            }
            None => break,
        }
    }
    out
}

// ── 签名控制报（安全审计 F4：指纹持有证明）──────────────────────

/// 签名域：按控制报类型取对应 domain（域分离防跨类型重放）。
pub fn sign_domain(kind: u8) -> &'static [u8] {
    match kind {
        TYPE_REG2 => REG2_DOMAIN,
        _ => b"rd-relay-unknown-v1",
    }
}

/// 签名覆盖的消息：`domain ‖ fp`。
pub fn signed_message(kind: u8, fp: &[u8; 32]) -> Vec<u8> {
    let mut msg = sign_domain(kind).to_vec();
    msg.extend_from_slice(fp);
    msg
}

/// 编码签名控制报：`[tag0][type][fp32][u16LE cert_len][证书 DER][64B 签名][候选尾巴?]`。
/// `candidates` 仅 REG2 携带（语义与 legacy REG 的尾巴一致）。
pub fn encode_signed_control(
    kind: u8,
    fp: &[u8; 32],
    cert_der: &[u8],
    sig: &[u8; SIGNATURE_LENGTH],
    candidates: &[SocketAddr],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIGNED_HDR_LEN + cert_der.len() + candidates.len() * 19);
    out.extend_from_slice(&TAG_CONTROL.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(fp);
    let cert_len =
        u16::try_from(cert_der.len()).expect("证书 DER 不超过 64KiB（实际 ~300B）");
    out.extend_from_slice(&cert_len.to_le_bytes());
    out.extend_from_slice(cert_der);
    out.extend_from_slice(sig);
    for addr in candidates.iter().take(MAX_CANDIDATES) {
        encode_addr(&mut out, addr);
    }
    out
}

/// 解码签名控制报的产物：`(type, fp, 证书 DER, 签名, 候选尾巴起始偏移)`。
pub type SignedControl<'a> = (
    u8,
    [u8; 32],
    &'a [u8],
    [u8; SIGNATURE_LENGTH],
    usize,
);

/// 解码签名控制报，返回 [`SignedControl`]。长度字段超界/截断返回 None。
pub fn decode_signed_control(buf: &[u8]) -> Option<SignedControl<'_>> {
    if buf.len() < SIGNED_HDR_LEN {
        return None;
    }
    if u64::from_le_bytes(buf[..8].try_into().ok()?) != TAG_CONTROL {
        return None;
    }
    let kind = buf[8];
    let fp: [u8; 32] = buf[9..41].try_into().ok()?;
    let cert_len = u16::from_le_bytes(buf[41..43].try_into().ok()?) as usize;
    let cert_end = 43usize.checked_add(cert_len)?;
    if buf.len() < cert_end + SIGNATURE_LENGTH {
        return None;
    }
    let cert = &buf[43..cert_end];
    let sig: [u8; SIGNATURE_LENGTH] = buf[cert_end..cert_end + SIGNATURE_LENGTH]
        .try_into()
        .ok()?;
    Some((kind, fp, cert, sig, cert_end + SIGNATURE_LENGTH))
}

/// 解码 REG2 尾巴里的直连候选（从 `off` 起，语义同 [`decode_reg_candidates`]）。
pub fn decode_reg_candidates_at(buf: &[u8], mut off: usize) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    while off < buf.len() && out.len() < MAX_CANDIDATES {
        match decode_addr(buf, off) {
            Some((addr, used)) => {
                out.push(addr);
                off += used;
            }
            None => break,
        }
    }
    out
}

/// 验证签名控制报：证书指纹匹配 + 证书内 Ed25519 公钥验证签名。任一失败即 false。
pub fn verify_signed_control(
    kind: u8,
    fp: &[u8; 32],
    cert_der: &[u8],
    sig: &[u8; SIGNATURE_LENGTH],
) -> bool {
    // 1) 证书指纹 = 注册/拨号声明的 fp
    if Sha256::digest(cert_der).as_slice() != fp {
        return false;
    }
    // 2) 证书内公钥验证签名（域分离消息）
    let Some(pub_bytes) = ed25519_pub_from_cert(cert_der) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pub_bytes) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(sig) else {
        return false;
    };
    vk.verify_strict(&signed_message(kind, fp), &sig).is_ok()
}

/// 从自签 X.509 证书 DER 中提取 Ed25519 公钥（SPKI BIT STRING 32 字节）。
///
/// 与 rd-transport::verify 的手写 TLV 同思路：只走确定性 DER 的必经路径
/// `Certificate→TBSCertificate→[0] version?→serial→sigAlg→issuer→validity→
/// subject→SPKI`，SPKI 内 `AlgorithmIdentifier` 后的 BIT STRING 首字节为
/// 未用位数（Ed25519 恒 0），其后 32 字节即公钥。
pub fn ed25519_pub_from_cert(der: &[u8]) -> Option<[u8; 32]> {
    let cert = read_tlv(der)?;
    expect_tag(cert, 0x30)?;
    let tbs = read_tlv(cert.content)?;
    expect_tag(tbs, 0x30)?;

    let mut cur = tbs.content;
    // 可选 [0] EXPLICIT version
    if let Some(first) = read_tlv(cur) {
        if first.tag == 0xA0 {
            cur = first.rest;
        }
    }
    let serial = read_tlv(cur)?;
    expect_tag(serial, 0x02)?;
    let sig_alg = read_tlv(serial.rest)?;
    expect_tag(sig_alg, 0x30)?;
    let issuer = read_tlv(sig_alg.rest)?;
    expect_tag(issuer, 0x30)?;
    let validity = read_tlv(issuer.rest)?;
    expect_tag(validity, 0x30)?;
    let subject = read_tlv(validity.rest)?;
    expect_tag(subject, 0x30)?;
    let spki = read_tlv(subject.rest)?;
    expect_tag(spki, 0x30)?;
    // SPKI ::= SEQUENCE { algorithm AlgorithmIdentifier, subjectPublicKey BIT STRING }
    let alg = read_tlv(spki.content)?;
    expect_tag(alg, 0x30)?;
    let bitstring = read_tlv(alg.rest)?;
    expect_tag(bitstring, 0x03)?;
    let bs = bitstring.content;
    // BIT STRING 首字节 = 未用位数；Ed25519 公钥 32 字节 → 内容 33 字节、未用 0
    if bs.len() != 33 || bs[0] != 0 {
        return None;
    }
    Some(bs[1..].try_into().expect("长度已校验为 32"))
}

#[derive(Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
    rest: &'a [u8],
}

fn expect_tag(tlv: Tlv<'_>, tag: u8) -> Option<Tlv<'_>> {
    (tlv.tag == tag).then_some(tlv)
}

/// 解析一个确定性 DER TLV（只支持定长长度，长度 ≤ u32::MAX）。
fn read_tlv(buf: &[u8]) -> Option<Tlv<'_>> {
    if buf.len() < 2 {
        return None;
    }
    let tag = buf[0];
    let (len, header_len) = if buf[1] & 0x80 == 0 {
        (usize::from(buf[1]), 2)
    } else {
        let n = usize::from(buf[1] & 0x7F);
        if n == 0 || n > 4 || buf.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for byte in &buf[2..2 + n] {
            len = (len << 8) | usize::from(*byte);
        }
        (len, 2 + n)
    };
    let content_end = header_len.checked_add(len)?;
    if buf.len() < content_end {
        return None;
    }
    Some(Tlv {
        tag,
        content: &buf[header_len..content_end],
        rest: &buf[content_end..],
    })
}

/// PUNCH_NOTIFY 编码：[tag0][type5][fp32][发起方地址]。
pub fn encode_punch_notify(fp: &[u8; 32], initiator: &SocketAddr) -> Vec<u8> {
    let mut out = encode_control(TYPE_PUNCH_NOTIFY, fp);
    encode_addr(&mut out, initiator);
    out
}

/// 解码 PUNCH_NOTIFY，返回 (被控端指纹, 发起方地址)。
pub fn decode_punch_notify(buf: &[u8]) -> Option<([u8; 32], SocketAddr)> {
    if buf.len() <= CTRL_LEN {
        return None;
    }
    let (kind, fp) = decode_control(buf)?;
    if kind != TYPE_PUNCH_NOTIFY {
        return None;
    }
    decode_addr(buf, CTRL_LEN).map(|(a, _)| (fp, a))
}

/// 启动中继：绑定 `bind`，收包循环进后台任务。返回实际监听地址（bind
/// 端口为 0 时由系统分配）。调用方持有返回即代表服务在跑；进程退出 =
/// 服务退出（运维层负责守护，见 deploy 脚本）。
pub async fn spawn(bind: SocketAddr) -> anyhow::Result<SocketAddr> {
    let sock = Arc::new(UdpSocket::bind(bind).await?);
    let local = sock.local_addr()?;
    tracing::info!(%local, "rd-relay 启动");
    let table = Arc::new(Mutex::new(Table::default()));

    // GC：回收空闲会话
    {
        let table = Arc::clone(&table);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let mut t = table.lock().expect("relay table 锁中毒");
                let before = t.by_tag.len();
                t.by_tag.retain(|_, s| {
                    let agent_live = s.agent_seen.elapsed() < SESSION_IDLE;
                    let ctrl_live = s.ctrl_seen.elapsed() < SESSION_IDLE;
                    agent_live || ctrl_live
                });
                if t.by_tag.len() != before {
                    tracing::info!(
                        回收 = before - t.by_tag.len(),
                        存活 = t.by_tag.len(),
                        "空闲会话清理"
                    );
                }
            }
        });
    }

    // 收包主循环（独立任务，spawn 立即返回）
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATAGRAM + 64];
        loop {
            let (n, from) = match sock.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(err) => {
                    tracing::warn!("中继收包失败(继续): {err}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if n < 9 {
                continue; // 连 tag 都不完整
            }
            let tag = u64::from_le_bytes(buf[..8].try_into().expect("8 字节"));
            let pkt = &buf[..n];
            if tag == TAG_CONTROL {
                handle_control(&sock, &table, pkt, from).await;
            } else {
                // 原样转发（含 tag）：两端虚拟 socket 用 tag 验证会话归属，
                // 且 QUIC 包一个字节都不能动（密文完整性）
                handle_data(&sock, &table, tag, pkt, from);
            }
        }
    });
    Ok(local)
}

async fn handle_control(sock: &UdpSocket, table: &Arc<Mutex<Table>>, pkt: &[u8], from: SocketAddr) {
    let Some((kind, fp)) = decode_control(pkt) else {
        return;
    };
    // 签名控制报（type 6/7）：先验签（指纹持有证明），失败静默丢弃
    let reply = match kind {
        TYPE_REG => handle_reg(table, fp, from, false, decode_reg_candidates(pkt)),
        TYPE_REG2 => {
            let Some((_, fp, cert, sig, tail_off)) = decode_signed_control(pkt) else {
                return;
            };
            if !verify_signed_control(TYPE_REG2, &fp, cert, &sig) {
                tracing::debug!(peer = %from, "签名 REG 验证失败，丢弃");
                return;
            }
            tracing::debug!(tag = Table::tag_of(&fp), peer = %from, "签名 REG 验证通过");
            handle_reg(table, fp, from, true, decode_reg_candidates_at(pkt, tail_off))
        }
        TYPE_DIAL => handle_dial(sock, table, fp, from).await,
        _ => None,
    };
    if let Some(reply) = reply {
        let _ = sock.send_to(&reply, from).await;
    }
}

/// REG / REG2 共用注册逻辑。`signed=true` 为验签通过的签名注册。
/// 签名会话不可被 legacy 报文刷新或降级（静默忽略，不回 ACK）。
fn handle_reg(
    table: &Arc<Mutex<Table>>,
    fp: [u8; 32],
    from: SocketAddr,
    signed: bool,
    candidates: Vec<SocketAddr>,
) -> Option<Vec<u8>> {
    let mut t = table.lock().expect("relay table 锁中毒");
    let tag = Table::tag_of(&fp);
    if let Some(existing) = t.by_tag.get(&tag) {
        if existing.fp == fp && existing.signed && !signed {
            // 安全审计 F4：签名会话不接受未认证刷新（防 fp 知晓者抢注/续命）
            tracing::debug!(tag, peer = %from, "签名会话拒绝 legacy REG，忽略");
            return None;
        }
        if existing.fp != fp {
            tracing::warn!(peer = %from, "REG 被拒（tag 冲突）");
            return Some(encode_control_ack(TYPE_REG_ACK, &fp, STATUS_CONFLICT, 0));
        }
    } else if t.by_tag.len() >= MAX_SESSIONS
        || sessions_from_ip(&t, from.ip()) >= MAX_SESSIONS_PER_IP
    {
        tracing::warn!(peer = %from, "REG 被拒（会话表已满或达单 IP 上限）");
        return Some(encode_control_ack(TYPE_REG_ACK, &fp, STATUS_CONFLICT, 0));
    }
    let s = t.by_tag.entry(tag).or_insert_with(|| {
        tracing::info!(tag, peer = %from, signed, "新被控端注册");
        Session {
            fp,
            agent: None,
            agent_seen: Instant::now(),
            ctrl: None,
            ctrl_seen: Instant::now(),
            candidates: Vec::new(),
            signed,
        }
    });
    // NAT 地址与直连候选始终以最新出包为准；签名标记只升不降
    s.agent = Some(from);
    s.agent_seen = Instant::now();
    s.candidates = candidates;
    if signed {
        s.signed = true;
    }
    tracing::debug!(tag, peer = %from, signed, "REG 刷新");
    Some(encode_control_ack(TYPE_REG_ACK, &fp, STATUS_OK, tag))
}

/// DIAL 拨入逻辑（无签名——见模块注释「残余风险」）。
async fn handle_dial(
    sock: &UdpSocket,
    table: &Arc<Mutex<Table>>,
    fp: [u8; 32],
    from: SocketAddr,
) -> Option<Vec<u8>> {
    // 锁内取出 (应答, 被控端地址)，发送在锁外
    let (reply, punch_to_agent) = {
        let mut t = table.lock().expect("relay table 锁中毒");
        let tag = Table::tag_of(&fp);
        match t.by_tag.get_mut(&tag) {
            Some(s) if s.fp == fp => {
                s.ctrl = Some(from);
                s.ctrl_seen = Instant::now();
                tracing::info!(tag, peer = %from, "控制端接入");
                // 地址列表：首个 = 中继处映射地址（探测目标），其余 = 直连候选
                let agent_addr = s.agent;
                let mut addrs = agent_addr.into_iter().collect::<Vec<_>>();
                addrs.extend(s.candidates.iter().copied());
                let notify = agent_addr.map(|addr| (encode_punch_notify(&fp, &from), addr));
                (encode_dial_ack(&fp, STATUS_OK, tag, &addrs), notify)
            }
            _ => {
                tracing::debug!(peer = %from, "DIAL 目标不在线");
                (encode_dial_ack(&fp, STATUS_NOT_FOUND, 0, &[]), None)
            }
        }
    };
    // 打洞协调：告知被控端「控制端在中继处的地址」（尽力送达；
    // 旧 agent 不认识 type 5 会静默丢弃，退化为纯中继路径）
    if let Some((pkt, agent_addr)) = punch_to_agent {
        if let Err(err) = sock.try_send_to(&pkt, agent_addr) {
            tracing::debug!(%agent_addr, "PUNCH_NOTIFY 发送失败(尽力送达): {err}");
        }
    }
    Some(reply)
}

/// 统计某来源 IP 名下的会话数（安全审计 F4：单 IP 注册上限的计数口径）。
fn sessions_from_ip(t: &Table, ip: IpAddr) -> usize {
    t.by_tag
        .values()
        .filter(|s| s.agent.map(|a| a.ip()) == Some(ip))
        .count()
}

fn handle_data(
    sock: &UdpSocket,
    table: &Arc<Mutex<Table>>,
    tag: u64,
    payload: &[u8],
    from: SocketAddr,
) {
    // payload = 原样数据报（含 tag 头）
    if payload.len() <= 8 {
        return;
    }
    let peer = {
        let mut t = table.lock().expect("relay table 锁中毒");
        let Some(s) = t.by_tag.get_mut(&tag) else {
            return; // 未知会话：静默丢弃
        };
        if s.agent == Some(from) {
            s.agent_seen = Instant::now();
            s.ctrl
        } else if s.ctrl == Some(from) {
            s.ctrl_seen = Instant::now();
            s.agent
        } else if s.ctrl.is_none() {
            // 控制端首个数据报（DIAL ACK 可能丢）：按来源登记
            s.ctrl = Some(from);
            s.ctrl_seen = Instant::now();
            s.agent
        } else {
            return; // 三方来源：丢弃
        }
    };
    let Some(dest) = peer else {
        return; // 对端还没出现
    };
    // 高频小包直接 try_send_to 不排队；拥塞与重传交给 QUIC 端点自己处理
    if let Err(err) = sock.try_send_to(payload, dest) {
        tracing::debug!(%dest, "转发失败(对端 NAT 口可能刚换): {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for<F: FnMut() -> Option<T>, T>(
        mut f: F,
        timeout: Duration,
    ) -> anyhow::Result<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = f() {
                return Ok(v);
            }
            if Instant::now() > deadline {
                anyhow::bail!("等待超时");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn reg_dial_然后双向转发() -> anyhow::Result<()> {
        let relay = spawn("127.0.0.1:0".parse()?).await?;

        let fp_agent = [7u8; 32];
        let agent = UdpSocket::bind("127.0.0.1:0").await?;
        let ctrl = UdpSocket::bind("127.0.0.1:0").await?;

        // agent REG（带重试，等 ACK）
        let reg = encode_control(TYPE_REG, &fp_agent);
        agent.send_to(&reg, relay).await?;
        wait_for(
            || {
                let mut buf = [0u8; 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from))
                        if from == relay
                            && decode_control_ack_tail(&buf[..n])
                                .map(|(s, t)| s == STATUS_OK && t == Table::tag_of(&fp_agent))
                                .unwrap_or(false) =>
                    {
                        Some(())
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;

        // ctrl DIAL → ok
        let dial = encode_control(TYPE_DIAL, &fp_agent);
        ctrl.send_to(&dial, relay).await?;
        wait_for(
            || {
                let mut buf = [0u8; 64];
                match ctrl.try_recv_from(&mut buf) {
                    Ok((n, from))
                        if from == relay
                            && decode_control_ack_tail(&buf[..n])
                                .map(|(s, _)| s == STATUS_OK)
                                .unwrap_or(false) =>
                    {
                        Some(())
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;

        // ctrl → 数据报（tag + payload）→ agent 收到原 payload
        // （跳过 PUNCH_NOTIFY 等控制报：tag=0 开头）
        let tag = Table::tag_of(&fp_agent);
        let pkt = tag.to_le_bytes().to_vec();
        let mut data = pkt.clone();
        data.extend_from_slice(b"hello-ctrl");
        ctrl.send_to(&data, relay).await?;
        let got = wait_for(
            || {
                let mut buf = [0u8; MAX_DATAGRAM + 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay && buf[..8] != [0u8; 8] => {
                        Some(buf[..n].to_vec())
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        // 原样转发（含 tag 头）——两端虚拟 socket 靠 tag 验证会话
        let mut expect = tag.to_le_bytes().to_vec();
        expect.extend_from_slice(b"hello-ctrl");
        assert_eq!(got, expect, "agent 应收到 ctrl 的原样数据报");

        // agent → ctrl 反向
        let mut back = pkt;
        back.extend_from_slice(b"bye-agent");
        agent.send_to(&back, relay).await?;
        let got = wait_for(
            || {
                let mut buf = [0u8; MAX_DATAGRAM + 64];
                match ctrl.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => Some(buf[..n].to_vec()),
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        let mut expect = tag.to_le_bytes().to_vec();
        expect.extend_from_slice(b"bye-agent");
        assert_eq!(got, expect);

        // 未知会话被静默丢弃（tag 随机）
        let mut rogue = vec![0u8; 8];
        rogue.extend_from_slice(&0xdead_beefu64.to_le_bytes());
        rogue.extend_from_slice(b"x");
        let rogue_sock = UdpSocket::bind("127.0.0.1:0").await?;
        rogue_sock.send_to(&rogue, relay).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // 不炸即通过（中继只记 debug）
        Ok(())
    }

    #[tokio::test]
    async fn dial_不在线返回not_found() -> anyhow::Result<()> {
        let relay = spawn("127.0.0.1:0".parse()?).await?;
        let ctrl = UdpSocket::bind("127.0.0.1:0").await?;
        let fp = [9u8; 32];
        ctrl.send_to(&encode_control(TYPE_DIAL, &fp), relay).await?;
        let (status, _) = wait_for(
            || {
                let mut buf = [0u8; 64];
                match ctrl.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => decode_control_ack_tail(&buf[..n]),
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        assert_eq!(status, STATUS_NOT_FOUND);
        Ok(())
    }

    #[test]
    fn 地址编解码_roundtrip_v4_v6() {
        for addr in [
            "1.2.3.4:21118".parse::<SocketAddr>().unwrap(),
            "[fd00::1]:9".parse::<SocketAddr>().unwrap(),
        ] {
            let mut buf = Vec::new();
            encode_addr(&mut buf, &addr);
            let (got, used) = decode_addr(&buf, 0).expect("解码失败");
            assert_eq!(got, addr);
            assert_eq!(used, buf.len());
        }
        // 短缓冲/坏 family 不炸不误读
        assert!(decode_addr(&[4, 0], 0).is_none());
        assert!(decode_addr(&[7, 0, 0, 1, 2, 3, 4], 0).is_none());
    }

    // ── 签名控制报（安全审计 F4）────────────────────────────────

    /// 生成测试身份：Ed25519 密钥 + 自签证书 DER + 证书指纹。
    fn test_identity(name: &str) -> (ed25519_dalek::SigningKey, Vec<u8>, [u8; 32]) {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        let mut params = rcgen::CertificateParams::default();
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2124, 1, 1);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("生成密钥");
        let cert = params.self_signed(&key_pair).expect("自签");
        let cert_der = cert.der().to_vec();
        let fp: [u8; 32] = sha2::Sha256::digest(&cert_der).into();
        let signing =
            ed25519_dalek::SigningKey::from_pkcs8_der(&key_pair.serialize_der()).expect("解析密钥");
        (signing, cert_der, fp)
    }

    /// 用测试身份签一条控制报消息。
    fn test_sign(kind: u8, fp: &[u8; 32], key: &ed25519_dalek::SigningKey) -> [u8; 64] {
        use ed25519_dalek::Signer;
        key.sign(&signed_message(kind, fp)).to_bytes()
    }

    #[test]
    fn 证书公钥提取_roundtrip与拒绝() {
        let (_key, cert_der, fp) = test_identity("spki-test");
        let pub_bytes = ed25519_pub_from_cert(&cert_der).expect("提取公钥");
        let vk = VerifyingKey::from_bytes(&pub_bytes).expect("公钥可解析");
        // 公钥签名可验证、指纹匹配
        let msg = signed_message(TYPE_REG2, &fp);
        let sig = {
            use ed25519_dalek::Signer;
            _key.sign(&msg)
        };
        assert!(vk.verify_strict(&msg, &sig).is_ok());
        // 篡改证书一个字节 → 指纹失配即拒（extract 可能仍成功但整体验证必败）
        let mut bad = cert_der.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x01;
        let bad_fp: [u8; 32] = sha2::Sha256::digest(&bad).into();
        assert!(!verify_signed_control(
            TYPE_REG2,
            &bad_fp,
            &bad,
            &sig.to_bytes()
        ));
        // 垃圾 DER → 提取失败
        assert!(ed25519_pub_from_cert(b"garbage").is_none());
    }

    #[test]
    fn 签名控制报编解码_roundtrip() {
        let (key, cert_der, fp) = test_identity("codec-test");
        let sig = test_sign(TYPE_REG2, &fp, &key);
        let cands: Vec<SocketAddr> = ["10.0.0.9:21118".parse().unwrap()].to_vec();
        let pkt = encode_signed_control(TYPE_REG2, &fp, &cert_der, &sig, &cands);
        let (kind, got_fp, got_cert, got_sig, off) = decode_signed_control(&pkt).expect("解码");
        assert_eq!(kind, TYPE_REG2);
        assert_eq!(got_fp, fp);
        assert_eq!(got_cert, &cert_der[..]);
        assert_eq!(got_sig, sig);
        assert_eq!(decode_reg_candidates_at(&pkt, off), cands);
        assert!(verify_signed_control(TYPE_REG2, &fp, &cert_der, &got_sig));
        // 截断到签名区之内 → None（尾部候选缺失不算截断，允许空尾巴）
        assert!(decode_signed_control(&pkt[..43 + cert_der.len()]).is_none());
        // 错误消息签名 → 验证失败（域分离防跨协议重放）
        let wrong_domain_sig = test_sign(TYPE_REG, &fp, &key);
        assert!(!verify_signed_control(TYPE_REG2, &fp, &cert_der, &wrong_domain_sig));
    }

    /// 签名 REG2 + legacy DIAL 全链路：注册按签名受理，拨号与地址尾巴、
    /// PUNCH_NOTIFY 均按签名会话工作。
    #[tokio::test]
    async fn 签名注册与拨号_全链路() -> anyhow::Result<()> {
        let relay = spawn("127.0.0.1:0".parse()?).await?;
        let (key, cert_der, fp) = test_identity("signed-e2e");
        let sig = test_sign(TYPE_REG2, &fp, &key);

        let agent = UdpSocket::bind("127.0.0.1:0").await?;
        let agent_local = agent.local_addr()?;
        let reg = encode_signed_control(TYPE_REG2, &fp, &cert_der, &sig, &[]);
        agent.send_to(&reg, relay).await?;
        wait_for(
            || {
                let mut buf = [0u8; 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from))
                        if from == relay
                            && decode_control_ack_tail(&buf[..n])
                                .map(|(s, _)| s == STATUS_OK)
                                .unwrap_or(false) =>
                    {
                        Some(())
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;

        // legacy DIAL 接入签名会话（DIAL 无签名，理由见模块注释「残余风险」）
        let dial = encode_control(TYPE_DIAL, &fp);
        let ctrl = UdpSocket::bind("127.0.0.1:0").await?;
        let ctrl_local = ctrl.local_addr()?;
        ctrl.send_to(&dial, relay).await?;
        let acked = wait_for(
            || {
                let mut buf = [0u8; 128];
                match ctrl.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => {
                        decode_control_ack_tail(&buf[..n]).filter(|(s, _)| *s == STATUS_OK)?;
                        Some(decode_dial_ack_agent_addrs(&buf[..n]))
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        assert_eq!(acked, vec![agent_local], "签名 DIAL ACK 带被控端映射地址");

        // 被控端收到 PUNCH_NOTIFY（发起方 = 签名 DIAL 来源）
        let (notify_fp, initiator) = wait_for(
            || {
                let mut buf = [0u8; 128];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => decode_punch_notify(&buf[..n]),
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        assert_eq!(notify_fp, fp);
        assert_eq!(initiator, ctrl_local);
        Ok(())
    }

    /// 签名会话不可被未认证报文降级（安全审计 F4 核心断言）：
    /// ① 坏签名 REG2 丢弃（无 ACK）；② legacy REG 静默忽略（无 ACK，
    /// fp 知晓者无法抢注/续命被控端槽位）；③ 换钥匙签的 REG2 也被拒
    /// （证书指纹与 fp 失配）。
    #[tokio::test]
    async fn 签名会话拒绝未认证降级() -> anyhow::Result<()> {
        let relay = spawn("127.0.0.1:0".parse()?).await?;
        let (key, cert_der, fp) = test_identity("downgrade-test");

        // ① 坏签名 REG2：无任何应答
        let bad_sig = [0u8; 64];
        let agent = UdpSocket::bind("127.0.0.1:0").await?;
        agent
            .send_to(&encode_signed_control(TYPE_REG2, &fp, &cert_der, &bad_sig, &[]), relay)
            .await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut buf = [0u8; 128];
        assert!(
            agent.try_recv_from(&mut buf).is_err(),
            "坏签名 REG 不得有应答"
        );

        // 正常签名注册建立签名会话
        let sig = test_sign(TYPE_REG2, &fp, &key);
        agent
            .send_to(&encode_signed_control(TYPE_REG2, &fp, &cert_der, &sig, &[]), relay)
            .await?;
        wait_for(
            || {
                let mut buf = [0u8; 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from))
                        if from == relay
                            && decode_control_ack_tail(&buf[..n])
                                .map(|(s, _)| s == STATUS_OK)
                                .unwrap_or(false) =>
                    {
                        Some(())
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;

        // ② legacy REG（fp 知晓者冒充续命）：静默忽略，无应答
        agent.send_to(&encode_control(TYPE_REG, &fp), relay).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut buf = [0u8; 128];
        assert!(agent.try_recv_from(&mut buf).is_err(), "legacy REG 不得有应答");

        // ③ 换一把钥匙签同一 fp：证书指纹与声明的 fp 失配 → 验签拒绝，无应答
        let (rogue_key, rogue_cert, _rogue_fp) = test_identity("rogue");
        let rogue_sig = test_sign(TYPE_REG2, &fp, &rogue_key);
        agent
            .send_to(&encode_signed_control(TYPE_REG2, &fp, &rogue_cert, &rogue_sig, &[]), relay)
            .await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut buf = [0u8; 128];
        assert!(agent.try_recv_from(&mut buf).is_err(), "冒名签名 REG 不得有应答");
        Ok(())
    }

    /// 单 IP 会话上限（安全审计 F4）：同 IP 超过 [`MAX_SESSIONS_PER_IP`] 个
    /// 注册即被拒（CONFLICT）。
    #[tokio::test]
    async fn 单ip注册超限被拒() -> anyhow::Result<()> {
        let relay = spawn("127.0.0.1:0".parse()?).await?;
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        let mut refused = 0usize;
        for i in 0..(MAX_SESSIONS_PER_IP + 4) {
            let (_key, cert_der, fp) = test_identity(&format!("flood-{i}"));
            let sig = test_sign(TYPE_REG2, &fp, &_key);
            sock.send_to(&encode_signed_control(TYPE_REG2, &fp, &cert_der, &sig, &[]), relay)
                .await?;
            let mut buf = [0u8; 64];
            let deadline = Instant::now() + Duration::from_secs(2);
            let status = loop {
                let n = sock.try_recv_from(&mut buf);
                match n {
                    Ok((n, from)) if from == relay => {
                        break decode_control_ack_tail(&buf[..n])
                            .map(|(s, _)| s)
                            .expect("REG ACK");
                    }
                    _ if Instant::now() > deadline => panic!("第 {i} 个注册无应答"),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            };
            if status == STATUS_CONFLICT {
                refused += 1;
            }
        }
        assert_eq!(refused, 4, "超限的 {refused} 个注册应全被拒");
        Ok(())
    }
    /// 候选地址管道（T-035）：REG 尾巴携带候选 → 会话存储 → DIAL ACK 返回
    /// `[中继处映射地址, 候选...]`；超出上限的候选被截断。
    #[tokio::test]
    async fn reg候选上报_dial_ack下发() -> anyhow::Result<()> {
        let relay = spawn("127.0.0.1:0".parse()?).await?;
        let fp = [7u8; 32];
        let cands: Vec<SocketAddr> = ["10.0.0.5:21118", "192.168.1.9:21118", "[fd00::5]:21118"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();

        let agent = UdpSocket::bind("127.0.0.1:0").await?;
        let agent_local = agent.local_addr()?;
        agent.send_to(&encode_reg(&fp, &cands), relay).await?;
        wait_for(
            || {
                let mut buf = [0u8; 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from))
                        if from == relay
                            && decode_control_ack_tail(&buf[..n])
                                .map(|(s, _)| s == STATUS_OK)
                                .unwrap_or(false) =>
                    {
                        Some(())
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;

        let ctrl = UdpSocket::bind("127.0.0.1:0").await?;
        ctrl.send_to(&encode_control(TYPE_DIAL, &fp), relay).await?;
        let acked = wait_for(
            || {
                let mut buf = [0u8; 256];
                match ctrl.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => {
                        decode_control_ack_tail(&buf[..n]).filter(|(s, _)| *s == STATUS_OK)?;
                        let v = decode_dial_ack_agent_addrs(&buf[..n]);
                        (!v.is_empty()).then_some(v)
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        // 首个 = 映射地址，其后 = 候选原序
        let mut expect = vec![agent_local];
        expect.extend(cands);
        assert_eq!(acked, expect, "DIAL ACK 应为 [映射地址, 候选...]");
        Ok(())
    }

    /// 打洞协调全链路（协议层）：REG → DIAL →
    /// ① DIAL ACK 尾巴携带被控端在中继处的地址；② 被控端收到 PUNCH_NOTIFY
    /// 且其中的发起方地址 = 控制端 socket 地址。
    #[tokio::test]
    async fn dial_带打洞协调_ack地址与punch通知() -> anyhow::Result<()> {
        let relay = spawn("127.0.0.1:0".parse()?).await?;
        let fp_agent = [7u8; 32];

        let agent = UdpSocket::bind("127.0.0.1:0").await?;
        let agent_local = agent.local_addr()?;
        agent
            .send_to(&encode_control(TYPE_REG, &fp_agent), relay)
            .await?;
        wait_for(
            || {
                let mut buf = [0u8; 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from))
                        if from == relay
                            && decode_control_ack_tail(&buf[..n])
                                .map(|(s, _)| s == STATUS_OK)
                                .unwrap_or(false) =>
                    {
                        Some(())
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;

        let ctrl = UdpSocket::bind("127.0.0.1:0").await?;
        let ctrl_local = ctrl.local_addr()?;
        ctrl.send_to(&encode_control(TYPE_DIAL, &fp_agent), relay)
            .await?;
        // ① ACK 带被控端地址列表（无候选上报时仅含映射地址 = agent 本机地址）
        let acked_agent_addrs = wait_for(
            || {
                let mut buf = [0u8; 128];
                match ctrl.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => {
                        decode_control_ack_tail(&buf[..n]).filter(|(s, _)| *s == STATUS_OK)?;
                        let v = decode_dial_ack_agent_addrs(&buf[..n]);
                        (!v.is_empty()).then_some(v)
                    }
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        assert_eq!(
            acked_agent_addrs,
            vec![agent_local],
            "ACK 首个应是被控端映射地址"
        );

        // ② agent 收到 PUNCH_NOTIFY，发起方地址 = ctrl 本机地址
        let (fp, initiator) = wait_for(
            || {
                let mut buf = [0u8; 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => decode_punch_notify(&buf[..n]),
                    _ => None,
                }
            },
            Duration::from_secs(3),
        )
        .await?;
        assert_eq!(fp, fp_agent);
        assert_eq!(initiator, ctrl_local, "PUNCH_NOTIFY 应携带控制端地址");
        Ok(())
    }
}
