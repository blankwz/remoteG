/**
 * 瓦片层单测（T-021）：帧头字节布局（与 05_api §3 / Rust framing.rs 逐字段
 * 一致）、负载校验、边界裁剪、尺寸变化清层、失效清除，以及 dispatchFrame
 * 的类型路由（视频/瓦片/失效、window_id 寻址、短帧丢弃）。
 *
 * 测试运行于 vitest node 环境（无 canvas），绘制面用 RecordingSurface 记录
 * 操作序列断言。
 */
import { describe, expect, it, vi } from "vitest";
import {
  clipRect,
  FRAME_INVALIDATE,
  FRAME_TILE,
  FRAME_VIDEO,
  parseTileHeader,
  TILE_HEADER_LEN,
  TileLayer,
  tilePayloadLen,
  type Rect,
  type TileHeader,
  type TileSurface,
} from "./TileLayer";
import { dispatchFrame, type FrameSink } from "./WindowGroupView";

/** 记录型绘制面（操作序列断言）。 */
class RecordingSurface implements TileSurface {
  readonly ops: string[] = [];

  putTile(tileRect: Rect, clip: Rect): void {
    this.ops.push(`put ${clip.x},${clip.y},${clip.w},${clip.h} @${tileRect.x},${tileRect.y}`);
  }

  clearRegion(r: Rect): void {
    this.ops.push(`clear ${r.x},${r.y},${r.w},${r.h}`);
  }

  clearAll(): void {
    this.ops.push("clearAll");
  }

  resize(w: number, h: number): void {
    this.ops.push(`resize ${w}x${h}`);
  }
}

/** 构造瓦片帧（类型前缀 + 25B 头 + RGBA 负载）。 */
function tileFrame(
  type: number,
  windowId: bigint,
  x: number,
  y: number,
  w: number,
  h: number,
  frameSeq = 7,
): ArrayBuffer {
  const payload = w * h * 4;
  const buf = new ArrayBuffer(1 + TILE_HEADER_LEN + payload);
  const view = new DataView(buf);
  view.setUint8(0, type);
  view.setBigUint64(1, windowId, true);
  view.setUint32(9, frameSeq, true);
  view.setUint16(13, x, true);
  view.setUint16(15, y, true);
  view.setUint16(17, w, true);
  view.setUint16(19, h, true);
  view.setUint8(21, 0); // fmt（前端已解码，不关心）
  view.setUint32(22, payload, true);
  return buf;
}

/** 构造视频帧（类型前缀 + 25B 视频头 + 4B AU）。 */
function videoFrame(windowId: bigint, key = true): ArrayBuffer {
  const buf = new ArrayBuffer(1 + 25 + 4);
  const view = new DataView(buf);
  view.setUint8(0, FRAME_VIDEO);
  view.setBigUint64(1, windowId, true);
  view.setUint32(9, 1, true);
  view.setBigUint64(13, 100n, true);
  view.setUint8(21, key ? 1 : 0);
  view.setUint32(22, 4, true);
  return buf;
}

describe("parseTileHeader", () => {
  it("按 05_api §3 布局解析（小端，u64 保精度）", () => {
    const header = parseTileHeader(tileFrame(FRAME_TILE, 0x0102030405060708n, 3, 4, 5, 6), 1);
    expect(header.windowId).toBe(0x0102030405060708n);
    expect(header.frameSeq).toBe(7);
    expect(header.x).toBe(3);
    expect(header.y).toBe(4);
    expect(header.w).toBe(5);
    expect(header.h).toBe(6);
  });

  it("偏移解析支持带 1 字节类型前缀的整帧", () => {
    const frame = tileFrame(FRAME_TILE, 9n, 1, 2, 4, 4);
    const header = parseTileHeader(frame, 1);
    expect(header.windowId).toBe(9n);
    expect(header.x).toBe(1);
    expect(header.w).toBe(4);
  });

  it("过短缓冲抛错", () => {
    expect(() => parseTileHeader(new ArrayBuffer(TILE_HEADER_LEN - 1))).toThrow(/too short/);
    expect(() => parseTileHeader(new ArrayBuffer(0))).toThrow(/too short/);
  });
});

describe("tilePayloadLen / clipRect", () => {
  it("负载字节数 = w*h*4", () => {
    expect(tilePayloadLen({ w: 64, h: 64 })).toBe(16384);
    expect(tilePayloadLen({ w: 0, h: 0 })).toBe(0);
  });

  it("clipRect：界内不变、部分裁剪、完全界外为 null", () => {
    expect(clipRect({ x: 0, y: 0, w: 10, h: 10 }, 100, 100)).toEqual({ x: 0, y: 0, w: 10, h: 10 });
    expect(clipRect({ x: -4, y: 98, w: 8, h: 8 }, 100, 100)).toEqual({ x: 0, y: 98, w: 4, h: 2 });
    expect(clipRect({ x: 150, y: 0, w: 8, h: 8 }, 100, 100)).toBeNull();
    expect(clipRect({ x: 0, y: 0, w: 0, h: 8 }, 100, 100)).toBeNull();
  });
});

describe("TileLayer", () => {
  it("setSize 首次与变化时 resize+clearAll，相同尺寸不动", () => {
    const surface = new RecordingSurface();
    const layer = new TileLayer(surface);
    layer.setSize(1920, 1080);
    expect(surface.ops).toEqual(["resize 1920x1080", "clearAll"]);
    surface.ops.length = 0;
    layer.setSize(1920, 1080);
    expect(surface.ops).toEqual([]);
    layer.setSize(640, 480);
    expect(surface.ops).toEqual(["resize 640x480", "clearAll"]);
  });

  it("handleTile 校验负载长度（不符不绘制）", () => {
    const surface = new RecordingSurface();
    const layer = new TileLayer(surface);
    layer.setSize(64, 64);
    surface.ops.length = 0;
    const header: TileHeader = { windowId: 1n, frameSeq: 1, x: 0, y: 0, w: 4, h: 4 };
    expect(layer.handleTile(header, new Uint8ClampedArray(4 * 4 * 4))).toBe(true);
    expect(surface.ops).toEqual(["put 0,0,4,4 @0,0"]);
    expect(layer.handleTile(header, new Uint8ClampedArray(10))).toBe(false);
    expect(surface.ops).toHaveLength(1);
  });

  it("未 setSize 前不绘制", () => {
    const surface = new RecordingSurface();
    const layer = new TileLayer(surface);
    const header: TileHeader = { windowId: 1n, frameSeq: 1, x: 0, y: 0, w: 4, h: 4 };
    expect(layer.handleTile(header, new Uint8ClampedArray(64))).toBe(false);
    expect(layer.handleInvalidate(header)).toBe(false);
    expect(surface.ops).toEqual([]);
  });

  it("handleTile 越界部分裁剪到画布（负坐标与超出右/下边）", () => {
    const surface = new RecordingSurface();
    const layer = new TileLayer(surface);
    layer.setSize(100, 100);
    surface.ops.length = 0;
    expect(
      layer.handleTile(
        { windowId: 1n, frameSeq: 1, x: 96, y: 96, w: 8, h: 8 },
        new Uint8ClampedArray(8 * 8 * 4),
      ),
    ).toBe(true);
    expect(surface.ops).toEqual(["put 96,96,4,4 @96,96"]);
    expect(
      layer.handleTile(
        { windowId: 1n, frameSeq: 2, x: 200, y: 200, w: 8, h: 8 },
        new Uint8ClampedArray(8 * 8 * 4),
      ),
    ).toBe(false);
  });

  it("handleInvalidate 清除瓦片区域", () => {
    const surface = new RecordingSurface();
    const layer = new TileLayer(surface);
    layer.setSize(100, 100);
    surface.ops.length = 0;
    expect(
      layer.handleInvalidate({ windowId: 1n, frameSeq: 3, x: 10, y: 20, w: 64, h: 64 }),
    ).toBe(true);
    expect(surface.ops).toEqual(["clear 10,20,64,64"]);
  });

  it("clear 整层清空", () => {
    const surface = new RecordingSurface();
    const layer = new TileLayer(surface);
    layer.setSize(50, 50);
    layer.clear();
    expect(surface.ops).toEqual(["resize 50x50", "clearAll", "clearAll"]);
  });
});

describe("dispatchFrame", () => {
  function makeSink() {
    return {
      decoder: { push: vi.fn() },
      tile: { handleTile: vi.fn(), handleInvalidate: vi.fn() },
    };
  }

  it("视频帧按 window_id 路由（剥离类型前缀后喂 decoder）", () => {
    const a = makeSink();
    const b = makeSink();
    const surfaces = new Map<string, FrameSink>([
      ["100", a],
      ["200", b],
    ]);
    dispatchFrame(surfaces, videoFrame(100n));
    expect(a.decoder.push).toHaveBeenCalledTimes(1);
    expect(b.decoder.push).not.toHaveBeenCalled();
    // push 的输入自偏移 1 起（25B 头 + AU）
    const pushed = a.decoder.push.mock.calls[0]?.[0] as ArrayBuffer;
    expect(pushed.byteLength).toBe(29);
    expect(parseTileHeader(pushed, 0).windowId).toBe(100n);
  });

  it("瓦片帧路由到 tile.handleTile 并携带 RGBA 视图", () => {
    const sink = makeSink();
    const surfaces = new Map<string, FrameSink>([["7", sink]]);
    dispatchFrame(surfaces, tileFrame(FRAME_TILE, 7n, 8, 16, 4, 4));
    expect(sink.tile.handleTile).toHaveBeenCalledTimes(1);
    const [header, rgba] = sink.tile.handleTile.mock.calls[0] as [
      TileHeader,
      Uint8ClampedArray,
    ];
    expect(header.windowId).toBe(7n);
    expect(header.x).toBe(8);
    expect(header.y).toBe(16);
    expect(rgba.length).toBe(64);
  });

  it("失效帧路由到 tile.handleInvalidate", () => {
    const sink = makeSink();
    const surfaces = new Map<string, FrameSink>([["7", sink]]);
    dispatchFrame(surfaces, tileFrame(FRAME_INVALIDATE, 7n, 1, 2, 32, 32));
    expect(sink.tile.handleInvalidate).toHaveBeenCalledTimes(1);
    expect(sink.tile.handleTile).not.toHaveBeenCalled();
    expect(sink.decoder.push).not.toHaveBeenCalled();
  });

  it("未知窗口与过短帧安全忽略", () => {
    const sink = makeSink();
    const surfaces = new Map<string, FrameSink>([["7", sink]]);
    dispatchFrame(surfaces, videoFrame(999n));
    dispatchFrame(surfaces, tileFrame(FRAME_TILE, 999n, 0, 0, 2, 2));
    dispatchFrame(surfaces, new ArrayBuffer(10)); // 过短
    dispatchFrame(surfaces, tileFrame(FRAME_TILE, 7n, 0, 0, 4, 4).slice(0, 20)); // 负载截断
    expect(sink.decoder.push).not.toHaveBeenCalled();
    expect(sink.tile.handleTile).not.toHaveBeenCalled();
    expect(sink.tile.handleInvalidate).not.toHaveBeenCalled();
  });

  it("跨流防残影：晚于更新视频帧到达的过期瓦片被丢弃", () => {
    const surface = new RecordingSurface();
    const layer = new TileLayer(surface);
    layer.setSize(128, 128);
    surface.ops.length = 0;
    layer.onVideoSeq(105);
    const rgba = new Uint8ClampedArray(4 * 4 * 4);
    // 过期瓦片（frameSeq=100 < latestVideoSeq=105）必须丢弃
    expect(
      layer.handleTile({ windowId: 1n, frameSeq: 100, x: 0, y: 0, w: 4, h: 4 }, rgba),
    ).toBe(false);
    expect(surface.ops).toHaveLength(0);
    // 对齐最新帧（frameSeq=105）的瓦片正常绘制
    expect(
      layer.handleTile({ windowId: 1n, frameSeq: 105, x: 0, y: 0, w: 4, h: 4 }, rgba),
    ).toBe(true);
    expect(surface.ops).toHaveLength(1);
  });
});
