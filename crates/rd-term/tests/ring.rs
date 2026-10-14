//! RingBuffer 单测：写入/回放、环绕覆盖、溢出截断边界（T-014）。

use rd_term::RingBuffer;

#[test]
fn push_snapshot_roundtrip() {
    let mut r = RingBuffer::with_capacity(64);
    r.push(b"hello\nworld");
    assert_eq!(r.snapshot(), b"hello\nworld");
}

#[test]
fn snapshot_empty_is_empty() {
    let mut r = RingBuffer::with_capacity(16);
    assert!(r.snapshot().is_empty());
    r.push(b"");
    assert!(r.snapshot().is_empty());
}

#[test]
fn wrap_around_keeps_latest_bytes() {
    let mut r = RingBuffer::with_capacity(8);
    r.push(b"abcdefgh");
    r.push(b"ij");
    assert_eq!(r.snapshot(), b"cdefghij");
    r.push(b"klmno");
    assert_eq!(r.snapshot(), b"hijklmno");
}

#[test]
fn oversized_input_keeps_tail() {
    let mut r = RingBuffer::with_capacity(8);
    r.push(b"0123456789ABCDEF");
    assert_eq!(r.snapshot(), b"89ABCDEF");
}

#[test]
fn truncate_skips_half_escape_to_line_boundary() {
    let mut r = RingBuffer::with_capacity(16);
    // 14 字节："\x1b[31m" + "ABCDEFGH" + "\n"
    r.push(b"\x1b[31mABCDEFGH\n");
    // 追加 5 字节溢出 3 字节，截断点恰好落在 "\x1b[31m" 转义序列中间。
    r.push(b"tail\n");
    let snap = r.snapshot();
    assert!(snap.ends_with(b"tail\n"), "tail lost: {snap:?}");
    assert!(
        snap.is_empty() || snap[0] == b'\n' || snap[0] == 0x1b,
        "replay must not start mid-escape-sequence: {snap:?}"
    );
}

#[test]
fn truncate_resumes_at_escape_boundary() {
    let mut r = RingBuffer::with_capacity(8);
    // 恰好 8 字节填满：两个换行 + 一个完整序列 + 'X'
    r.push(b"\n\n\x1b[31mX");
    // 追加 4 字节溢出 4 字节，截断点落在 "[31m" 中段；
    // 安全边界应回退到新的 ESC 序列起始。
    r.push(b"AB\x1bC");
    assert_eq!(r.snapshot(), b"\x1bC");
}

#[test]
fn truncate_without_boundary_keeps_start() {
    let mut r = RingBuffer::with_capacity(8);
    r.push(b"abcdefg");
    r.push(b"hij");
    // 截断后近端无 \n/ESC：放弃对齐，保留原起点而非丢更多数据。
    assert_eq!(r.snapshot(), b"cdefghij");
}

#[test]
fn len_and_capacity_track_usage() {
    let mut r = RingBuffer::with_capacity(16);
    assert!(r.is_empty());
    r.push(b"abc");
    assert_eq!(r.len(), 3);
    assert_eq!(r.capacity(), 16);
    r.push(&[0u8; 32]);
    assert_eq!(r.len(), 16);
}
