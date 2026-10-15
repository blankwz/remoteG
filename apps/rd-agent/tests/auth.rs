//! T-015 鉴权链路端到端测试(任务卡测试要求)。
//!
//! 启动真实 `rd-agent run` 子进程(debug 构建,`--test-pair-code` 注入固定码),
//! 用 rd-transport 直连:
//! 1. `untrusted_peer_gets_auth_error`:未信任连接发 `ListWindows` → `Error{code:1}`;
//! 2. `pairing_succeeds_and_survives_reconnect`:用 `ControllerPairing` 走完
//!    4 步配对 → 成功;断开重连后首条 `Hello` 直接得到 `HelloAck`;
//!    信任列表文件已落盘。
//!
//! 每个用例独立临时目录,互不污染用户 `%LOCALAPPDATA%\rd`。

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rd_proto::v0::{envelope, Envelope, Hello, ListWindows, PairRequest};
use rd_security::{ControllerPairing, PairStep};
use rd_transport::{connect, ControlRx, ControlTx, Identity};

/// 测试配对码(与子进程 `--test-pair-code` 一致;仅存在于本测试进程与子进程内存)。
const TEST_PAIR_CODE: &str = "123456";
/// 每次控制流收包的超时。
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// 子进程管理
// ---------------------------------------------------------------------------

struct AgentProc {
    child: Child,
}

impl Drop for AgentProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 启动 `rd-agent run`(随机端口)并从 stdout 读取实际监听地址。
fn spawn_agent(dir: &std::path::Path) -> Result<(AgentProc, SocketAddr)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rd-agent"))
        .args([
            "run",
            "--no-tray",
            "--listen",
            "127.0.0.1:0",
            "--print-port",
            "--config-dir",
        ])
        .arg(dir)
        .args(["--test-pair-code", TEST_PAIR_CODE])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env("RUST_LOG", "debug")
        .spawn()
        .context("启动 rd-agent 子进程失败")?;

    let stdout = child.stdout.take().context("未捕获 rd-agent stdout")?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        // stdout 现在带彩色日志行:逐行读直到机器可读的 RD_LISTEN= 前缀
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if let Some(addr) = trimmed.strip_prefix("RD_LISTEN=") {
                        let _ = tx.send(addr.to_string());
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .context("rd-agent 未在 30s 内打印监听地址(检查 stderr)")?;
    let addr: SocketAddr = line
        .parse()
        .with_context(|| format!("无法解析 rd-agent 打印的监听地址行: {line:?}"))?;
    Ok((AgentProc { child }, addr))
}

// ---------------------------------------------------------------------------
// 控制流辅助
// ---------------------------------------------------------------------------

fn hello_envelope() -> Envelope {
    Envelope {
        req_id: 1,
        msg: Some(envelope::Msg::Hello(Hello {
            proto_ver: 0,
            device_name: "e2e-test-client".to_string(),
            decode_caps: Vec::new(),
            supports_444: false,
        })),
    }
}

async fn send(tx: &mut ControlTx, env: &Envelope) -> Result<()> {
    tx.send(env).await
}

async fn recv(rx: &mut ControlRx) -> Result<Envelope> {
    let env = tokio::time::timeout(RECV_TIMEOUT, rx.recv())
        .await
        .context("等待响应超时")?
        .context("控制流已关闭")?
        .context("解码信封失败")?;
    Ok(env)
}

/// 收一条 `PairResult`。
async fn recv_pair_result(rx: &mut ControlRx) -> Result<rd_proto::v0::PairResult> {
    match recv(rx).await?.msg {
        Some(envelope::Msg::PairRes(res)) => Ok(res),
        Some(other) => bail!("期望 PairResult,实际 {other:?}"),
        None => bail!("收到空信封"),
    }
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// 未信任连接:Hello 后发 ListWindows → 必须收到 Error{code:1}(11_security 验收)。
#[tokio::test(flavor = "multi_thread")]
async fn untrusted_peer_gets_auth_error() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (_agent, addr) = spawn_agent(dir.path())?;

    let id = Identity::generate("e2e-untrusted")?;
    let conn = connect(addr, &id, None).await?;
    let (mut tx, mut rx) = conn.open_control().await?;
    send(&mut tx, &hello_envelope()).await?;
    // 未信任:不回 HelloAck,直接拒绝业务消息
    send(
        &mut tx,
        &Envelope {
            req_id: 2,
            msg: Some(envelope::Msg::ListWindows(ListWindows {})),
        },
    )
    .await?;

    let env = recv(&mut rx).await?;
    match env.msg {
        Some(envelope::Msg::Error(err)) => {
            assert_eq!(err.code, 1, "未鉴权错误码必须为 1,实际 {err:?}");
        }
        Some(other) => bail!("期望 Error{{code:1}},实际 {other:?}"),
        None => bail!("收到空信封"),
    }
    Ok(())
}

/// 4 步配对成功 → 信任列表落盘 → 断开重连直接 HelloAck(任务卡测试要求)。
#[tokio::test(flavor = "multi_thread")]
async fn pairing_succeeds_and_survives_reconnect() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (_agent, addr) = spawn_agent(dir.path())?;

    let id = Identity::generate("e2e-pairing")?;

    // --- 第一次连接:配对 ---
    let conn = connect(addr, &id, None).await?;
    let agent_fp = conn.peer_fingerprint();
    let (mut tx, mut rx) = conn.open_control().await?;
    send(&mut tx, &hello_envelope()).await?;
    // Agent 对未信任连接的 Hello 回 Error{1}(05_api §1),消费后发起配对
    let rejected = recv(&mut rx).await?;
    assert!(
        matches!(rejected.msg, Some(envelope::Msg::Error(ref e)) if e.code == 1),
        "配对前期望 Error{{code:1}},实际 {rejected:?}"
    );

    let (mut pairing, first_msg) =
        ControllerPairing::start(TEST_PAIR_CODE, &id.fingerprint(), &agent_fp);
    send(
        &mut tx,
        &Envelope {
            req_id: 10,
            msg: Some(envelope::Msg::PairReq(PairRequest {
                spake2_msg: first_msg,
            })),
        },
    )
    .await?;

    // 第 2 轮:Agent 的 SPAKE2 消息(ok:true 承载)
    let res1 = recv_pair_result(&mut rx).await?;
    assert!(res1.ok, "第 2 轮应为成功: {:?}", res1.reason);
    let mac_c = match pairing.on_message(&res1.spake2_msg) {
        PairStep::Send(mac) => mac,
        other => bail!("控制端第 2 轮期望 Send(MAC_c),实际 {other:?}"),
    };

    // 第 3→4 轮:发 MAC_c;Agent 校验通过后回 MAC_a(此刻 Agent 侧配对已成立)
    send(
        &mut tx,
        &Envelope {
            req_id: 11,
            msg: Some(envelope::Msg::PairReq(PairRequest { spake2_msg: mac_c })),
        },
    )
    .await?;
    let res2 = recv_pair_result(&mut rx).await?;
    assert!(res2.ok, "第 4 轮应为成功: {:?}", res2.reason);
    assert!(
        !res2.spake2_msg.is_empty(),
        "第 4 轮必须携带 Agent 的 MAC_a"
    );
    assert_eq!(
        pairing.on_message(&res2.spake2_msg),
        PairStep::Done,
        "控制端校验 MAC_a 必须完成配对"
    );
    conn.close(0, "paired");
    drop(conn);
    drop(tx);
    drop(rx);

    // Agent 在发出 MAC_a 之后落盘信任列表;进程间无同步点,留一个短暂窗口
    // (真实场景中该时间远小于跨机 RTT,重连前信任必然已就绪)
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 信任列表已落盘(存的是被信任的控制端指纹)
    let trust_file = dir.path().join("trusted.json");
    assert!(trust_file.exists(), "配对成功后信任列表必须落盘");
    let raw = std::fs::read_to_string(&trust_file)?;
    assert!(
        raw.contains(&id.fingerprint().hex()),
        "信任列表必须包含控制端(客户端)指纹"
    );

    // --- 重连:直接 HelloAck(不再需要配对)---
    let conn2 = connect(addr, &id, Some(agent_fp)).await?;
    let (mut tx2, mut rx2) = conn2.open_control().await?;
    send(&mut tx2, &hello_envelope()).await?;
    let ack = tokio::time::timeout(RECV_TIMEOUT, rx2.recv())
        .await
        .context("等待 HelloAck 超时")?
        .context("控制流已关闭")?
        .context("解码信封失败")?;
    match ack.msg {
        Some(envelope::Msg::HelloAck(ack)) => {
            assert!(!ack.host_name.is_empty(), "HelloAck.host_name 不能为空");
            assert!(!ack.encode_caps.is_empty(), "HelloAck 必须携带编码能力");
        }
        Some(other) => bail!("受信任重连应直接 HelloAck,实际 {other:?}"),
        None => bail!("收到空信封"),
    }
    Ok(())
}
