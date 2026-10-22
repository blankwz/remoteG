#![cfg(windows)]

//! T-025 无头端到端测试公共设施：构建/启动 testwin 与 Agent、配对客户端、
//! 收帧与比对工具（任务卡步骤 1）。
//!
//! 进程与连接模型：
//! - Agent 与 testwin 进程在首个用例时启动一次（`OnceLock<Env>`），Drop 时按
//!   句柄 kill（不做全局强杀）；Agent 配置目录为进程内 TempDir，自动清理；
//! - `clarity-bench` 不是 rd-agent 的依赖（`CARGO_BIN_EXE` 不跨 crate），
//!   首个用例前检查 `target/debug/clarity-bench.exe`，缺失则调
//!   `cargo build -p clarity-bench` 补建；
//! - 每个用例独立建立 QUIC 连接：首次连接走 4 步配对（信任列表落盘），
//!   后续连接直接 `HelloAck`（与 tests/auth.rs 同语义）；
//! - `serial_lock()` 串行化全部用例——即使不带 `--test-threads=1` 运行，
//!   共享的 testwin 也不会被并发操作（任务卡要求串行语义）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rd_proto::v0::{envelope, Envelope, Hello, PairRequest, PairResult};
use rd_security::{ControllerPairing, PairStep};
use rd_transport::{
    connect, Conn, ControlRx, ControlTx, DataFrame, DataRx, Fingerprint, Identity, StreamKind,
    VideoHeader,
};

/// 测试配对码（与 Agent 子进程 `--test-pair-code` 一致；仅存在于测试与子进程内存）。
pub const TEST_PAIR_CODE: &str = "123456";
/// testwin 客户区尺寸（物理像素，`--size`；150% DPI 下同样成立）。
pub const CLIENT_W: u32 = 800;
pub const CLIENT_H: u32 = 600;
/// 控制流收包默认超时。
pub const RECV_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// 共享环境
// ---------------------------------------------------------------------------

/// 一次初始化的共享测试环境：testwin 与 Agent 子进程、监听地址、窗口句柄。
pub struct Env {
    /// Agent 监听地址（`127.0.0.1:<随机端口>`）。
    pub addr: SocketAddr,
    /// testwin 顶层窗口句柄。
    pub hwnd: u64,
    _testwin: TestwinProc,
    _agent: AgentProc,
    _config_dir: tempfile::TempDir,
}

static ENV: OnceLock<Env> = OnceLock::new();
/// 用例互斥锁（tokio Mutex：guard 需跨 await 持有整个用例周期）。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// 本测试进程是否已完成 4 步配对（信任列表已在 Agent 侧落盘）。
static PAIRED: AtomicBool = AtomicBool::new(false);

/// 用例间互斥：共享的 testwin/Agent 同一时刻只允许一个用例操作。
/// 即使命令行未加 `--test-threads=1` 也能保证串行语义。
pub async fn serial_lock() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

/// 共享环境（首次调用启动 testwin 与 Agent，约 2 s；此后零开销）。
pub fn env() -> &'static Env {
    ENV.get_or_init(|| {
        setup_env().unwrap_or_else(|err| panic!("初始化 e2e 环境（testwin/Agent）失败: {err:#}"))
    })
}

fn setup_env() -> Result<Env> {
    kill_orphans();
    ensure_testwin_built()?;
    let config_dir = tempfile::tempdir().context("创建 Agent 临时配置目录失败")?;
    let testwin = spawn_testwin()?;
    let testwin_pid = testwin.child.id();
    // 按子进程 PID 锁定自己的 testwin——桌面上可能有历史遗留的同名窗口。
    let hwnd = wait_testwin_window(testwin_pid).context("等待 testwin 窗口出现失败")?;
    let (agent, addr) = spawn_agent(config_dir.path())?;
    let agent_pid = agent.child.id();
    spawn_cleanup_watchdog(std::process::id(), agent_pid, testwin_pid);
    Ok(Env {
        addr,
        hwnd,
        _testwin: testwin,
        _agent: agent,
        _config_dir: config_dir,
    })
}

/// 历次运行遗留进程的特征过滤（PowerShell）：只匹配 e2e 专有参数——
/// rd-agent 的 `--test-pair-code` 为 debug-only 测试标志，生产不携带；
/// clarity-bench 匹配 e2e 的 static 尺寸参数。仅清理存活超过 30 s 的进程：
/// 同轮 `cargo test` 中并行的 auth 等其他测试 target 也可能带同参运行。
const ORPHAN_PROBE_PS: &str = r#"
$cutoff = (Get-Date).AddSeconds(-30)
Get-CimInstance Win32_Process -Filter "Name='rd-agent.exe'" |
  Where-Object { $_.CommandLine -like '*--test-pair-code*' -and $_.CreationDate -lt $cutoff } |
  ForEach-Object { $_.ProcessId }
Get-CimInstance Win32_Process -Filter "Name='clarity-bench.exe'" |
  Where-Object { $_.CommandLine -like '*testwin --mode static*' -and $_.CreationDate -lt $cutoff } |
  ForEach-Object { $_.ProcessId }
"#;

/// 清理历次 e2e 运行遗留的子进程（按 PID 结束，禁止按进程名全局强杀）。
/// `OnceLock` 静态不执行 Drop，测试二进制退出后子进程会残留；主清理由
/// [`spawn_cleanup_watchdog`] 在测试进程退出的瞬间完成，本函数在每次初始
/// 化时兜底回收（覆盖看门狗被强杀等极端情况）。
fn kill_orphans() {
    let Ok(out) = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ORPHAN_PROBE_PS])
        .output()
    else {
        return; // 探测失败不阻塞测试
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue;
        };
        let killed = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if killed {
            println!("[e2e] 已清理历次运行遗留进程 pid={pid}");
        }
    }
}

/// 启动看门狗：阻塞等待测试进程退出，随即按 PID 结束本次启动的 Agent 与
/// testwin。静态持有的 Env 不会 Drop，无法在进程内做 teardown——看门狗
/// 在进程退出的瞬间补位，串行/并行/中断运行均无残留、无用例间竞态。
fn spawn_cleanup_watchdog(test_pid: u32, agent_pid: u32, testwin_pid: u32) {
    let script = format!(
        "Wait-Process -Id {test_pid} -ErrorAction SilentlyContinue; \
         taskkill /PID {agent_pid} /F 2>$null; taskkill /PID {testwin_pid} /F 2>$null"
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// 子进程管理（Drop 按 PID 结束，禁止全局强杀）
// ---------------------------------------------------------------------------

struct TestwinProc {
    child: Child,
}

impl Drop for TestwinProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct AgentProc {
    child: Child,
}

impl Drop for AgentProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `target/debug/clarity-bench(.exe)`：`CARGO_BIN_EXE` 不跨 crate，手工定位。
fn testwin_exe() -> PathBuf {
    let dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    dir.join("debug").join(if cfg!(windows) {
        "clarity-bench.exe"
    } else {
        "clarity-bench"
    })
}

/// clarity-bench 缺失时补建（env 继承：ffmpeg/clang 注入仍在）。
fn ensure_testwin_built() -> Result<()> {
    if testwin_exe().exists() {
        return Ok(());
    }
    println!("[e2e] target/debug 下无 clarity-bench，执行 cargo build -p clarity-bench …");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "-p", "clarity-bench"])
        .status()
        .context("启动 cargo build -p clarity-bench 失败")?;
    anyhow::ensure!(
        status.success(),
        "cargo build -p clarity-bench 失败: {status}"
    );
    Ok(())
}

/// 启动 `clarity-bench testwin --mode static --size 800x600`。
fn spawn_testwin() -> Result<TestwinProc> {
    let child = Command::new(testwin_exe())
        .args([
            "testwin",
            "--mode",
            "static",
            "--size",
            &format!("{CLIENT_W}x{CLIENT_H}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("启动 testwin 失败: {}", testwin_exe().display()))?;
    Ok(TestwinProc { child })
}

/// 轮询窗口列表直到 `pid` 的 testwin 出现（可捕获顶层窗口），返回 hwnd。
fn wait_testwin_window(pid: u32) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let wins = rd_window_win::list_windows()?;
        if let Some(w) = wins.iter().find(|w| {
            w.pid == pid
                && w.process.eq_ignore_ascii_case("clarity-bench.exe")
                && w.title.starts_with("clarity-bench")
        }) {
            return Ok(w.hwnd as u64);
        }
        if Instant::now() > deadline {
            bail!("10s 内未在窗口列表中找到本进程启动的 clarity-bench testwin（pid {pid}）");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 启动 `rd-agent run`（随机端口）并从 stdout 读取实际监听地址（同 tests/auth.rs）。
fn spawn_agent(config_dir: &Path) -> Result<(AgentProc, SocketAddr)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rd-agent"))
        .args([
            "run",
            "--no-tray",
            "--listen",
            "127.0.0.1:0",
            "--print-port",
            "--config-dir",
        ])
        .arg(config_dir)
        .args(["--test-pair-code", TEST_PAIR_CODE])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env("RUST_LOG", "info")
        .spawn()
        .context("启动 rd-agent 子进程失败")?;
    let stdout = child.stdout.take().context("未捕获 rd-agent stdout")?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    // 逐行读直到机器可读的 RD_LISTEN= 前缀(stdout 现在同时输出彩色日志行);
    // 找到后**继续消费**管道,防止 Agent 的 stdout 写因读端关闭而失败。
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF:Agent 退出
                Ok(_) => {
                    if let Some(addr) = line.trim_end().strip_prefix("RD_LISTEN=") {
                        let _ = tx.send(addr.to_string());
                        // 不 break:继续排空管道
                    }
                }
                Err(_) => break,
            }
        }
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .context("rd-agent 未在 30s 内打印监听地址")?;
    let addr: SocketAddr = line
        .parse()
        .with_context(|| format!("无法解析 rd-agent 打印的监听地址行: {line:?}"))?;
    Ok((AgentProc { child }, addr))
}

/// 本测试进程的固定控制端身份（配对一次后所有用例复用信任）。
pub fn client_identity() -> &'static Identity {
    static ID: OnceLock<Identity> = OnceLock::new();
    ID.get_or_init(|| Identity::generate("rd-e2e-client").expect("生成 e2e 客户端身份失败"))
}

// ---------------------------------------------------------------------------
// 控制流客户端
// ---------------------------------------------------------------------------

/// 一个用例级的已鉴权客户端（Hello → HelloAck 之后的控制流两端）。
pub struct Client {
    pub conn: Conn,
    pub tx: ControlTx,
    pub rx: ControlRx,
    next_req: u32,
}

impl Client {
    /// 建立连接并完成 Hello 握手；首次调用附带 4 步配对。
    pub async fn connect() -> Result<Self> {
        let env = env();
        let conn = connect(env.addr, client_identity(), None)
            .await
            .context("连接 Agent 失败")?;
        let agent_fp = conn.peer_fingerprint();
        let (mut tx, mut rx) = conn.open_control().await.context("打开控制流失败")?;
        tx.send(&hello_envelope()).await?;
        if PAIRED.swap(true, Ordering::SeqCst) {
            expect_hello_ack(&mut rx).await?;
        } else {
            // 未信任:Agent 对 Hello 回 Error{1}(req_id 沿用),消费后再发起配对
            let rejected = tokio::time::timeout(RECV_TIMEOUT, rx.recv())
                .await
                .context("等待配对前置 Error{1} 超时")?
                .context("控制流已关闭")?
                .context("解码信封失败")?;
            assert!(
                matches!(rejected.msg, Some(envelope::Msg::Error(ref e)) if e.code == 1),
                "配对前期望 Error{{code:1}},实际 {rejected:?}"
            );
            pair_over(&mut tx, &mut rx, agent_fp).await?;
            expect_hello_ack(&mut rx).await?;
        }
        Ok(Self {
            conn,
            tx,
            rx,
            next_req: 1,
        })
    }

    /// 发送一条消息（自动分配 req_id），返回该 id。
    pub async fn send(&mut self, msg: envelope::Msg) -> Result<u32> {
        let id = self.next_req;
        self.next_req = self.next_req.wrapping_add(1).max(1);
        self.tx
            .send(&Envelope {
                req_id: id,
                msg: Some(msg),
            })
            .await
            .context("发送控制流消息失败")?;
        Ok(id)
    }

    /// 发请求并等待同 req_id 的应答（期间 req_id=0 的推送打印后丢弃）。
    pub async fn call(&mut self, msg: envelope::Msg) -> Result<Envelope> {
        let id = self.send(msg).await?;
        self.recv_until(RECV_TIMEOUT, |env: &Envelope| env.req_id == id)
            .await
    }

    /// 按谓词收包；期间不匹配的消息打印后丢弃。
    pub async fn recv_until<F>(&mut self, timeout: Duration, pred: F) -> Result<Envelope>
    where
        F: Fn(&Envelope) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let env = tokio::time::timeout(left, self.rx.recv())
                .await
                .context("等待控制流消息超时")?
                .context("控制流已关闭")?
                .context("解码信封失败")?;
            if pred(&env) {
                return Ok(env);
            }
            println!("[e2e] 丢弃非目标推送: {:?}", env.msg);
        }
    }

    /// 显式关闭连接并驻留一瞬，确保 QUIC close 帧在本用例的 runtime 内发出。
    ///
    /// `#[tokio::test]` 的 runtime 在用例函数返回时立即 shutdown：若仅依赖
    /// drop 触发连接关闭，close/FIN 帧可能来不及发出，Agent 侧察觉断开会
    /// 退化为 idle 超时，`on_disconnect` 的窗口管线清理随之延迟不可控
    /// （t07 门禁探测曾见 3s 内 clicks 一路 +1——旧管线始终未被清理）。
    /// 下一个用例依赖旧连接清理结果时，必须以本方法收尾。
    pub async fn close(self) {
        self.conn.close(0, "e2e case done");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn hello_envelope() -> Envelope {
    Envelope {
        req_id: 1,
        msg: Some(envelope::Msg::Hello(Hello {
            proto_ver: 0,
            device_name: "rd-e2e-client".to_string(),
            decode_caps: Vec::new(),
            supports_444: false,
        })),
    }
}

/// 4 步配对（语义同 tests/auth.rs 的 ControllerPairing 流程）。
async fn pair_over(tx: &mut ControlTx, rx: &mut ControlRx, agent_fp: Fingerprint) -> Result<()> {
    let id = client_identity();
    let (mut pairing, first_msg) =
        ControllerPairing::start(TEST_PAIR_CODE, &id.fingerprint(), &agent_fp);
    tx.send(&Envelope {
        req_id: 900,
        msg: Some(envelope::Msg::PairReq(PairRequest {
            spake2_msg: first_msg,
        })),
    })
    .await?;

    // 第 2 轮：Agent 的 SPAKE2 消息（ok:true 承载）
    let res1 = recv_pair_result(rx).await?;
    anyhow::ensure!(res1.ok, "配对第 2 轮失败: {}", res1.reason);
    let mac_c = match pairing.on_message(&res1.spake2_msg) {
        PairStep::Send(mac) => mac,
        other => bail!("控制端第 2 轮期望 Send(MAC_c)，实际 {other:?}"),
    };

    // 第 3 → 4 轮：发 MAC_c，收 Agent 的 MAC_a
    tx.send(&Envelope {
        req_id: 901,
        msg: Some(envelope::Msg::PairReq(PairRequest { spake2_msg: mac_c })),
    })
    .await?;
    let res2 = recv_pair_result(rx).await?;
    anyhow::ensure!(res2.ok, "配对第 4 轮失败: {}", res2.reason);
    anyhow::ensure!(
        !res2.spake2_msg.is_empty(),
        "第 4 轮必须携带 Agent 的 MAC_a"
    );
    anyhow::ensure!(
        pairing.on_message(&res2.spake2_msg) == PairStep::Done,
        "控制端校验 MAC_a 必须完成配对"
    );
    println!("[e2e] 配对完成（信任列表已落盘）");
    Ok(())
}

async fn recv_pair_result(rx: &mut ControlRx) -> Result<PairResult> {
    loop {
        let env = tokio::time::timeout(RECV_TIMEOUT, rx.recv())
            .await
            .context("等待 PairResult 超时")?
            .context("控制流已关闭")?
            .context("解码信封失败")?;
        if let Some(envelope::Msg::PairRes(res)) = env.msg {
            return Ok(res);
        }
    }
}

async fn expect_hello_ack(rx: &mut ControlRx) -> Result<()> {
    let ack = tokio::time::timeout(RECV_TIMEOUT, rx.recv())
        .await
        .context("等待 HelloAck 超时")?
        .context("控制流已关闭")?
        .context("解码信封失败")?;
    match ack.msg {
        Some(envelope::Msg::HelloAck(_)) => Ok(()),
        Some(other) => bail!("受信任连接应直接 HelloAck，实际 {other:?}"),
        None => bail!("收到空信封"),
    }
}

// ---------------------------------------------------------------------------
// 轮询等待与注入防御
// ---------------------------------------------------------------------------

/// 条件轮询：每 `interval` 调用一次 `pred`，为真立即返回 true；`timeout` 到期
/// 仍为假返回 false。
///
/// 等待"条件满足"而非赌固定时长——固定 sleep 在负载高时会挤压事件窗口
/// （FIX-1 报告的 t06/t07 间歇性失败根因之一）。各用例的等待一律经本 helper
/// 表达，断言语义由调用方保持。
pub async fn wait_for<F>(timeout: Duration, interval: Duration, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        tokio::time::sleep(interval.min(left)).await;
    }
}

/// 注入用例的预置前：测试进程先走一遍与 Agent 相同的三层置前，Agent 端随后
/// 的 ensure_fg 将直接命中"已前台"分支。后台进程受前台锁时置前偶发失败，
/// Agent 会直接丢弃该次注入——所有注入用例在发出注入前（含失败重试时）统一
/// 预置前，消除置前失败被丢弃的路径。失败时打印诊断（Agent 端日志写文件，
/// 测试输出不可见，此处是置前失败唯一的观测点）。
pub fn preset_foreground(hwnd: u64) -> bool {
    let ok = rd_input_win::ensure_foreground(hwnd as isize);
    if !ok {
        println!(
            "[e2e] 预置前失败（hwnd {hwnd:#x} 未成为前台）——本次注入可能被 Agent 前台门控丢弃"
        );
    }
    ok
}

// ---------------------------------------------------------------------------
// 窗口/数据流辅助
// ---------------------------------------------------------------------------

/// `OpenWindow` 的完整结果：ack + 两条单向数据流（视频、瓦片）。
pub struct WindowStreams {
    pub ack: rd_proto::v0::OpenWindowAck,
    pub video: DataRx,
    pub tile: DataRx,
}

/// 打开窗口并收齐 ack 与两条数据流（Agent 侧先视频后瓦片建立，顺序一致）。
pub async fn open_window(c: &mut Client, hwnd: u64) -> Result<WindowStreams> {
    let reply = c
        .call(envelope::Msg::OpenWindow(rd_proto::v0::OpenWindow {
            window_id: hwnd,
            fit: rd_proto::v0::FitMode::Fit as i32,
            max_level: rd_proto::v0::RenderLevel::L0Pixel as i32,
            max_fps: 30,
            view_only: false,
        }))
        .await?;
    let ack = match reply.msg {
        Some(envelope::Msg::OpenWindowAck(ack)) => ack,
        Some(other) => bail!("期望 OpenWindowAck，实际 {other:?}"),
        None => bail!("收到空信封"),
    };
    anyhow::ensure!(
        ack.window_id == hwnd,
        "ack.window_id 不符: {}",
        ack.window_id
    );

    let (kind1, _, video) = c.conn.accept_data().await?;
    anyhow::ensure!(
        kind1 == StreamKind::Video,
        "第一条数据流应为视频，实际 {kind1:?}"
    );
    let (kind2, _, tile) = c.conn.accept_data().await?;
    anyhow::ensure!(
        kind2 == StreamKind::Tile,
        "第二条数据流应为瓦片，实际 {kind2:?}"
    );
    Ok(WindowStreams { ack, video, tile })
}

/// 从视频流收下一个属于 `window_id` 的帧（Annex-B AU）。
pub async fn recv_video_frame(
    rx: &mut DataRx,
    window_id: u64,
    timeout: Duration,
) -> Result<(VideoHeader, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let frame = tokio::time::timeout(left, rx.recv())
            .await
            .context("等待视频帧超时")?
            .context("视频流接收失败")?;
        match frame {
            Some(DataFrame::Video(header, au)) if header.window_id == window_id => {
                return Ok((header, au.to_vec()));
            }
            Some(DataFrame::Video(header, _)) => {
                println!("[e2e] 丢弃其他窗口视频帧: window={:#x}", header.window_id);
            }
            Some(other) => println!("[e2e] 视频流收到非预期帧: {other:?}"),
            None => bail!("视频流被对端关闭"),
        }
    }
}

/// 收集瓦片直到全覆盖 / 400 ms 无新瓦片 / 预算耗尽。
///
/// 返回（拼图画布 BGRA, 逐像素覆盖掩码）。失效标记（fmt=255）跳过不计覆盖。
pub async fn collect_tiles(
    rx: &mut DataRx,
    window_id: u64,
    budget: Duration,
) -> Result<(Vec<u8>, Vec<bool>)> {
    let (w, h) = (CLIENT_W as usize, CLIENT_H as usize);
    let mut canvas = vec![0u8; w * h * 4];
    let mut covered = vec![false; w * h];
    let mut covered_count = 0usize;
    // 诊断统计：区分"流上无数据"与"数据被过滤"。
    let mut stats = (0usize, 0usize, 0usize); // (数据瓦片, 失效标记, 其他窗口)
    let deadline = Instant::now() + budget;
    // 瓦片 tick 50 ms；连续 400 ms 无新数据视为该静默窗口内无新增（静止画面
    // 精修完成后即如此），提前结束。
    const QUIET: Duration = Duration::from_millis(400);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || covered_count >= w * h {
            break;
        }
        let wait = QUIET.min(left);
        let frame = match tokio::time::timeout(wait, rx.recv()).await {
            Err(_elapsed) => break,
            Ok(Err(err)) => return Err(err.context("瓦片流接收失败")),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(frame))) => frame,
        };
        let DataFrame::Tile(header, data) = frame else {
            println!("[e2e] 瓦片流收到非预期帧: {frame:?}");
            continue;
        };
        if header.window_id != window_id {
            stats.2 += 1;
            continue;
        }
        if header.fmt == rd_encode::tiles::FMT_INVALIDATE {
            stats.1 += 1;
            continue;
        }
        stats.0 += 1;
        let tile = rd_encode::tiles::TileOut {
            x: header.x,
            y: header.y,
            w: header.w,
            h: header.h,
            fmt: header.fmt,
            data: data.to_vec(),
        };
        let decoded = rd_encode::tiles::decode_tile(&tile).context("瓦片解码失败")?;
        let (tw, th) = (decoded.width as usize, decoded.height as usize);
        anyhow::ensure!(
            decoded.bgra.len() == tw * th * 4,
            "瓦片解码字节数不符: {}",
            decoded.bgra.len()
        );
        anyhow::ensure!(
            header.x as usize + tw <= w && header.y as usize + th <= h,
            "瓦片越界: ({},{}) {}x{}",
            header.x,
            header.y,
            tw,
            th
        );
        for row in 0..th {
            let cy = header.y as usize + row;
            let dst = (cy * w + header.x as usize) * 4;
            let src = row * tw * 4;
            canvas[dst..dst + tw * 4].copy_from_slice(&decoded.bgra[src..src + tw * 4]);
            let mask = &mut covered[cy * w + header.x as usize..][..tw];
            covered_count += mask.iter().filter(|c| !**c).count();
            mask.fill(true);
        }
    }
    println!(
        "[e2e] collect_tiles: 数据瓦片 {}，失效标记 {}，其他窗口 {}，覆盖 {} px",
        stats.0, stats.1, stats.2, covered_count
    );
    Ok((canvas, covered))
}

/// 用 rd-capture-win 直接抓 testwin 客户区一帧（BGRA，`stride` 行距）作比对源。
pub fn capture_source_frame(hwnd: u64) -> Result<rd_capture_win::CapturedFrame> {
    let (capture, rx) = rd_capture_win::WindowCapture::start(
        hwnd as isize,
        rd_capture_win::CaptureOptions { max_fps: 30 },
    )
    .context("启动源图捕获失败")?;
    let frame = rx
        .recv_timeout(Duration::from_secs(3))
        .context("等待源图首帧超时")?;
    drop(capture);
    Ok(frame)
}

/// 读取 testwin 当前标题（测试进程本地枚举窗口，不经 Agent）。
pub fn testwin_title(hwnd: u64) -> Result<String> {
    let wins = rd_window_win::list_windows()?;
    wins.iter()
        .find(|w| w.hwnd as u64 == hwnd)
        .map(|w| w.title.clone())
        .ok_or_else(|| anyhow::anyhow!("窗口列表中找不到 testwin（hwnd {hwnd:#x}）"))
}

// ---------------------------------------------------------------------------
// H.264 解码（ffmpeg CLI）：验证 keyframe 比特流合法、尺寸正确
// ---------------------------------------------------------------------------

/// 解码一段 H.264 Annex-B（通常为单个 IDR AU），返回视频分辨率。
///
/// 解码器取 `$FFMPEG_DIR/bin/ffmpeg.exe`，否则回退 PATH（CI 已把 BtbN 包的
/// bin 追加进 PATH）。
pub fn decode_h264_size(au: &[u8]) -> Result<(u32, u32)> {
    let ffmpeg = ffmpeg_exe()?;
    let dir = tempfile::tempdir().context("创建解码临时目录失败")?;
    let input = dir.path().join("au.h264");
    let output = dir.path().join("frame.ppm");
    std::fs::write(&input, au)?;
    let out = Command::new(&ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "h264", "-i"])
        .arg(&input)
        .args(["-frames:v", "1", "-f", "image2"])
        .arg(&output)
        .output()
        .with_context(|| format!("运行 ffmpeg 失败: {}", ffmpeg.display()))?;
    anyhow::ensure!(
        out.status.success(),
        "ffmpeg 解码失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = std::fs::read(&output).context("ffmpeg 未产出 PPM 帧文件")?;
    parse_ppm_size(&raw)
}

fn ffmpeg_exe() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("FFMPEG_DIR") {
        let p = PathBuf::from(dir).join("bin").join("ffmpeg.exe");
        if p.exists() {
            return Ok(p);
        }
    }
    let where_out = Command::new("where").arg("ffmpeg").output();
    if let Ok(out) = where_out {
        if out.status.success() {
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            if !first.is_empty() {
                return Ok(PathBuf::from(first));
            }
        }
    }
    bail!(
        "找不到 ffmpeg.exe（FFMPEG_DIR 未设置且 PATH 无 ffmpeg）；\
         本机由 ~/.cargo/config.toml [env] 注入，CI 由 ci.yml 配置"
    )
}

/// 解析二进制 PPM（P6）头部的宽高。
///
/// 头部为 ASCII（`P6␠<w>␠<h>␠255␠`，长度随分辨率位数变化），其后即二进制
/// 像素——只对头部窗口做 lossy 解码，绝不能把像素字节交给 UTF-8 校验。
fn parse_ppm_size(raw: &[u8]) -> Result<(u32, u32)> {
    let head = &raw[..raw.len().min(64)];
    let text = String::from_utf8_lossy(head);
    let mut it = text.split_whitespace();
    let magic = it.next().unwrap_or_default();
    anyhow::ensure!(magic == "P6", "PPM magic 异常: {magic}");
    let w: u32 = it
        .next()
        .context("PPM 缺宽度")?
        .parse()
        .context("PPM 宽度非法")?;
    let h: u32 = it
        .next()
        .context("PPM 缺高度")?
        .parse()
        .context("PPM 高度非法")?;
    Ok((w, h))
}
