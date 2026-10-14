//! 帧分流（05_api/协议契约.md §3）：
//! - 控制流：`u32 LE 长度 + protobuf Envelope`；
//! - 数据流：流开头 1 字节 [`StreamKind`]，其后每帧带帧头；
//!   视频/瓦片帧头 25 字节（小端，末尾 `u32 len`），终端帧头仅 `u32 len`。

use prost::Message;

use rd_proto::v0::Envelope;
#[cfg(test)]
use rd_proto::v0::Hello;

/// 数据流种类（流开头 1 字节 kind）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StreamKind {
    /// 视频（Agent→控制端 单向）
    Video = 1,
    /// 瓦片（Agent→控制端 单向）
    Tile = 2,
    /// 终端（双向）
    Terminal = 3,
}

impl StreamKind {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Video),
            2 => Some(Self::Tile),
            3 => Some(Self::Terminal),
            _ => None,
        }
    }
}

/// 未知的数据流种类字节。
#[derive(Debug, thiserror::Error)]
#[error("unknown stream kind: {0}")]
pub struct UnknownStreamKind(pub u8);

impl TryFrom<u8> for StreamKind {
    type Error = UnknownStreamKind;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        Self::from_u8(v).ok_or(UnknownStreamKind(v))
    }
}

/// 视频帧头字段（05_api §3）：`u64 window_id, u32 seq, u64 pts_us, u8 flags(bit0=key), u32 len`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub window_id: u64,
    pub seq: u32,
    pub pts_us: u64,
    pub keyframe: bool,
}

/// 瓦片帧头字段（05_api §3）：
/// `u64 window_id, u32 frame_seq, u16 x,y,w,h, u8 fmt(0=QOI,1=zstd-BGRA), u32 len`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileHeader {
    pub window_id: u64,
    pub frame_seq: u32,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub fmt: u8,
}

/// 数据流解出的一帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataFrame {
    Video(VideoHeader, bytes::Bytes),
    Tile(TileHeader, bytes::Bytes),
    Raw(bytes::Bytes),
}

pub const VIDEO_HEADER_LEN: usize = 25;
pub const TILE_HEADER_LEN: usize = 25;
pub const LEN_PREFIX_LEN: usize = 4;
/// 控制帧（Envelope）长度上限，防对端恶意超长帧。
pub const MAX_CONTROL_FRAME_LEN: usize = 8 * 1024 * 1024;
/// 数据帧负载长度上限。
pub const MAX_DATA_FRAME_LEN: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame header too short: need {need} bytes, got {got}")]
    HeaderTooShort { need: usize, got: usize },
    #[error("frame too large: {len} bytes (max {max})")]
    FrameTooLarge { len: usize, max: usize },
    #[error(transparent)]
    UnknownStreamKind(#[from] UnknownStreamKind),
}

/// 编码视频帧头（含 `u32 len` 负载长度，小端，共 25 字节）。
pub fn encode_video_header(h: &VideoHeader, payload_len: usize) -> [u8; VIDEO_HEADER_LEN] {
    let mut out = [0u8; VIDEO_HEADER_LEN];
    out[0..8].copy_from_slice(&h.window_id.to_le_bytes());
    out[8..12].copy_from_slice(&h.seq.to_le_bytes());
    out[12..20].copy_from_slice(&h.pts_us.to_le_bytes());
    out[20] = u8::from(h.keyframe);
    out[21..25].copy_from_slice(&trunc_len(payload_len).to_le_bytes());
    out
}

/// 解码视频帧头，返回 (头, 负载长度)。
pub fn decode_video_header(buf: &[u8]) -> Result<(VideoHeader, u32), FrameError> {
    if buf.len() < VIDEO_HEADER_LEN {
        return Err(FrameError::HeaderTooShort {
            need: VIDEO_HEADER_LEN,
            got: buf.len(),
        });
    }
    let header = VideoHeader {
        window_id: u64::from_le_bytes(buf[0..8].try_into().expect("8 bytes")),
        seq: u32::from_le_bytes(buf[8..12].try_into().expect("4 bytes")),
        pts_us: u64::from_le_bytes(buf[12..20].try_into().expect("8 bytes")),
        keyframe: buf[20] & 0x01 != 0,
    };
    let len = u32::from_le_bytes(buf[21..25].try_into().expect("4 bytes"));
    Ok((header, len))
}

/// 编码瓦片帧头（含 `u32 len` 负载长度，小端，共 25 字节）。
pub fn encode_tile_header(h: &TileHeader, payload_len: usize) -> [u8; TILE_HEADER_LEN] {
    let mut out = [0u8; TILE_HEADER_LEN];
    out[0..8].copy_from_slice(&h.window_id.to_le_bytes());
    out[8..12].copy_from_slice(&h.frame_seq.to_le_bytes());
    out[12..14].copy_from_slice(&h.x.to_le_bytes());
    out[14..16].copy_from_slice(&h.y.to_le_bytes());
    out[16..18].copy_from_slice(&h.w.to_le_bytes());
    out[18..20].copy_from_slice(&h.h.to_le_bytes());
    out[20] = h.fmt;
    out[21..25].copy_from_slice(&trunc_len(payload_len).to_le_bytes());
    out
}

/// 解码瓦片帧头，返回 (头, 负载长度)。
pub fn decode_tile_header(buf: &[u8]) -> Result<(TileHeader, u32), FrameError> {
    if buf.len() < TILE_HEADER_LEN {
        return Err(FrameError::HeaderTooShort {
            need: TILE_HEADER_LEN,
            got: buf.len(),
        });
    }
    let header = TileHeader {
        window_id: u64::from_le_bytes(buf[0..8].try_into().expect("8 bytes")),
        frame_seq: u32::from_le_bytes(buf[8..12].try_into().expect("4 bytes")),
        x: u16::from_le_bytes(buf[12..14].try_into().expect("2 bytes")),
        y: u16::from_le_bytes(buf[14..16].try_into().expect("2 bytes")),
        w: u16::from_le_bytes(buf[16..18].try_into().expect("2 bytes")),
        h: u16::from_le_bytes(buf[18..20].try_into().expect("2 bytes")),
        fmt: buf[20],
    };
    let len = u32::from_le_bytes(buf[21..25].try_into().expect("4 bytes"));
    Ok((header, len))
}

/// 编码控制流整帧：`u32 LE 长度 + Envelope 字节`。
pub fn encode_control_frame(e: &Envelope) -> Result<Vec<u8>, FrameError> {
    let body = e.encode_to_vec();
    if body.len() > MAX_CONTROL_FRAME_LEN {
        return Err(FrameError::FrameTooLarge {
            len: body.len(),
            max: MAX_CONTROL_FRAME_LEN,
        });
    }
    let mut out = Vec::with_capacity(LEN_PREFIX_LEN + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn trunc_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帧头字节布局必须与 05_api §3 逐字段一致。
    #[test]
    fn video_header_exact_layout() {
        let h = VideoHeader {
            window_id: 0x0102030405060708,
            seq: 0x0A0B_0C0D,
            pts_us: 0x1112_1314_1516_1718,
            keyframe: true,
        };
        let out = encode_video_header(&h, 0x1A1B_1C1D);
        let expected: [u8; 25] = [
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // window_id LE
            0x0D, 0x0C, 0x0B, 0x0A, // seq LE
            0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, // pts_us LE
            0x01, // flags: bit0=key
            0x1D, 0x1C, 0x1B, 0x1A, // len LE
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn video_header_non_key_flag() {
        let h = VideoHeader {
            window_id: 7,
            seq: 9,
            pts_us: 123_456,
            keyframe: false,
        };
        let out = encode_video_header(&h, 42);
        assert_eq!(out[20], 0);
        assert_eq!(&out[21..25], &42u32.to_le_bytes());
        let (decoded, len) = decode_video_header(&out).expect("decode");
        assert_eq!(decoded, h);
        assert_eq!(len, 42);
    }

    /// 帧头字节布局必须与 05_api §3 逐字段一致。
    #[test]
    fn tile_header_exact_layout() {
        let h = TileHeader {
            window_id: 1,
            frame_seq: 2,
            x: 3,
            y: 4,
            w: 5,
            h: 6,
            fmt: 7,
        };
        let out = encode_tile_header(&h, 8);
        let expected: [u8; 25] = [
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // window_id LE
            0x02, 0x00, 0x00, 0x00, // frame_seq LE
            0x03, 0x00, // x LE
            0x04, 0x00, // y LE
            0x05, 0x00, // w LE
            0x06, 0x00, // h LE
            0x07, // fmt
            0x08, 0x00, 0x00, 0x00, // len LE
        ];
        assert_eq!(out, expected);
        let (decoded, len) = decode_tile_header(&out).expect("decode");
        assert_eq!(decoded, h);
        assert_eq!(len, 8);
    }

    #[test]
    fn header_decode_rejects_short_buf() {
        assert!(decode_video_header(&[0u8; 24]).is_err());
        assert!(decode_tile_header(&[0u8; 0]).is_err());
    }

    #[test]
    fn stream_kind_byte_roundtrip() {
        for k in [StreamKind::Video, StreamKind::Tile, StreamKind::Terminal] {
            assert_eq!(StreamKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(StreamKind::Video.as_u8(), 1);
        assert_eq!(StreamKind::Tile.as_u8(), 2);
        assert_eq!(StreamKind::Terminal.as_u8(), 3);
        assert!(StreamKind::from_u8(0).is_none());
        assert!(StreamKind::from_u8(4).is_none());
        assert!(StreamKind::try_from(9u8).is_err());
    }

    #[test]
    fn control_frame_roundtrip() {
        let e = Envelope {
            req_id: 42,
            msg: Some(rd_proto::v0::envelope::Msg::Hello(Hello::default())),
        };
        let frame = encode_control_frame(&e).expect("encode");
        assert_eq!(&frame[..4], &(frame.len() as u32 - 4).to_le_bytes());
        let decoded = Envelope::decode(&frame[4..]).expect("decode");
        assert_eq!(decoded.req_id, 42);
    }
}
