//! E2 自扰静默期（snug ConnectionTracker 的取 max 静默窗语义重写）。
//!
//! 凡本进程引起的窗口/显示扰动（隐私屏黑窗创建/销毁、未来虚拟屏换档）先
//! 登记静默窗，静默期内相关事件不外推（控制端列表晚一点刷新即可，不出错）。
//! 取 max 语义 derived from zephyr4123/snug (MIT)，Sources/SnugCore/Session/
//! ConnectionTracker.swift:31-35：连续扰动叠加时静默窗延后到最晚的扰动点。

/// 连续扰动静默窗（秒；snug ConnectionTracker.swift:19 quietWindow 实测值）。
pub const QUIET_WINDOW: f64 = 5.0;
/// 登记去抖：上次登记这么久内的再次登记不延长静默窗——事件风暴下的重复
/// 登记不无限拖延（snug ConnectionTracker.swift:18 disconnectDebounce）。
pub const NOTE_DEBOUNCE: f64 = 1.5;

/// 本进程引起的扰动静默窗。
#[derive(Debug, Default, Clone)]
pub struct QuietWindow {
    /// 静默截止时刻（单调秒）
    until: f64,
    /// 上次登记时刻（去抖用）
    last_note: f64,
}

impl QuietWindow {
    /// 登记一次自扰：静默窗延到 `now + QUIET_WINDOW`（与已有窗取 max——
    /// 连续扰动不叠加延长，只延后到最晚扰动点）。
    pub fn note(&mut self, now: f64) {
        if now - self.last_note < NOTE_DEBOUNCE {
            return; // 去抖：风暴中重复登记不无限拖延
        }
        self.last_note = now;
        self.until = self.until.max(now + QUIET_WINDOW);
    }

    /// 是否仍在静默期内。
    pub fn is_quiet(&self, now: f64) -> bool {
        now < self.until
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_takes_max_and_debounces() {
        let mut q = QuietWindow::default();
        assert!(!q.is_quiet(0.0));
        q.note(10.0);
        assert!(q.is_quiet(12.0), "静默到 15.0");
        assert!(!q.is_quiet(15.0), "恰好 15.0 已过期");
        // 取 max：11s 的登记在 10s 登记（静默到 15s）之后，12.5s 截止 < 15s，取 max
        q.note(11.0);
        assert!(q.is_quiet(14.9));
        assert!(!q.is_quiet(15.1), "取 max 不延长到 16s");
        // 去抖：10.5s 的登记（距上次 0.5s < 1.5s）被忽略，静默窗不动
        q.note(10.5);
        assert!(!q.is_quiet(15.1));
        // 风暴后长期安静再登记 → 新窗
        q.note(20.0);
        assert!(q.is_quiet(24.0));
    }
}
