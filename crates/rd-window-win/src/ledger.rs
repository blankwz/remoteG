//! E1 窗口操作记账还原：改动前写账（原子写 JSON）→ 复原成功销账 → 启动时
//! recover()。
//!
//! 动机（snug PhoneDesk 头注释语义）：会话内存态快照在崩溃/断电时丢失，窗
//! 口被远程会话调整后回不去。账本落盘后，崩溃重启也能把窗口放回去。
//! 原子写模式 derived from zephyr4123/snug (MIT)，Sources/SnugCore/Persistence/
//! JSONFile.swift:35（写临时文件 → rename 替换）。
//!
//! 纪律：账本只记会话引起的窗口调整（record 于调整前）；复原成功即销账
//! （clear），未销账条目 = 未复原的调整，启动时 recover() 逐条还原。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 账本条目：一次窗口调整的快照（调整**前**的状态）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// 窗口句柄（isize 十进制）
    pub hwnd: isize,
    /// showCmd（1 普通 / 2 最小化 / 3 最大化）
    pub show_cmd: u32,
    /// 还原矩形 [left, top, right, bottom]（屏幕物理像素）
    pub normal: [i32; 4],
    /// 引起调整的原因（"remote-session" / "privacy-on" 等，诊断用）
    pub reason: String,
    /// 记账时间（Unix 秒）
    pub at_unix: u64,
}

/// 窗口操作账本（`<dir>/rd-window-ledger.json`）。
#[derive(Debug, Clone)]
pub struct OperationLedger {
    path: PathBuf,
}

impl OperationLedger {
    /// 打开（惰性创建文件）。
    pub fn open(dir: &Path) -> Self {
        Self {
            path: dir.join("rd-window-ledger.json"),
        }
    }

    /// 改动前写账：记下该窗口当前布局，原子写落盘。同一窗口重复记账 = 覆盖
    /// 旧条目（还原取这一条即可：恢复后窗口即回到本次调整前的状态）。
    pub fn record(
        &self,
        hwnd: isize,
        reason: &str,
        p: &super::resize::SavedPlacement,
    ) -> Result<()> {
        let entry = LedgerEntry {
            hwnd,
            show_cmd: p.show_cmd(),
            normal: p.normal_rect_ltrb(),
            reason: reason.to_string(),
            at_unix: unix_now(),
        };
        let mut entries = self.read_all()?;
        entries.retain(|e| e.hwnd != hwnd);
        entries.push(entry);
        self.write_all(&entries)
    }

    /// 复原成功销账：删该窗口条目（无条目/读失败时静默——销账失败只影响
    /// 下次启动多做一次无害还原）。
    pub fn clear(&self, hwnd: isize) {
        if let Ok(mut entries) = self.read_all() {
            entries.retain(|e| e.hwnd != hwnd);
            let _ = self.write_all(&entries);
        }
    }

    /// 启动时恢复：读账本逐条还原（SetWindowPlacement），全清。返回条数。
    /// 句柄在新进程里可能失效（窗口已关）——按句柄值尝试，失败跳过并记日志。
    #[cfg(windows)]
    pub fn recover(&self) -> Result<usize> {
        let entries = self.read_all()?;
        let n = entries.len();
        for e in &entries {
            let p = super::resize::SavedPlacement::from_parts(e.show_cmd, e.normal);
            if let Err(err) = super::resize::restore_placement(e.hwnd, &p) {
                tracing::warn!(
                    hwnd = format!("{:#x}", e.hwnd),
                    "账本恢复失败（窗口已关？）: {err:#}"
                );
            } else {
                tracing::info!(
                    hwnd = format!("{:#x}", e.hwnd),
                    reason = %e.reason,
                    "账本恢复窗口布局"
                );
            }
        }
        self.write_all(&[])?;
        Ok(n)
    }

    fn read_all(&self) -> Result<Vec<LedgerEntry>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("账本 JSON 解析失败"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err).context("读账本失败"),
        }
    }

    /// 原子写：临时文件 + rename 替换（崩溃中断时旧账本完好）。
    fn write_all(&self, entries: &[LedgerEntry]) -> Result<()> {
        let json = serde_json::to_vec_pretty(entries).context("账本序列化失败")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .with_context(|| format!("写账本临时文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("账本 rename 失败: {}", self.path.display()))
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_clear_roundtrip_and_persistence() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger = OperationLedger::open(dir.path());
        assert!(ledger.read_all().unwrap().is_empty());

        let p = super::super::resize::SavedPlacement::from_parts(3, [100, 200, 900, 700]);
        ledger.record(0x1A0B2C, "remote-session", &p).unwrap();
        let entries = ledger.read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].show_cmd, 3);
        assert_eq!(entries[0].normal, [100, 200, 900, 700]);
        assert_eq!(entries[0].reason, "remote-session");

        // 同窗口重复记账覆盖（不重复累积）
        let p2 = super::super::resize::SavedPlacement::from_parts(1, [0, 0, 400, 300]);
        ledger.record(0x1A0B2C, "remote-session", &p2).unwrap();
        assert_eq!(ledger.read_all().unwrap().len(), 1);

        // 第二个窗口独立记账
        ledger.record(0xDEAD, "privacy-on", &p2).unwrap();
        assert_eq!(ledger.read_all().unwrap().len(), 2);

        // 销账：精确删对应窗口
        ledger.clear(0x1A0B2C);
        let entries = ledger.read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hwnd, 0xDEAD);

        // 崩溃模拟：重开账本对象，账本还在（落盘持久）
        let ledger2 = OperationLedger::open(dir.path());
        assert_eq!(ledger2.read_all().unwrap().len(), 1);
    }
}
