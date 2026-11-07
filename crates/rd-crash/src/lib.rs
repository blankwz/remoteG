//! 崩溃上报（T-040）：panic hook → 本地 JSON dump（**只记元数据，不做内存
//! 转储**——避免隐私争议，用户预览后才上报）。
//!
//! 职责边界：
//! - [`install_panic_hook`]：包装默认 hook，崩溃时落一个 dump 文件；
//! - [`list_dumps`]/[`clear_dumps`]：托盘/设置页的记录管理与预览数据源；
//! - [`issue_new_url`]：把 dump 摘要组装成 GitHub Issue 预填链接（用户点
//!   「上报」后由系统浏览器打开，提交与否仍在用户手里）。
//!
//! 上报仓库地址是产品级占位（[`ISSUE_NEW_URL_BASE`]），仓库远端就绪
//! （T-039）时替换；隐私政策条款随官网（T-042）落。

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// 保留的 dump 上限（超出删最旧）——崩溃风暴不撑爆磁盘。
const KEEP_MAX: usize = 10;

/// GitHub Issue 新建页基址（产品级占位；T-039 仓库远端就绪时替换）。
pub const ISSUE_NEW_URL_BASE: &str = "https://github.com/OWNER/rd/issues/new";

/// 单条崩溃记录（dump 文件的 JSON 结构，也是列表/预览的视图）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpEntry {
    /// 记录属于哪端（rd-agent / rd-controller）。
    pub app: String,
    /// 版本（CARGO_PKG_VERSION）。
    pub version: String,
    /// 崩溃时间（Unix 秒）。
    pub time_unix: u64,
    /// 崩溃线程名。
    pub thread: String,
    /// panic 消息（第一行，截断到 200 字符）。
    pub message: String,
    /// panic 源码位置（file:line）。
    pub location: String,
    /// dump 文件名（crash-<unix>-<微秒>.json）。
    pub file: String,
}

/// dump 目录（约定：各端日志目录下的 `crash/`）。
pub fn crash_dir(log_dir: &Path) -> PathBuf {
    log_dir.join("crash")
}

/// 落一条 dump（由 panic hook 调用；**内部绝不 panic**——失败只丢记录不升级）。
pub fn write_dump(dir: &Path, app: &str, version: &str, message: &str, location: &str, thread: &str) {
    let entry = DumpEntry {
        app: app.to_string(),
        version: version.to_string(),
        time_unix: now_unix(),
        thread: thread.to_string(),
        message: first_line(message, 200),
        location: location.to_string(),
        file: String::new(),
    };
    let _ = std::fs::create_dir_all(dir);
    let file = format!("crash-{}-{}.json", entry.time_unix, micros());
    let mut entry = entry;
    entry.file = file.clone();
    let payload = match serde_json::to_string_pretty(&entry) {
        Ok(p) => p,
        Err(_) => return, // 纯结构体序列化不会失败；保险起见静默
    };
    // create_new 防同名覆盖；任何失败都只是丢一条记录，不升级为 panic
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(&file))
        .and_then(|mut f| f.write_all(payload.as_bytes()));
    prune_dumps(dir, KEEP_MAX);
}

/// 安装 panic hook：包装既有 hook（保留默认 stderr 输出），崩溃时落 dump。
/// `app`/`version` 建议在调用方用 CARGO_PKG_* 锚定。
pub fn install_panic_hook(app: &'static str, version: &'static str, dir: PathBuf) {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let message = payload_str(info.payload());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        write_dump(&dir, app, version, &message, &location, &thread);
        tracing::error!(app, %location, "panic 已捕获，dump 已落盘");
        original(info);
    }));
}

fn payload_str(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string payload>".to_string()
    }
}

/// 列出 dump（按时间倒序；半写/损坏文件跳过，不阻塞其余记录）。
pub fn list_dumps(dir: &Path) -> Vec<DumpEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(mut entry) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<DumpEntry>(&s).ok())
        else {
            continue;
        };
        entry.file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        out.push(entry);
    }
    out.sort_by(|a, b| b.time_unix.cmp(&a.time_unix).then(b.file.cmp(&a.file)));
    out
}

/// 清空 dump 目录，返回删除条数。
pub fn clear_dumps(dir: &Path) -> usize {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("json")
                && std::fs::remove_file(e.path()).is_ok()
            {
                n += 1;
            }
        }
    }
    n
}

/// 按上限删最旧（文件名 crash-<unix>-<微秒>.json 字典序 = 时间序）。
pub fn prune_dumps(dir: &Path, keep: usize) {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".json"))
            .collect(),
        Err(_) => return,
    };
    names.sort();
    while names.len() > keep {
        let oldest = names.remove(0);
        let _ = std::fs::remove_file(dir.join(oldest));
    }
}

/// 组装「在浏览器上报」的预填 Issue 链接（body 截断，防 URL 超长）。
pub fn issue_new_url(dumps: &[DumpEntry], max_body_chars: usize) -> String {
    let mut body = String::from("自动收集的崩溃元数据（无内存内容）：\n\n");
    for d in dumps {
        body.push_str(&format!(
            "- {} v{} @ {} | thread={} | {} ({})\n",
            d.app, d.version, d.time_unix, d.thread, d.message, d.location
        ));
        if body.chars().count() >= max_body_chars {
            body.push_str("\n（已截断…）");
            break;
        }
    }
    format!(
        "{}?title={}&body={}",
        ISSUE_NEW_URL_BASE,
        urlencode("崩溃报告"),
        urlencode(&body)
    )
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    line.chars().take(max).collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn micros() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_micros())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_list_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = crash_dir(dir.path());
        write_dump(&d, "rd-agent", "0.3.0", "boom\nsecond line", "src/x.rs:10", "main");
        let dumps = list_dumps(&d);
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].app, "rd-agent");
        assert_eq!(dumps[0].version, "0.3.0");
        assert_eq!(dumps[0].message, "boom"); // 只取首行
        assert_eq!(dumps[0].location, "src/x.rs:10");
        assert_eq!(dumps[0].thread, "main");
        assert!(dumps[0].file.ends_with(".json"));
    }

    #[test]
    fn list_sorts_desc_and_skips_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = crash_dir(dir.path());
        std::fs::create_dir_all(&d).unwrap();
        let a = r#"{"app":"a","version":"1","time_unix":100,"thread":"t","message":"m","location":"l","file":""}"#;
        let b = r#"{"app":"b","version":"1","time_unix":200,"thread":"t","message":"m","location":"l","file":""}"#;
        std::fs::write(d.join("crash-100-1.json"), a).unwrap();
        std::fs::write(d.join("crash-200-1.json"), b).unwrap();
        std::fs::write(d.join("crash-300-1.json"), "not json").unwrap(); // 损坏
        let dumps = list_dumps(&d);
        assert_eq!(dumps.len(), 2); // 损坏被跳过
        assert_eq!(dumps[0].time_unix, 200); // 倒序
        assert_eq!(dumps[0].file, "crash-200-1.json"); // file 回填
    }

    #[test]
    fn prune_keeps_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = crash_dir(dir.path());
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..15u64 {
            let entry = DumpEntry {
                app: "a".to_string(),
                version: "1".to_string(),
                time_unix: i,
                thread: "t".to_string(),
                message: "m".to_string(),
                location: "l".to_string(),
                file: String::new(),
            };
            std::fs::write(
                d.join(format!("crash-{i}-1.json")),
                serde_json::to_string(&entry).unwrap(),
            )
            .unwrap();
        }
        prune_dumps(&d, 10);
        assert_eq!(list_dumps(&d).len(), 10);
        assert!(!d.join("crash-0-1.json").exists()); // 最旧被删
        assert!(d.join("crash-14-1.json").exists()); // 最新保留
    }

    #[test]
    fn clear_removes_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = crash_dir(dir.path());
        write_dump(&d, "a", "1", "m", "l", "t");
        write_dump(&d, "a", "1", "m", "l", "t");
        assert_eq!(clear_dumps(&d), 2);
        assert!(list_dumps(&d).is_empty());
    }

    #[test]
    fn issue_url_encodes_and_truncates() {
        let dumps: Vec<DumpEntry> = (0..50)
            .map(|i| DumpEntry {
                app: "rd-agent".to_string(),
                version: "0.3.0".to_string(),
                time_unix: i,
                thread: "main".to_string(),
                message: format!("崩溃 {i} 很长的消息内容"),
                location: "src/x.rs:1".to_string(),
                file: String::new(),
            })
            .collect();
        let url = issue_new_url(&dumps, 500);
        assert!(url.starts_with(ISSUE_NEW_URL_BASE));
        assert!(url.contains("title=%E5%B4%A9%E6%BA%83%E6%8A%A5%E5%91%8A")); // 中文编码
        assert!(url.contains("%E5%B7%B2%E6%88%AA%E6%96%AD")); // 「已截断」出现
        assert!(url.len() < 8000, "URL 不应超长: {}", url.len());
    }

    #[test]
    fn panic_hook_writes_dump() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = crash_dir(dir.path());
        install_panic_hook("rd-test", "9.9.9", d.clone());
        let _ = std::panic::catch_unwind(|| panic!("hook 测试崩溃"));
        let dumps = list_dumps(&d);
        assert_eq!(dumps.len(), 1);
        assert_eq!(dumps[0].message, "hook 测试崩溃");
        assert_eq!(dumps[0].app, "rd-test");
        clear_dumps(&d);
        // 还原默认 hook，避免污染其他测试（set_hook 是进程全局状态）
        let _ = std::panic::take_hook();
    }
}
