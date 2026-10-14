//! 配对：6 位一次性配对码 + SPAKE2（Ed25519）+ HMAC-SHA256 指纹绑定（US-06，T-013）。
//!
//! # 协议（4 步，复用 PairRequest/PairResult 的 `spake2_msg` 字段承载）
//!
//! 线格式统一为信封 `[ver=1][tag][payload]`：tag `0x01` = SPAKE2 消息，`0x02` = HMAC 标签。
//!
//! 1. 控制端 `start(code, fp_c, fp_a)` → `SPAKE2::start_a`（identity A=`"rd-controller"`，
//!    B=`"rd-agent"`，密码=配对码），发送第 1 轮 SPAKE2 消息；
//! 2. Agent `on_message`（`start_b` + `finish` 求出共享 key）→ 回送第 2 轮 SPAKE2 消息；
//! 3. 控制端 `finish` 求出 key → 发送 `MAC_c = HMAC(key, fp_c ‖ fp_a)`；
//! 4. Agent 校验 `MAC_c`（防错码/中间人）→ 回送 `MAC_a = HMAC(key, fp_a ‖ fp_c)`；
//!    控制端校验 `MAC_a` 后 `Done`。
//!
//! SPAKE2 本身不检测错码（双方只是派生出不同的 key），因此**第 3/4 步的 HMAC 确认
//! 是唯一的码正确性判定**，同时把 key 绑定到双方证书指纹（防中间人替换指纹）。
//!
//! # 安全约束
//! - 配对码：CSPRNG 生成、6 位数字、5 分钟有效、一次有效；`new_code` 作废旧码；
//! - 失败锁定：Agent 侧全局计数，5 次失败锁定 10 分钟；
//! - 本模块**不得**打印配对码与任何密钥材料（key/MAC/SPAKE2 状态）到日志。

use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use rand::Rng;
use rd_transport::Fingerprint;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};

/// 配对码有效期。
const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// 触发锁定的失败次数。
const MAX_FAILURES: u32 = 5;
/// 锁定时长。
const LOCK_DURATION: Duration = Duration::from_secs(10 * 60);
/// 配对码位数。
const CODE_LEN: usize = 6;
/// 消息信封版本。
const ENV_VERSION: u8 = 1;
/// 信封 tag：SPAKE2 消息。
const TAG_SPAKE2: u8 = 0x01;
/// 信封 tag：HMAC-SHA256 确认标签。
const TAG_MAC: u8 = 0x02;
/// HMAC-SHA256 输出长度。
const MAC_LEN: usize = 32;
/// SPAKE2 identity A（控制端）。
const ID_A: &[u8] = b"rd-controller";
/// SPAKE2 identity B（Agent）。
const ID_B: &[u8] = b"rd-agent";

type HmacSha256 = Hmac<Sha256>;

/// 配对结果：回送一条消息、完成或失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairStep {
    /// 发送这条消息（第 1/2 轮为 SPAKE2 消息，第 3/4 轮为 MAC 标签）。
    Send(Vec<u8>),
    /// 配对完成（双方均确认了码与指纹）。
    Done,
    /// 配对失败。
    Failed(PairError),
}

/// 配对失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// 配对码错误，或 MAC 校验不通过（含指纹被替换）。
    WrongCode,
    /// 配对码不存在、已过期或已被使用。
    Expired,
    /// 失败次数过多，锁定至该时刻。
    Locked { until: Instant },
    /// 消息格式/协议顺序错误。
    Protocol,
}

// ---------------------------------------------------------------------------
// Agent 侧（被控端，SPAKE2 角色 B）
// ---------------------------------------------------------------------------

/// Agent 配对状态机。由 Agent 进程持有**唯一实例**（失败计数全局）。
pub struct AgentPairing {
    code: Option<CodeEntry>,
    pending: Option<AgentPending>,
    fail_count: u32,
    locked_until: Option<Instant>,
}

struct CodeEntry {
    value: String,
    expires: Instant,
}

/// 第 1 轮之后挂起的握手上下文（key 为密钥材料，禁止日志输出）。
#[derive(Debug)]
struct AgentPending {
    key: Vec<u8>,
    local_fp: Fingerprint,
    peer_fp: Fingerprint,
}

impl Default for AgentPairing {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentPairing {
    pub fn new() -> Self {
        Self {
            code: None,
            pending: None,
            fail_count: 0,
            locked_until: None,
        }
    }

    /// 生成新的 6 位配对码（CSPRNG，允许前导零），旧码与进行中的握手立即作废。
    /// 不重置失败计数/锁定状态。
    pub fn new_code(&mut self) -> String {
        let value = rand::thread_rng().gen_range(0..10u32.pow(CODE_LEN as u32));
        let value = format!("{value:0width$}", width = CODE_LEN);
        self.code = Some(CodeEntry {
            expires: Instant::now() + CODE_TTL,
            value: value.clone(),
        });
        self.pending = None;
        value
    }

    /// 仅测试用：把失败锁定的截止时间拨到当前时刻（模拟 10 分钟到期）。
    #[doc(hidden)]
    pub fn __force_unlock_for_tests(&mut self) {
        self.locked_until = Some(Instant::now());
    }

    /// 当前有效配对码与到期时刻；已过期返回 `None`。
    pub fn current_code(&self) -> Option<(String, Instant)> {
        let code = self.code.as_ref()?;
        if Instant::now() >= code.expires {
            return None;
        }
        Some((code.value.clone(), code.expires))
    }

    /// 立即作废当前配对码（托盘“作废/刷新”）。不影响失败锁定状态。
    pub fn expire_code(&mut self) {
        if let Some(code) = self.code.as_mut() {
            code.expires = Instant::now();
        }
        self.pending = None;
    }

    /// 处理对端一条消息。`local_fp`/`peer_fp` 为本端与对端 TLS 证书指纹（传输层实测值）。
    pub fn on_message(
        &mut self,
        msg: &[u8],
        local_fp: &Fingerprint,
        peer_fp: &Fingerprint,
    ) -> PairStep {
        // 锁定最优先：锁定期间不做任何密码学运算
        if let Some(until) = self.locked_until {
            if Instant::now() < until {
                return PairStep::Failed(PairError::Locked { until });
            }
            self.locked_until = None;
        }

        let Some((tag, payload)) = parse_envelope(msg) else {
            return self.fail(PairError::Protocol);
        };
        match tag {
            TAG_SPAKE2 => self.on_spake2(payload, local_fp, peer_fp),
            TAG_MAC => self.on_peer_mac(payload),
            _ => self.fail(PairError::Protocol),
        }
    }

    /// 第 1 轮：对端的 SPAKE2 消息。求出共享 key 并挂起，回送自己的 SPAKE2 消息。
    fn on_spake2(
        &mut self,
        payload: &[u8],
        local_fp: &Fingerprint,
        peer_fp: &Fingerprint,
    ) -> PairStep {
        let Some(code) = self.valid_code() else {
            self.code = None;
            return PairStep::Failed(PairError::Expired);
        };

        let (state, outbound) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &SpakeIdentity::new(ID_A),
            &SpakeIdentity::new(ID_B),
        );
        // 对端消息损坏/角色不符：SPAKE2 派生失败（错码要到 MAC 阶段才暴露）
        let key = match state.finish(payload) {
            Ok(key) => key,
            Err(_) => return self.fail(PairError::Protocol),
        };
        self.pending = Some(AgentPending {
            key,
            local_fp: *local_fp,
            peer_fp: *peer_fp,
        });
        PairStep::Send(envelope(TAG_SPAKE2, &outbound))
    }

    /// 第 3 轮：对端的 MAC。通过则视为码正确、指纹一致，作废配对码并回送自己的 MAC。
    fn on_peer_mac(&mut self, payload: &[u8]) -> PairStep {
        if payload.len() != MAC_LEN {
            return self.fail(PairError::Protocol);
        }
        let Some(pending) = self.pending.take() else {
            return self.fail(PairError::Protocol);
        };
        // 对端 MAC = HMAC(key, 对端本端指纹 ‖ 对端眼中的对端) = HMAC(key, pending.peer_fp ‖ pending.local_fp)
        if !verify_mac(&pending.key, &pending.peer_fp, &pending.local_fp, payload) {
            return self.fail(PairError::WrongCode);
        }
        self.code = None; // 一次性：验证通过即作废
        let mac = compute_mac(&pending.key, &pending.local_fp, &pending.peer_fp);
        PairStep::Send(envelope(TAG_MAC, &mac))
    }

    fn valid_code(&self) -> Option<String> {
        let code = self.code.as_ref()?;
        (Instant::now() < code.expires).then(|| code.value.clone())
    }

    /// 记一次失败；满 5 次锁定 10 分钟并清零计数。
    fn fail(&mut self, err: PairError) -> PairStep {
        self.pending = None;
        self.fail_count += 1;
        if self.fail_count >= MAX_FAILURES {
            self.locked_until = Some(Instant::now() + LOCK_DURATION);
            self.fail_count = 0;
        }
        PairStep::Failed(err)
    }
}

// ---------------------------------------------------------------------------
// 控制端（SPAKE2 角色 A）
// ---------------------------------------------------------------------------

/// 控制端配对状态机。
pub struct ControllerPairing {
    phase: Option<ControllerPhase>,
}

enum ControllerPhase {
    /// 已发送第 1 轮，等待 Agent 的 SPAKE2 消息。
    AwaitingAgentSpake2 {
        state: Spake2<Ed25519Group>,
        local_fp: Fingerprint,
        peer_fp: Fingerprint,
    },
    /// 已发送自己的 MAC，等待 Agent 的 MAC。
    AwaitingAgentMac {
        key: Vec<u8>,
        local_fp: Fingerprint,
        peer_fp: Fingerprint,
    },
}

impl ControllerPairing {
    /// 用配对码发起配对，返回状态机与第 1 条待发送消息。
    pub fn start(code: &str, local_fp: &Fingerprint, peer_fp: &Fingerprint) -> (Self, Vec<u8>) {
        let (state, outbound) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &SpakeIdentity::new(ID_A),
            &SpakeIdentity::new(ID_B),
        );
        (
            Self {
                phase: Some(ControllerPhase::AwaitingAgentSpake2 {
                    state,
                    local_fp: *local_fp,
                    peer_fp: *peer_fp,
                }),
            },
            envelope(TAG_SPAKE2, &outbound),
        )
    }

    /// 处理 Agent 的一条消息。
    pub fn on_message(&mut self, msg: &[u8]) -> PairStep {
        let Some((tag, payload)) = parse_envelope(msg) else {
            return PairStep::Failed(PairError::Protocol);
        };
        // 每条消息消耗当前阶段状态（协议一次性）
        let Some(phase) = self.phase.take() else {
            return PairStep::Failed(PairError::Protocol);
        };
        match (phase, tag) {
            (
                ControllerPhase::AwaitingAgentSpake2 {
                    state,
                    local_fp,
                    peer_fp,
                },
                TAG_SPAKE2,
            ) => {
                let key = match state.finish(payload) {
                    Ok(key) => key,
                    Err(_) => return PairStep::Failed(PairError::Protocol),
                };
                let mac = compute_mac(&key, &local_fp, &peer_fp);
                self.phase = Some(ControllerPhase::AwaitingAgentMac {
                    key,
                    local_fp,
                    peer_fp,
                });
                PairStep::Send(envelope(TAG_MAC, &mac))
            }
            (
                ControllerPhase::AwaitingAgentMac {
                    key,
                    local_fp,
                    peer_fp,
                },
                TAG_MAC,
            ) => {
                if payload.len() != MAC_LEN {
                    return PairStep::Failed(PairError::Protocol);
                }
                // Agent 的 MAC = HMAC(key, agent 本端 ‖ agent 眼中的对端) = HMAC(key, ctrl.peer_fp ‖ ctrl.local_fp)
                if verify_mac(&key, &peer_fp, &local_fp, payload) {
                    PairStep::Done
                } else {
                    PairStep::Failed(PairError::WrongCode)
                }
            }
            _ => PairStep::Failed(PairError::Protocol),
        }
    }
}

// ---------------------------------------------------------------------------
// 消息封装与 MAC
// ---------------------------------------------------------------------------

/// 信封：`[ver][tag][payload]`。
fn envelope(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + payload.len());
    out.push(ENV_VERSION);
    out.push(tag);
    out.extend_from_slice(payload);
    out
}

/// 解析信封，返回 `(tag, payload)`；版本不符或为空按垃圾消息处理。
fn parse_envelope(msg: &[u8]) -> Option<(u8, &[u8])> {
    let (ver, rest) = msg.split_first()?;
    if *ver != ENV_VERSION {
        return None;
    }
    let (tag, payload) = rest.split_first()?;
    if payload.is_empty() {
        return None;
    }
    Some((*tag, payload))
}

/// `HMAC-SHA256(key, own ‖ peer)`，指纹用 32 字节原始值。
fn compute_mac(key: &[u8], own: &Fingerprint, peer: &Fingerprint) -> [u8; MAC_LEN] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(&own.0);
    mac.update(&peer.0);
    mac.finalize().into_bytes().into()
}

/// 常量时间校验 `HMAC-SHA256(key, own ‖ peer) == tag`。
fn verify_mac(key: &[u8], own: &Fingerprint, peer: &Fingerprint, tag: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(&own.0);
    mac.update(&peer.0);
    mac.verify_slice(tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_six_digits_and_invalidates_old() {
        let mut agent = AgentPairing::new();
        assert!(agent.current_code().is_none());
        let a = agent.new_code();
        assert_eq!(a.len(), CODE_LEN);
        assert!(a.bytes().all(|b| b.is_ascii_digit()));
        assert!(a.bytes().all(|b| b.is_ascii_digit()));
        let (_, expires) = agent.current_code().expect("有码");
        assert!(expires > Instant::now());
        let _b = agent.new_code();
        // 旧码作废：current_code 反映的是新码
        let (current, _) = agent.current_code().expect("有码");
        assert_ne!(a, current);
    }

    #[test]
    fn expire_code_makes_it_unusable() {
        let mut agent = AgentPairing::new();
        let code = agent.new_code();
        agent.expire_code();
        assert!(agent.current_code().is_none());
        let _ = code;
    }

    #[test]
    fn envelope_roundtrip_and_garbage() {
        let msg = envelope(TAG_SPAKE2, &[1, 2, 3]);
        assert_eq!(parse_envelope(&msg), Some((TAG_SPAKE2, &[1u8, 2, 3][..])));
        assert_eq!(parse_envelope(&[]), None);
        assert_eq!(parse_envelope(&[9, 1, 1]), None); // 版本不符
        assert_eq!(parse_envelope(&[1, 2]), None); // 空 payload
    }

    #[test]
    fn mac_binds_fingerprint_order() {
        let key = [7u8; 32];
        let a = Fingerprint([1; 32]);
        let b = Fingerprint([2; 32]);
        let mac = compute_mac(&key, &a, &b);
        assert!(verify_mac(&key, &a, &b, &mac));
        // 方向颠倒即不匹配
        assert!(!verify_mac(&key, &b, &a, &mac));
        // 换密钥即不匹配
        assert!(!verify_mac(&key, &a, &b, &[0u8; 32]));
    }
}
