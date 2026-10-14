//! TermManager 集成测试：真实 shell（Windows 用 cmd.exe，其他平台用 $SHELL）。
//! ConPTY 回显带 \r\n 与 ANSI 序列，断言一律用"包含字节序列"的宽松匹配（T-014）。

use std::time::{Duration, Instant};

use rd_term::{TermManager, TermOutput};
use tokio::sync::broadcast;

/// 测试用 shell：Windows 选 cmd.exe（系统必有），其他平台用 $SHELL。
#[cfg(windows)]
fn test_shell() -> String {
    "cmd.exe".to_string()
}

/// 测试用 shell：Windows 选 cmd.exe（系统必有），其他平台用 $SHELL。
#[cfg(not(windows))]
fn test_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// 平台对应的行为行结束符。
fn eol() -> &'static str {
    if cfg!(windows) {
        "\r\n"
    } else {
        "\n"
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 收集输出流直到包含 `needle` 或超时；返回累计字节（供失败信息）。
async fn collect_until(
    rx: &mut broadcast::Receiver<TermOutput>,
    needle: &[u8],
    timeout: Duration,
) -> Vec<u8> {
    let mut acc: Vec<u8> = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if contains(&acc, needle) {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Ok(TermOutput::Data(d))) => acc.extend_from_slice(&d),
            Ok(Ok(TermOutput::Exit(_))) | Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Err(_) => {}
        }
    }
    acc
}

#[tokio::test]
async fn open_shell_write_and_read_echo() {
    let mgr = TermManager::new(Duration::from_secs(60));
    let id = mgr.open(&test_shell(), 120, 30).expect("open terminal");
    let (_snapshot, mut rx) = mgr.attach(&id).expect("attach");
    let cmd = format!("echo rd_ok{}", eol());
    mgr.write(&id, cmd.as_bytes()).expect("write");
    let out = collect_until(&mut rx, b"rd_ok", Duration::from_secs(10)).await;
    assert!(
        contains(&out, b"rd_ok"),
        "output should contain rd_ok, got: {:?}",
        String::from_utf8_lossy(&out)
    );
    mgr.close(&id);
}

#[tokio::test]
async fn resize_is_accepted() {
    let mgr = TermManager::new(Duration::from_secs(60));
    let id = mgr.open(&test_shell(), 80, 24).expect("open terminal");
    mgr.resize(&id, 100, 40).expect("resize up");
    mgr.resize(&id, 40, 10).expect("resize down");
    mgr.close(&id);
}

#[tokio::test]
async fn attach_after_detach_replays_history() {
    let mgr = TermManager::new(Duration::from_secs(60));
    let id = mgr.open(&test_shell(), 120, 30).expect("open terminal");
    {
        let (_, mut rx) = mgr.attach(&id).expect("attach #1");
        let cmd = format!("echo rd_hist{}", eol());
        mgr.write(&id, cmd.as_bytes()).expect("write");
        let out = collect_until(&mut rx, b"rd_hist", Duration::from_secs(10)).await;
        assert!(
            contains(&out, b"rd_hist"),
            "first session output: {:?}",
            String::from_utf8_lossy(&out)
        );
    } // Receiver drop = detach
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (snapshot, _rx2) = mgr.attach(&id).expect("attach #2");
    assert!(
        contains(&snapshot, b"rd_hist"),
        "replay snapshot missing history: {:?}",
        String::from_utf8_lossy(&snapshot)
    );
    mgr.close(&id);
}

#[tokio::test]
async fn idle_session_reclaimed_after_timeout() {
    let mgr = TermManager::new(Duration::from_secs(1));
    let id = mgr.open(&test_shell(), 80, 24).expect("open terminal");
    // 从不订阅：超过 idle_timeout 后会话应被看门狗回收。
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        mgr.attach(&id).is_err(),
        "session should be reclaimed after idle timeout"
    );
}

#[tokio::test]
async fn non_allowlisted_shell_rejected_with_code_30() {
    let mgr = TermManager::new(Duration::from_secs(60));
    let err = mgr
        .open("definitely-not-a-shell.exe", 80, 24)
        .expect_err("non-allowlisted shell must be rejected");
    assert!(
        format!("{err:#}").contains("30"),
        "error should carry protocol code 30: {err:#}"
    );
}

#[test]
fn attach_missing_term_id_errors() {
    let mgr = TermManager::new(Duration::from_secs(60));
    assert!(mgr.attach("deadbeef").is_err());
}
