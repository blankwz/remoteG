//! 数据流 → tauri::ipc::Channel 泵（T-005）。
//!
//! 帧（25B 帧头 + AU 原样）以 [`InvokeResponseBody::Raw`] 推给前端；每秒打印
//! 字节/帧数统计，供 T-007 评估 Channel 吞吐是否够 1080p@60。

use std::time::{Duration, Instant};

use bytes::{BufMut, BytesMut};
use rd_transport::framing::{VIDEO_HEADER_LEN, encode_video_header};
use rd_transport::{DataFrame, DataRx};
use tauri::ipc::{Channel, InvokeResponseBody};

/// 统计打印周期。
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// 单周期吞吐计数。
#[derive(Debug, Default)]
struct Throughput {
    bytes: u64,
    frames: u64,
}

impl Throughput {
    fn record(&mut self, bytes: u64) {
        self.bytes += bytes;
        self.frames += 1;
    }
}

/// 泵一条视频数据流到前端 Channel；对端 FIN 后正常返回。
///
/// `label` 仅用于日志（形如 `127.0.0.1:21118#4`）。
pub async fn pump_video(
    mut rx: DataRx,
    on_frame: Channel<InvokeResponseBody>,
    label: String,
) -> anyhow::Result<()> {
    let mut stats = Throughput::default();
    let mut window = Instant::now();
    loop {
        match rx.recv().await {
            Ok(Some(DataFrame::Video(header, au))) => {
                let mut frame = BytesMut::with_capacity(VIDEO_HEADER_LEN + au.len());
                frame.put_slice(&encode_video_header(&header, au.len()));
                frame.put_slice(&au);
                stats.record(frame.len() as u64);
                on_frame
                    .send(InvokeResponseBody::Raw(frame.into()))
                    .map_err(|e| anyhow::anyhow!("channel send: {e}"))?;
            }
            // DataRx 按 kind 解帧，Video 流不会给出其他变体；防御性忽略
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::info!(stream = %label, "video stream finished");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        if window.elapsed() >= REPORT_INTERVAL {
            tracing::info!(
                stream = %label,
                bytes_per_s = stats.bytes,
                frames_per_s = stats.frames,
                "channel throughput (T-007 evaluation)"
            );
            stats = Throughput::default();
            window = Instant::now();
        }
    }
}
