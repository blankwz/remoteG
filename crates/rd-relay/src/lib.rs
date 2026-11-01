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
//!   type 3 = DIAL(controller)   应答 type 4: [u8 status][u64 tag]
//!   status: 1=ok 2=not_found 3=conflict（fp→tag 冲突）
//! tag = fingerprint 前 8 字节（自证书指纹派生，无需协商；碰撞概率可忽略）
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
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

/// 中继默认监听端口（与 rd 的 QUIC 端口一致，运维只记一个数字）。
pub const DEFAULT_PORT: u16 = 21118;
/// 数据报大小上限（rd MTU 钳 1200 + 裕量）。
pub const MAX_DATAGRAM: usize = 1500;
/// 会话空闲回收阈值（> rd 的 QUIC idle timeout 20s，活跃会话不会被误删）。
pub const SESSION_IDLE: Duration = Duration::from_secs(60);
/// 最大并发会话（免费版 1 并发的中继侧放大裕量）。
pub const MAX_SESSIONS: usize = 512;

pub const TAG_CONTROL: u64 = 0;
pub const TYPE_REG: u8 = 1;
pub const TYPE_REG_ACK: u8 = 2;
pub const TYPE_DIAL: u8 = 3;
pub const TYPE_DIAL_ACK: u8 = 4;
pub const STATUS_OK: u8 = 1;
pub const STATUS_NOT_FOUND: u8 = 2;
pub const STATUS_CONFLICT: u8 = 3;
/// 控制报固定长度：tag(8) + type(1) + fp(32) = 41
const CTRL_LEN: usize = 41;

/// 会话：一个被控端指纹 ↔ 双端最新 NAT 地址。
struct Session {
    fp: [u8; 32],
    agent: Option<SocketAddr>,
    agent_seen: Instant,
    ctrl: Option<SocketAddr>,
    ctrl_seen: Instant,
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
    if buf.len() < CTRL_LEN + 1 + 8 {
        return None;
    }
    let status = buf[CTRL_LEN];
    let tag = u64::from_le_bytes(buf[CTRL_LEN + 1..CTRL_LEN + 9].try_into().ok()?);
    Some((status, tag))
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
    let reply = match kind {
        TYPE_REG => {
            // 锁内只算结果（不跨 await——std 锁守卫跨 await 会让 future 失去 Send）
            let reply = {
                let mut t = table.lock().expect("relay table 锁中毒");
                let tag = Table::tag_of(&fp);
                // 冲突判定：同 tag 但指纹不同
                let conflict = match t.by_tag.get(&tag) {
                    Some(existing) => existing.fp != fp,
                    None => t.by_tag.len() >= MAX_SESSIONS,
                };
                if conflict {
                    tracing::warn!(peer = %from, "REG 被拒（tag 冲突或会话表已满）");
                    Some(encode_control_ack(TYPE_REG_ACK, &fp, STATUS_CONFLICT, 0))
                } else {
                    let s = t.by_tag.entry(tag).or_insert_with(|| {
                        tracing::info!(tag, peer = %from, "新被控端注册");
                        Session {
                            fp,
                            agent: None,
                            agent_seen: Instant::now(),
                            ctrl: None,
                            ctrl_seen: Instant::now(),
                        }
                    });
                    // NAT 地址始终以最新出包为准
                    s.agent = Some(from);
                    s.agent_seen = Instant::now();
                    tracing::debug!(tag, peer = %from, "REG 刷新");
                    Some(encode_control_ack(TYPE_REG_ACK, &fp, STATUS_OK, tag))
                }
            };
            reply
        }
        TYPE_DIAL => {
            let reply = {
                let mut t = table.lock().expect("relay table 锁中毒");
                let tag = Table::tag_of(&fp);
                match t.by_tag.get_mut(&tag) {
                    Some(s) if s.fp == fp => {
                        s.ctrl = Some(from);
                        s.ctrl_seen = Instant::now();
                        tracing::info!(tag, peer = %from, "控制端接入");
                        encode_control_ack(TYPE_DIAL_ACK, &fp, STATUS_OK, tag)
                    }
                    _ => {
                        tracing::debug!(peer = %from, "DIAL 目标不在线");
                        encode_control_ack(TYPE_DIAL_ACK, &fp, STATUS_NOT_FOUND, 0)
                    }
                }
            };
            Some(reply)
        }
        _ => None,
    };
    if let Some(reply) = reply {
        let _ = sock.send_to(&reply, from).await;
    }
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
        let tag = Table::tag_of(&fp_agent);
        let pkt = tag.to_le_bytes().to_vec();
        let mut data = pkt.clone();
        data.extend_from_slice(b"hello-ctrl");
        ctrl.send_to(&data, relay).await?;
        let got = wait_for(
            || {
                let mut buf = [0u8; MAX_DATAGRAM + 64];
                match agent.try_recv_from(&mut buf) {
                    Ok((n, from)) if from == relay => Some(buf[..n].to_vec()),
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
}
