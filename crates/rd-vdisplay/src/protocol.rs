//! virtual-display-rs 驱动管道协议（master 0.4.0-dev 形态）。
//!
//! 线上格式（与上游 `driver-ipc` crate 逐字段一致）：
//! - 管道：`\\.\pipe\virtualdisplaydriver`，**byte 模式**，JSON 消息以 `0x04`
//!   分隔（EOF 字节，非协议难点——服务端按字节流切分，客户端单次/多次写均可）；
//! - 客户端 → 驱动：`{"Notify":[{monitor...}]}` / `{"Remove":[id...]}` /
//!   `{"RemoveAll"}` / `"State"`（请求当前状态）；
//! - 驱动 → 客户端：`{"State":[monitor...]}`（应答 State 请求）、
//!   `{"Changed":[monitor...]}`（别的客户端改了状态时推给本客户端）。
//!
//! 关键约束（实测记录在 `03_architecture/虚拟屏权限实测.md` §5/§8）：
//! - `id` 即 IddCx ConnectorIndex，**有效范围 0~9**；超界时驱动建屏失败但
//!   Notify 仍返回成功；
//! - `Notify` 是全量替换语义（不是增量）：要么带上全部 monitor，要么删掉；
//! - 同一时刻一个 id 只能有一个 monitor。

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 消息分隔字节。
pub const EOF: u8 = 0x04;

/// 默认管道名（不含 `\\.\pipe\` 前缀）。
pub const DEFAULT_PIPE_NAME: &str = "virtualdisplaydriver";

/// 驱动支持的 monitor id 上限（IddCx ConnectorIndex 有效 0~9）。
pub const MAX_MONITOR_ID: u32 = 9;

/// 一个 monitor 的一条显示模式（与上游 `Mode` 一致）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_rates: Vec<u32>,
}

/// 一个 monitor（与上游 `Monitor` 一致）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Monitor {
    pub id: u32,
    pub name: Option<String>,
    pub enabled: bool,
    pub modes: Vec<Mode>,
}

/// 客户端 → 驱动命令（serde 外部标签，与上游一致）。
#[derive(Debug, Clone, Serialize)]
pub enum DriverCommand<'a> {
    Notify(&'a [Monitor]),
    Remove(&'a [u32]),
    RemoveAll,
}

/// 驱动 → 客户端的入站消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// `State` 请求的应答（也是新连接后的初始状态）。
    State(Vec<Monitor>),
    /// 其他客户端改变了状态时的推送。
    Changed(Vec<Monitor>),
}

/// 把一条命令编码成线上字节（JSON + EOF 分隔）。
pub fn encode_command(cmd: &DriverCommand<'_>) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(cmd)?;
    bytes.push(EOF);
    Ok(bytes)
}

/// 请求状态命令的线上字节（`"State"` 是裸字符串枚举，与上游一致）。
pub fn encode_state_request() -> Vec<u8> {
    let mut bytes = br#""State""#.to_vec();
    bytes.push(EOF);
    bytes
}

/// 解析一个已切分的帧。
///
/// 未知/损坏帧返回 `None`（前向兼容：驱动升级新增命令不应打断连接）。
pub fn parse_incoming(frame: &[u8]) -> Option<Incoming> {
    let value: serde_json::Value = serde_json::from_slice(frame).ok()?;
    if let Some(list) = value.get("State") {
        let monitors = serde_json::from_value(list.clone()).ok()?;
        return Some(Incoming::State(monitors));
    }
    if let Some(list) = value.get("Changed") {
        let monitors = serde_json::from_value(list.clone()).ok()?;
        return Some(Incoming::Changed(monitors));
    }
    None
}

/// 从输入流里收集的那条入站消息。
///
/// 调用方把收到的每个完整帧（按 EOF 切分）送进来，等 `State` 应答时跳过
/// 无关的 `Changed` 帧。
pub fn wait_for_incoming(
    rx: &Receiver<Vec<u8>>,
    timeout: Duration,
    mut want: impl FnMut(&Incoming) -> bool,
) -> Option<Incoming> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(std::time::Instant::now())?;
        match rx.recv_timeout(left) {
            Ok(frame) => match parse_incoming(&frame) {
                Some(msg) if want(&msg) => return Some(msg),
                _ => continue,
            },
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// 按 EOF 分隔把字节流切成帧：喂多少切多少，未满一帧的部分留在缓冲里。
#[derive(Debug, Default)]
pub struct FrameSplitter {
    buf: Vec<u8>,
}

impl FrameSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一段字节，返回其中所有完整帧（按到达顺序）。
    ///
    /// 数据本身不含 EOF 字节——JSON 里 `0x04` 只能以 `\u0004` 转义出现，
    /// 不会与分隔符冲突（上游同样按字节扫描）。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        let mut start = 0;
        for (i, byte) in self.buf.iter().enumerate() {
            if *byte == EOF {
                if i > start {
                    frames.push(self.buf[start..i].to_vec());
                }
                start = i + 1;
            }
        }
        if start > 0 {
            self.buf.drain(..start);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn mon(id: u32, width: u32) -> Monitor {
        Monitor {
            id,
            name: Some("rd-vdisplay".into()),
            enabled: true,
            modes: vec![Mode {
                width,
                height: 950,
                refresh_rates: vec![60],
            }],
        }
    }

    #[test]
    fn notify_wire_format_matches_upstream() {
        let mons = [mon(5, 540)];
        let bytes = encode_command(&DriverCommand::Notify(&mons)).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with(r#"{"Notify":"#), "{text}");
        assert!(text.ends_with('\u{4}'));
        // 字段名与上游一致（驱动按 serde 默认名反序列化）
        for key in [
            "\"id\"",
            "\"name\"",
            "\"enabled\"",
            "\"modes\"",
            "\"width\"",
            "\"height\"",
            "\"refresh_rates\"",
        ] {
            assert!(text.contains(key), "缺少字段 {key}: {text}");
        }
    }

    #[test]
    fn remove_and_remove_all_wire_format() {
        let bytes = encode_command(&DriverCommand::Remove(&[3, 7])).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.trim_end_matches('\u{4}'), r#"{"Remove":[3,7]}"#);
        // 单元变体是裸字符串（与上游 `DriverCommand::RemoveAll` 的 serde 形态一致）
        let bytes = encode_command(&DriverCommand::RemoveAll).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap().trim_end_matches('\u{4}'),
            r#""RemoveAll""#
        );
    }

    #[test]
    fn state_request_is_bare_string() {
        let bytes = encode_state_request();
        assert_eq!(bytes, vec![b'"', b'S', b't', b'a', b't', b'e', b'"', EOF]);
    }

    #[test]
    fn parse_state_and_changed() {
        let frames = [
            (r#"{"State":[{"id":5,"name":"rd-vdisplay","enabled":true,"modes":[{"width":540,"height":950,"refresh_rates":[60]}]}]}"#.as_bytes(), true),
            (r#"{"Changed":[]}"#.as_bytes(), false),
        ];
        for (frame, is_state) in frames {
            let parsed = parse_incoming(frame).expect("应能解析");
            match (parsed, is_state) {
                (Incoming::State(mons), true) => assert_eq!(mons, vec![mon(5, 540)]),
                (Incoming::Changed(mons), false) => assert!(mons.is_empty()),
                (other, _) => panic!("形态不符: {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_and_garbage_frames_are_ignored() {
        assert!(parse_incoming(br#"{"SomethingElse":1}"#).is_none());
        assert!(parse_incoming(b"not json").is_none());
        assert!(parse_incoming(b"{}").is_none());
    }

    #[test]
    fn splitter_handles_partial_and_batched_frames() {
        let mut splitter = FrameSplitter::new();
        // 半帧：不产出
        assert!(splitter.push(br#"{"Changed":["#).is_empty());
        // 补完（含分隔符）：产出一帧
        let mut tail = br#"]}"#.to_vec();
        tail.push(EOF);
        let frames = splitter.push(&tail);
        assert_eq!(frames.len(), 1);
        assert!(
            parse_incoming(&frames[0]).is_some(),
            "frame = {:?}",
            String::from_utf8_lossy(&frames[0])
        );
        // 两帧挤在一次读里：产出两帧
        let mut batch = br#"{"Changed":[]}"#.to_vec();
        batch.push(EOF);
        batch.extend_from_slice(br#"{"Changed":[]}"#);
        batch.push(EOF);
        let frames = splitter.push(&batch);
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn splitter_ignores_empty_frame() {
        // 连续两个 EOF 之间无内容 → 不产出空帧（脏数据防御）
        let frames = FrameSplitter::new().push(&[EOF, EOF]);
        assert!(frames.is_empty());
    }

    #[test]
    fn wait_for_incoming_skips_unrelated_and_times_out() {
        let (tx, rx) = mpsc::channel();
        tx.send(br#"{"Changed":[]}"#.to_vec()).unwrap();
        assert!(
            wait_for_incoming(&rx, Duration::from_millis(50), |i| matches!(
                i,
                Incoming::State(_)
            ))
            .is_none()
        );
        // 有 State 时命中
        let (tx, rx) = mpsc::channel();
        tx.send(br#"{"State":[]}"#.to_vec()).unwrap();
        assert!(matches!(
            wait_for_incoming(&rx, Duration::from_millis(50), |i| matches!(
                i,
                Incoming::State(_)
            )),
            Some(Incoming::State(_))
        ));
    }
}
