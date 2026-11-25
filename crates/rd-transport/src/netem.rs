//! 可编程弱网故障注入阀门（RD Geek Lab Netem Shaper）。
//!
//! 直接串联在 `rd-transport` 的真实 UDP/QUIC 数据收发路径上：
//! - 默认 `enabled = false` 时为纯原子读 Bypass（0 分配、0 锁、0 延迟开销）；
//! - 开启后支持实时毫秒级拨动：
//!   - `delay_ms` / `jitter_ms`：基础单向延迟与均匀抖动
//!   - `loss_pct`：随机丢包率（0.0 ~ 50.0%）
//!   - `burst_len`：Gilbert-Elliott 连续突发丢包长度（1 ~ 20 包）
//!   - `rate_kbps`：令牌桶带宽闸门（0 = 不限速）
//! - 实时统计累计注入丢包数、延迟包数与限速等待次数，供实验室示波器直读。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

/// 弱网阀门配置（前后端 JSON 直映）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetemConfig {
    pub enabled: bool,
    /// 附加单向延迟 (0 ~ 1000 ms)
    pub delay_ms: u32,
    /// 延迟抖动范围 (±0 ~ ±200 ms)
    pub jitter_ms: u32,
    /// 丢包率百分比 (0.0 ~ 50.0, 内部按千分比存储)
    pub loss_pct: f32,
    /// 每次触发丢包时的连续丢包数 (1 ~ 20)
    pub burst_len: u32,
    /// 带宽限速 (kbps, 0 = 无限制)
    pub rate_kbps: u32,
}

impl Default for NetemConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            delay_ms: 0,
            jitter_ms: 0,
            loss_pct: 0.0,
            burst_len: 1,
            rate_kbps: 0,
        }
    }
}

/// 弱网阀门实时运行统计。
#[derive(Clone, Copy, Debug, Default)]
pub struct NetemStats {
    pub total_pkts: u64,
    pub dropped_pkts: u64,
    pub delayed_pkts: u64,
    pub throttled_bytes: u64,
}

struct GlobalNetem {
    enabled: AtomicBool,
    delay_ms: AtomicU32,
    jitter_ms: AtomicU32,
    /// 千分比 (0 ~ 500 = 0.0% ~ 50.0%)
    loss_permille: AtomicU32,
    burst_len: AtomicU32,
    rate_kbps: AtomicU32,
    /// true = BBR; false = Cubic
    use_bbr: AtomicBool,
    /// 0=auto, 1=direct, 2=punch, 3=relay, 4=easytier
    force_route: AtomicU32,
    // 内部状态与统计
    burst_remaining: AtomicU32,
    prng_state: AtomicU64,
    total_pkts: AtomicU64,
    dropped_pkts: AtomicU64,
    delayed_pkts: AtomicU64,
    throttled_bytes: AtomicU64,
}

static NETEM: OnceLock<GlobalNetem> = OnceLock::new();

fn state() -> &'static GlobalNetem {
    NETEM.get_or_init(|| GlobalNetem {
        enabled: AtomicBool::new(false),
        delay_ms: AtomicU32::new(0),
        jitter_ms: AtomicU32::new(0),
        loss_permille: AtomicU32::new(0),
        burst_len: AtomicU32::new(1),
        rate_kbps: AtomicU32::new(0),
        use_bbr: AtomicBool::new(true),
        force_route: AtomicU32::new(0),
        burst_remaining: AtomicU32::new(0),
        prng_state: AtomicU64::new(0x853c_49e6_748f_ea9b),
        total_pkts: AtomicU64::new(0),
        dropped_pkts: AtomicU64::new(0),
        delayed_pkts: AtomicU64::new(0),
        throttled_bytes: AtomicU64::new(0),
    })
}

/// 设置 QUIC 拥塞控制策略（true = BBR, false = Cubic）。
pub fn set_use_bbr(bbr: bool) {
    state().use_bbr.store(bbr, Ordering::Relaxed);
}

/// 读取当前是否使用 BBR 拥塞控制。
pub fn get_use_bbr() -> bool {
    state().use_bbr.load(Ordering::Relaxed)
}

/// 设置选路强制模式（"auto" | "direct" | "punch" | "relay" | "easytier"）。
pub fn set_force_route(mode: &str) {
    let code = match mode {
        "direct" => 1,
        "punch" => 2,
        "relay" => 3,
        "easytier" => 4,
        _ => 0,
    };
    state().force_route.store(code, Ordering::Relaxed);
}

/// 读取当前选路强制模式。
pub fn get_force_route() -> &'static str {
    match state().force_route.load(Ordering::Relaxed) {
        1 => "direct",
        2 => "punch",
        3 => "relay",
        4 => "easytier",
        _ => "auto",
    }
}

/// Xorshift64* 极速无锁伪随机。
fn next_rand_u32() -> u32 {
    let st = state();
    let mut x = st.prng_state.load(Ordering::Relaxed);
    if x == 0 {
        x = 0x853c_49e6_748f_ea9b;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    st.prng_state.store(x, Ordering::Relaxed);
    ((x.wrapping_mul(0x2545_f491_4f6c_dd1d)) >> 32) as u32
}

/// 更新全局 Netem 弱网阀门参数（毫秒级即时生效）。
pub fn set_netem_config(cfg: NetemConfig) {
    let st = state();
    st.delay_ms.store(cfg.delay_ms.min(1000), Ordering::Relaxed);
    st.jitter_ms
        .store(cfg.jitter_ms.min(300), Ordering::Relaxed);
    let permille = (cfg.loss_pct.clamp(0.0, 50.0) * 10.0).round() as u32;
    st.loss_permille.store(permille, Ordering::Relaxed);
    st.burst_len
        .store(cfg.burst_len.clamp(1, 20), Ordering::Relaxed);
    st.rate_kbps
        .store(cfg.rate_kbps.min(1_000_000), Ordering::Relaxed);
    if !cfg.enabled {
        st.burst_remaining.store(0, Ordering::Relaxed);
    }
    st.enabled.store(cfg.enabled, Ordering::Release);
}

/// 读取当前全局 Netem 配置。
pub fn get_netem_config() -> NetemConfig {
    let st = state();
    NetemConfig {
        enabled: st.enabled.load(Ordering::Acquire),
        delay_ms: st.delay_ms.load(Ordering::Relaxed),
        jitter_ms: st.jitter_ms.load(Ordering::Relaxed),
        loss_pct: st.loss_permille.load(Ordering::Relaxed) as f32 / 10.0,
        burst_len: st.burst_len.load(Ordering::Relaxed),
        rate_kbps: st.rate_kbps.load(Ordering::Relaxed),
    }
}

/// 读取并可选重置 Netem 统计。
pub fn get_netem_stats() -> NetemStats {
    let st = state();
    NetemStats {
        total_pkts: st.total_pkts.load(Ordering::Relaxed),
        dropped_pkts: st.dropped_pkts.load(Ordering::Relaxed),
        delayed_pkts: st.delayed_pkts.load(Ordering::Relaxed),
        throttled_bytes: st.throttled_bytes.load(Ordering::Relaxed),
    }
}

/// 一键重置 Netem 为关闭态（Panic Reset 逃生舱调用）。
pub fn reset_netem() {
    set_netem_config(NetemConfig::default());
    let st = state();
    st.total_pkts.store(0, Ordering::Relaxed);
    st.dropped_pkts.store(0, Ordering::Relaxed);
    st.delayed_pkts.store(0, Ordering::Relaxed);
    st.throttled_bytes.store(0, Ordering::Relaxed);
}

/// 对一个不可靠数据报（QUIC Datagram / FEC 分片）判定是否按 Netem 规则丢弃。
/// 返回 `true` 表示该包被弱网阀门吞掉。
#[inline]
pub fn should_drop_dgram() -> bool {
    let st = state();
    if !st.enabled.load(Ordering::Relaxed) {
        return false;
    }
    st.total_pkts.fetch_add(1, Ordering::Relaxed);

    // 1) 检查是否处于突发丢包序列中
    let rem = st.burst_remaining.load(Ordering::Relaxed);
    if rem > 0 {
        st.burst_remaining
            .store(rem.saturating_sub(1), Ordering::Relaxed);
        st.dropped_pkts.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    // 2) 随机抽样触发新丢包
    let permille = st.loss_permille.load(Ordering::Relaxed);
    if permille == 0 {
        return false;
    }
    let r = next_rand_u32() % 1000;
    if r < permille {
        let burst = st.burst_len.load(Ordering::Relaxed).max(1);
        if burst > 1 {
            st.burst_remaining.store(burst - 1, Ordering::Relaxed);
        }
        st.dropped_pkts.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    false
}

/// 计算当前帧/数据包在 Netem 下应注入的附加延迟（基础延迟 + 抖动 + 带宽串行化耗时）。
#[inline]
pub fn compute_frame_delay(bytes: usize) -> Option<Duration> {
    let st = state();
    if !st.enabled.load(Ordering::Relaxed) {
        return None;
    }
    let base = st.delay_ms.load(Ordering::Relaxed);
    let jitter = st.jitter_ms.load(Ordering::Relaxed);
    let rate_kbps = st.rate_kbps.load(Ordering::Relaxed);

    let mut total_us = u64::from(base) * 1000;
    if jitter > 0 {
        let span = u64::from(jitter) * 2000;
        let offset = (u64::from(next_rand_u32()) % span).saturating_sub(u64::from(jitter) * 1000);
        total_us = total_us.saturating_add(offset);
    }
    if rate_kbps > 0 {
        // 串行化传输耗时 = bytes * 8 * 1000 / rate_kbps (微秒)
        let serial_us = (bytes as u64).saturating_mul(8_000) / u64::from(rate_kbps.max(1));
        total_us = total_us.saturating_add(serial_us);
        st.throttled_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
    if total_us > 0 {
        st.delayed_pkts.fetch_add(1, Ordering::Relaxed);
        Some(Duration::from_micros(total_us.min(3_000_000)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netem_bypass_and_loss_work() {
        reset_netem();
        assert!(!should_drop_dgram());
        assert!(compute_frame_delay(1200).is_none());

        set_netem_config(NetemConfig {
            enabled: true,
            delay_ms: 40,
            jitter_ms: 10,
            loss_pct: 50.0,
            burst_len: 3,
            rate_kbps: 2000,
        });
        let d = compute_frame_delay(1000).expect("delay enabled");
        assert!(d.as_millis() >= 30);

        let mut dropped = 0;
        for _ in 0..100 {
            if should_drop_dgram() {
                dropped += 1;
            }
        }
        assert!(dropped > 10, "50% 丢包率应显著丢包, 实际 {dropped}");
        reset_netem();
    }
}
