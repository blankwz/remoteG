/**
 * 帧头解析单测（T-005）：字节布局必须与 05_api/协议契约.md §3 逐字段一致
 * （u64 LE window_id, u32 LE seq, u64 pts_us, u8 flags(bit0=key), u32 LE len）。
 * 另含 codec string 选择（444 协商命中映射 High 4:4:4，未知值回退 420），
 * 以及 resetForKeyframe 契约（P0-3 后台标签降流的恢复路径）。
 */
import { beforeEach, describe, expect, it } from "vitest";
import {
  H264_CODEC_420,
  H264_CODEC_444,
  VIDEO_HEADER_LEN,
  H264Decoder,
  codecStringFor,
  parseVideoHeader,
} from "./decoder";

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

describe("codecStringFor", () => {
  it("maps H264_444 ack codec to High 4:4:4 codec string", () => {
    expect(codecStringFor("H264_444")).toBe(H264_CODEC_444);
    expect(codecStringFor("H264_444")).toBe("avc1.F40033");
  });

  it("keeps 420 baseline for H264_420 ack codec", () => {
    expect(codecStringFor("H264_420")).toBe(H264_CODEC_420);
    expect(codecStringFor("H264_420")).toBe("avc1.42E033");
  });

  it("falls back to 420 for unknown / unsupported / empty ack codecs", () => {
    // ack_to_dto 的其他可能取值与未来扩展均回退默认解码路径
    for (const codec of ["HEVC_444", "AV1", "unknown(99)", "", "h264_444"]) {
      expect(codecStringFor(codec)).toBe(H264_CODEC_420);
    }
  });

  it("falls back to 420 when ack (or codec field) is absent", () => {
    expect(codecStringFor(null)).toBe(H264_CODEC_420);
    expect(codecStringFor(undefined)).toBe(H264_CODEC_420);
  });
});

/**
 * resetForKeyframe（P0-3 后台标签降流）。
 *
 * node 环境无 WebCodecs：以最小替身替换全局 VideoDecoder/EncodedVideoChunk，
 * 驱动 push() 验证恢复路径的核心不变量——「跳帧后参考链已断，重置后的
 * delta 必须丢弃，直到下个 keyframe 才续播」，否则切回标签会先渲染一段
 * 参考帧缺失的花屏。
 */

/** 替身解码器：记录 decode 的 chunk 类型，供断言。 */
class StubVideoDecoder {
  static readonly all: StubVideoDecoder[] = [];
  readonly decoded: string[] = [];

  constructor(cfg: { output: (f: unknown) => void; error: (e: unknown) => void }) {
    void cfg;
    StubVideoDecoder.all.push(this);
  }

  configure(): void {}

  decode(chunk: { type: string }): void {
    this.decoded.push(chunk.type);
  }

  close(): void {}
}

class StubEncodedVideoChunk {
  readonly type: string;
  constructor(desc: { type: string; timestamp: number; data: Uint8Array }) {
    this.type = desc.type;
  }
}

/** 安装全局替身（decoder.ts 在构造时才引用全局符号）。 */
function installStubs(): void {
  const g = globalThis as unknown as Record<string, unknown>;
  g.VideoDecoder = StubVideoDecoder;
  g.EncodedVideoChunk = StubEncodedVideoChunk;
}

/** 构造一帧完整视频数据（25B 头 + 4B AU 假负载）。 */
function videoFrame(keyframe: boolean): ArrayBuffer {
  return buildFrame({ ...SAMPLE, flags: keyframe ? 1 : 0, payloadLen: 4 }, new Uint8Array(4));
}

function makeDecoder(): { decoder: H264Decoder; stub: StubVideoDecoder } {
  const decoder = new H264Decoder(
    {} as HTMLCanvasElement,
    (_stats: import("./decoder").DecodeStats) => undefined,
  );
  const stub = StubVideoDecoder.all[StubVideoDecoder.all.length - 1];
  if (stub === undefined) {
    throw new Error("stub decoder 未注册");
  }
  return { decoder, stub };
}

beforeEach(() => {
  StubVideoDecoder.all.length = 0;
  installStubs();
});

describe("H264Decoder.resetForKeyframe（P0-3 恢复路径）", () => {
  it("正常收流：keyframe 先行，delta 依序解码", () => {
    const { decoder, stub } = makeDecoder();
    decoder.push(videoFrame(true));
    decoder.push(videoFrame(false));
    expect(stub.decoded).toEqual(["key", "delta"]);
  });

  it("重置后 delta 被丢弃，直到下个 keyframe 才续播（参考链已断）", () => {
    const { decoder, stub } = makeDecoder();
    decoder.push(videoFrame(true));
    decoder.resetForKeyframe();
    // 重置后先来的 delta：不可解（无参考），必须丢弃
    decoder.push(videoFrame(false));
    expect(stub.decoded).toEqual(["key"]);
    // 下个 keyframe：恢复解码
    decoder.push(videoFrame(true));
    expect(stub.decoded).toEqual(["key", "key"]);
  });

  it("未见 keyframe 的全新解码器上重置无副作用", () => {
    const { decoder, stub } = makeDecoder();
    decoder.resetForKeyframe();
    decoder.push(videoFrame(false)); // 仍处等关键帧态：delta 照常丢弃
    expect(stub.decoded).toEqual([]);
    decoder.push(videoFrame(true));
    expect(stub.decoded).toEqual(["key"]);
  });
});
