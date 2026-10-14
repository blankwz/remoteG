/**
 * 帧头解析单测（T-005）：字节布局必须与 05_api/协议契约.md §3 逐字段一致
 * （u64 LE window_id, u32 LE seq, u64 pts_us, u8 flags(bit0=key), u32 LE len）。
 */
import { describe, expect, it } from "vitest";
import { VIDEO_HEADER_LEN, parseVideoHeader } from "./decoder";

interface HeaderInput {
  windowId: bigint;
  seq: number;
  ptsUs: bigint;
  flags: number;
  payloadLen: number;
}

function buildHeader(input: HeaderInput): ArrayBuffer {
  const buf = new ArrayBuffer(VIDEO_HEADER_LEN);
  const view = new DataView(buf);
  view.setBigUint64(0, input.windowId, true);
  view.setUint32(8, input.seq, true);
  view.setBigUint64(12, input.ptsUs, true);
  view.setUint8(20, input.flags);
  view.setUint32(21, input.payloadLen, true);
  return buf;
}

function buildFrame(input: HeaderInput, au: Uint8Array): ArrayBuffer {
  const buf = new Uint8Array(VIDEO_HEADER_LEN + au.length);
  buf.set(new Uint8Array(buildHeader(input)), 0);
  buf.set(au, VIDEO_HEADER_LEN);
  return buf.buffer;
}

const SAMPLE: HeaderInput = {
  windowId: 0x0102030405060708n,
  seq: 0x0a0b0c0d,
  ptsUs: 1234567n,
  flags: 0x01,
  payloadLen: 4096,
};

describe("parseVideoHeader", () => {
  it("parses a keyframe header per 05_api §3 layout", () => {
    const header = parseVideoHeader(
      buildFrame(SAMPLE, new Uint8Array(SAMPLE.payloadLen)),
    );
    expect(header.windowId).toBe(0x0102030405060708n);
    expect(header.seq).toBe(0x0a0b0c0d);
    expect(header.ptsUs).toBe(1234567n);
    expect(header.keyframe).toBe(true);
    expect(header.payloadLen).toBe(4096);
  });

  it("flags non-key frames when bit0 is clear", () => {
    const header = parseVideoHeader(buildHeader({ ...SAMPLE, flags: 0x00 }));
    expect(header.keyframe).toBe(false);
  });

  it("ignores reserved flag bits beyond bit0", () => {
    // 帧头只定义 bit0=key，保留位不得影响 keyframe 判定
    const header = parseVideoHeader(buildHeader({ ...SAMPLE, flags: 0xfe }));
    expect(header.keyframe).toBe(false);
    const header2 = parseVideoHeader(buildHeader({ ...SAMPLE, flags: 0xff }));
    expect(header2.keyframe).toBe(true);
  });

  it("keeps u64 precision for max window_id and pts_us", () => {
    const header = parseVideoHeader(
      buildHeader({
        ...SAMPLE,
        windowId: 0xffffffffffffffffn,
        ptsUs: 0xffffffffffffffffn,
      }),
    );
    expect(header.windowId).toBe(0xffffffffffffffffn);
    expect(header.ptsUs).toBe(0xffffffffffffffffn);
  });

  it("rejects buffers shorter than 25 bytes", () => {
    expect(() => parseVideoHeader(new ArrayBuffer(VIDEO_HEADER_LEN - 1))).toThrow(
      /too short/,
    );
    expect(() => parseVideoHeader(new ArrayBuffer(0))).toThrow(/too short/);
  });

  it("does not treat header-only buffer as an error", () => {
    const header = parseVideoHeader(buildHeader(SAMPLE));
    expect(header.payloadLen).toBe(SAMPLE.payloadLen);
  });
});
