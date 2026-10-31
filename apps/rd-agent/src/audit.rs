//! 操作审计日志（商业化合规底座）：远控行为留痕到
//! `<config_dir>/logs/audit.jsonl`，一行一条 JSON，人工可直接查看。
//!
//! 与 tracing 日志分离的定位：tracing 面向开发排查，审计面向
//! 「谁在什么时候控了这台电脑、做了哪类操作」的追责场景（防远控滥用，
//! 国内远控软件的监管硬要求）。三条纪律：
//! - 只记元数据，**绝不记内容**（键值、屏幕、终端数据、剪贴板文本不留痕）；
//! - 对端 device_name 已在上游消毒（trim + 去控制字符 + 限长），仍经
//!   `serde_json` 转义兜底，防换行伪造审计行；
//! - 失败不致命：磁盘满/权限问题降级 warn，不阻塞远控本身。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// 单文件上限：超过即整体改名 `.1` 后重开（保留一代，简单够用）。
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

struct Audit {
    path: PathBuf,
    file: Option<File>,
}

/// 全局审计器；[`init`] 前调用 [`log`] 为空操作（启动早期无埋点，防御）。
static AUDIT: Mutex<Option<Audit>> = Mutex::new(None);

/// 初始化：日志目录（与 tracing 日志同目录）。失败仅告警（审计不可用
/// 不应阻止被控服务启动——可用性优先，缺审计宁可事后告警）。
pub fn init(dir: &Path) {
    if let Err(err) = fs::create_dir_all(dir) {
        tracing::warn!(dir = %dir.display(), "审计日志目录创建失败: {err}");
        return;
    }
    let path = dir.join("audit.jsonl");
    let file = open_append(&path);
    if file.is_none() {
        tracing::warn!(path = %path.display(), "审计日志文件打开失败");
    }
    *AUDIT.lock().expect("audit 锁中毒") = Some(Audit { path, file });
}

/// 追加一条审计事件。
///
/// `event`：蛇形命名事件（`session_start` / `window_open` / …）；
/// `peer`：对端 device_name（已消毒）；`fp`：对端指纹短码（真实身份锚点，
/// name 可伪造、fp 不可）；
/// `extra`：附加字段（元数据，禁止放任何用户内容）。
pub fn log(event: &str, peer: &str, fp: &str, extra: Value) {
    let mut guard = AUDIT.lock().expect("audit 锁中毒");
    let Some(audit) = guard.as_mut() else { return };
    if audit.file.is_none() {
        audit.file = open_append(&audit.path);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut line = serde_json::json!({
        "ts": ts,
        "event": event,
        "peer": peer,
        "fp": fp,
    });
    if let (Value::Object(dst), Value::Object(src)) = (&mut line, extra) {
        for (k, v) in src {
            dst.insert(k, v);
        }
    }
    let Some(file) = audit.file.as_mut() else {
        return;
    };
    if let Err(err) = writeln!(file, "{line}") {
        tracing::warn!("审计日志写入失败: {err}");
        audit.file = None;
        return;
    }
    // 轮转：超限即改名重开（拿不到锁的并发写丢进旧句柄，可接受）
    if file.metadata().map(|m| m.len()).unwrap_or(0) >= ROTATE_BYTES {
        let old = audit.path.with_extension("jsonl.1");
        let _ = fs::rename(&audit.path, &old);
        audit.file = open_append(&audit.path);
    }
}

fn open_append(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 审计器是进程级单例：三个场景合并为单个测试顺序执行，避免并行
    /// 互串目录（init 全局覆盖）。
    #[test]
    fn audit_lifecycle() {
        let dir = std::env::temp_dir().join(format!("rd-audit-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                *AUDIT.lock().unwrap_or_else(|e| e.into_inner()) = None;
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        init(&dir);

        // 1) 基本落盘 + 字段合并
        log(
            "session_start",
            "妈妈的老电脑",
            "aabbccdd",
            serde_json::json!({ "proto": 0 }),
        );
        let path = dir.join("audit.jsonl");
        let text = fs::read_to_string(&path).expect("审计文件应存在");
        let v: Value = serde_json::from_str(text.trim_end()).expect("每行都是合法 JSON");
        assert_eq!(v["event"], "session_start");
        assert_eq!(v["peer"], "妈妈的老电脑");
        assert_eq!(v["fp"], "aabbccdd");
        assert_eq!(v["proto"], 0);
        assert!(v["ts"].as_u64().is_some(), "时间戳必填");

        // 2) device_name 里的引号/换行必须被转义成合法 JSON 行（防伪造审计行）
        log("session_start", "坏\"名字\n第二行", "11223344", Value::Null);
        let text = fs::read_to_string(&path).expect("审计文件应存在");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: Value = serde_json::from_str(lines[1]).expect("转义后仍是合法 JSON");
        assert_eq!(v["peer"], "坏\"名字\n第二行");

        // 3) 轮转：写满阈值后旧文件改名 `.1`，新事件落新文件
        let big = "x".repeat(1024);
        for _ in 0..((ROTATE_BYTES / 1024) + 2) {
            log("tick", "p", "fp", serde_json::json!({ "pad": big }));
        }
        assert!(dir.join("audit.jsonl.1").exists(), "超限后应产生 .1 代文件");
        assert!(dir.join("audit.jsonl").exists(), "新文件应继续可写");
    }
}
