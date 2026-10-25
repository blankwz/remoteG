//! T-005 视频链路 spike：QUIC 连接 → 控制流 Hello → 数据流泵到前端 Channel。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rd_proto::v0::{Codec, Envelope, Hello, envelope};
use rd_transport::{Conn, StreamKind, connect};
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::conn::ConnManager;
use crate::video_pump;

/// rd.proto v0。
const PROTO_VER: u32 = 0;

fn next_req_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 连接 rd-agent（spike）：建立 QUIC、发送 Hello，并在后台泵数据流到 `on_frame`。
///
/// 返回 `Ok(())` 表示连接与握手建立成功；数据流随后异步到达。重复对同一地址
/// 调用会关闭并替换旧连接。
#[tauri::command]
pub async fn spike_connect(
    addr: String,
    on_frame: Channel<InvokeResponseBody>,
    state: State<'_, ConnManager>,
) -> Result<(), String> {
    let sock: SocketAddr = addr
        .parse()
        .map_err(|e| format!("invalid addr {addr:?}: {e}"))?;

    let conn = connect(sock, state.identity(), None)
        .await
        .map_err(|e| format!("connect {addr}: {e:#}"))?;
    tracing::info!(
        addr = %addr,
        peer = %conn.peer_fingerprint().short_hex(),
        "spike connected (fingerprint not yet verified: pairing is T-013)"
    );

    let device_id = addr.clone();
    if let Some(old) = state.insert(device_id, Arc::new(conn.clone())) {
        old.close(0, "replaced by newer connection");
    }

    // 客户端不等服务端 accept_control：open 后立即写 Hello，
    // 首字节落地即触发服务端 accept_bi 返回（见 rd-transport conn.rs 注释）。
    let (mut ctrl_tx, mut ctrl_rx) = conn
        .open_control()
        .await
        .map_err(|e| format!("open control stream: {e:#}"))?;
    let hello = Envelope {
        req_id: next_req_id(),
        msg: Some(envelope::Msg::Hello(Hello {
            proto_ver: PROTO_VER,
            device_name: "rd-controller".into(),
            decode_caps: vec![Codec::H264420 as i32],
            supports_444: false,
            media_fec: false,
        })),
    };
    ctrl_tx
        .send(&hello)
        .await
        .map_err(|e| format!("send hello: {e:#}"))?;
    tracing::info!(addr = %addr, "hello sent");

    // 控制流回读：M0 仅记录（配对/窗口控制在 T-019/T-020 接管）
    tauri::async_runtime::spawn(async move {
        loop {
            match ctrl_rx.recv().await {
                Ok(Some(env)) => {
                    tracing::debug!(req_id = env.req_id, "control envelope received");
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("control stream ended: {e:#}");
                    break;
                }
            }
        }
    });

    // 数据流 accept 循环：连接断开时整体退出
    let label = addr.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = accept_loop(conn, label, on_frame).await {
            tracing::warn!("data streams ended: {e:#}");
        }
    });

    Ok(())
}

async fn accept_loop(
    conn: Conn,
    device_id: String,
    on_frame: Channel<InvokeResponseBody>,
) -> anyhow::Result<()> {
    loop {
        let (kind, stream_id, rx) = conn.accept_data().await?;
        tracing::info!(device = %device_id, stream_id, ?kind, "data stream accepted");
        match kind {
            StreamKind::Video => {
                let channel = on_frame.clone();
                let label = format!("{device_id}#{stream_id}");
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = video_pump::pump_video(rx, channel, label).await {
                        tracing::warn!("video pump ended: {e:#}");
                    }
                });
            }
            other => {
                tracing::warn!(kind = ?other, "unexpected data stream kind (ignored in spike)");
            }
        }
    }
}
