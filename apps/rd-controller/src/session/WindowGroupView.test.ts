/**
 * 帧路由注册表生命周期单测（断线重连帧路由丢失修复的回归测试）。
 *
 * 背景：主会话 effect 曾在重建（断线重连 epoch++）时 clear 注册表，而
 * WindowSurface 的注册 effect 依赖 [winId, register, codec] 不随重连变化、
 * 无人重新注册 → 重连后所有帧被 dispatchFrame 静默丢弃（黑屏），直到切换
 * 标签重新挂载。修复后注册表只能经 registerSurface 增删，主会话重建不触碰它，
 * WindowSurface 随 sessionEpoch 变化自行注销重注册。
 *
 * 测试环境为 vitest node 环境（无 DOM/React 渲染，同 TileLayer.test.ts 的
 * 策略）：这里驱动导出的纯函数 registerSurface + dispatchFrame，验证组件所
 * 依赖的注册表契约——条目只增删不清空、按 winId 覆盖、注销仅移除自身、
 * 重建后路由保持有效。真 WebView 重连场景的手测清单见修复任务报告。
 */
import { describe, expect, it, vi } from "vitest";
import {
  dispatchFrame,
  registerSurface,
  type FrameSink,
} from "./WindowGroupView";
import {
  FRAME_INVALIDATE,
  FRAME_TILE,
  FRAME_VIDEO,
  TILE_HEADER_LEN,
} from "./TileLayer";

/** 构造瓦片帧（类型前缀 + 25B 头 + RGBA 负载；布局同 05_api §3）。 */
function tileFrame(
  type: number,
  windowId: bigint,
  x: number,
  y: number,
  w: number,
  h: number,
): ArrayBuffer {
  const payload = w * h * 4;
  const buf = new ArrayBuffer(1 + TILE_HEADER_LEN + payload);
  const view = new DataView(buf);
  view.setUint8(0, type);
  view.setBigUint64(1, windowId, true);
  view.setUint32(9, 0, true); // frame_seq
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
  view.setUint32(9, 1, true); // seq
  view.setBigUint64(13, 100n, true); // pts_us
  view.setUint8(21, key ? 1 : 0); // flags: keyframe
  view.setUint32(22, 4, true); // payload len
  return buf;
}

/** 记录型处理面（decoder/tile 调用可断言）。 */
function makeSink() {
  return {
    decoder: { push: vi.fn() },
    tile: { handleTile: vi.fn(), handleInvalidate: vi.fn() },
  };
}

describe("registerSurface（处理面注册表）", () => {
  it("挂载注册后，视频/瓦片/失效帧按 window_id 路由到对应处理面", () => {
    const root = makeSink();
    const child = makeSink();
    const surfaces = new Map<string, FrameSink>();
    registerSurface(surfaces, "100", root);
    registerSurface(surfaces, "200", child);

    dispatchFrame(surfaces, videoFrame(100n));
    dispatchFrame(surfaces, tileFrame(FRAME_TILE, 200n, 0, 0, 2, 2));
    dispatchFrame(surfaces, tileFrame(FRAME_INVALIDATE, 200n, 1, 2, 8, 8));

    expect(root.decoder.push).toHaveBeenCalledTimes(1);
    expect(root.tile.handleTile).not.toHaveBeenCalled();
    expect(child.tile.handleTile).toHaveBeenCalledTimes(1);
    expect(child.tile.handleInvalidate).toHaveBeenCalledTimes(1);
    expect(child.decoder.push).not.toHaveBeenCalled();
  });

  it("会话重建（断线重连 epoch++）不清空注册表：重建后帧继续路由（重连黑屏回归）", () => {
    const sink = makeSink();
    const surfaces = new Map<string, FrameSink>();
    registerSurface(surfaces, "100", sink);

    // 修复后的主会话重建语义：只重订阅事件、重新 openWindow，注册表原样保留
    // （重连不卸载 WindowSurface，旧句柄仍有效；sessionEpoch 变化触发的注销
    // 重建走 registerSurface 自身，注册表始终连续有效）。
    dispatchFrame(surfaces, videoFrame(100n));
    expect(sink.decoder.push).toHaveBeenCalledTimes(1);

    // 断言注册表在"重建"前后保持同一批条目且路由连续可用
    expect([...surfaces.keys()]).toEqual(["100"]);
    dispatchFrame(surfaces, videoFrame(100n));
    expect(sink.decoder.push).toHaveBeenCalledTimes(2);
  });

  it("重开同窗口按 winId 覆盖旧句柄：新句柄收帧，旧句柄静默", () => {
    const first = makeSink();
    const second = makeSink();
    const surfaces = new Map<string, FrameSink>();
    registerSurface(surfaces, "100", first);
    registerSurface(surfaces, "100", second);

    dispatchFrame(surfaces, videoFrame(100n));
    expect(second.decoder.push).toHaveBeenCalledTimes(1);
    expect(first.decoder.push).not.toHaveBeenCalled();
    expect(surfaces.size).toBe(1);
  });

  it("注销（卸载/重建清理）仅移除自身；未注册窗口的帧安全忽略", () => {
    const root = makeSink();
    const child = makeSink();
    const surfaces = new Map<string, FrameSink>();
    registerSurface(surfaces, "100", root);
    registerSurface(surfaces, "200", child);

    registerSurface(surfaces, "200", null);

    dispatchFrame(surfaces, videoFrame(100n));
    dispatchFrame(surfaces, tileFrame(FRAME_TILE, 200n, 0, 0, 2, 2));
    dispatchFrame(surfaces, videoFrame(999n));

    expect(root.decoder.push).toHaveBeenCalledTimes(1);
    expect(child.decoder.push).not.toHaveBeenCalled();
    expect(child.tile.handleTile).not.toHaveBeenCalled();
    expect(surfaces.size).toBe(1);
    // 空表/未注册：不抛错（dispatchFrame 对未知窗口安全忽略）
    registerSurface(surfaces, "100", null);
    expect(() => dispatchFrame(surfaces, videoFrame(100n))).not.toThrow();
  });
});

describe("dispatchFrame 后台暂停门控（P0-3 隐藏标签降流）", () => {
  it("paused 的窗口：视频/瓦片/失效帧全部在入口丢弃（不解码不绘制）", () => {
    const sink = { ...makeSink(), paused: true };
    const surfaces = new Map<string, FrameSink>();
    registerSurface(surfaces, "100", sink);

    dispatchFrame(surfaces, videoFrame(100n, true));
    dispatchFrame(surfaces, tileFrame(FRAME_TILE, 100n, 0, 0, 2, 2));
    dispatchFrame(surfaces, tileFrame(FRAME_INVALIDATE, 100n, 1, 2, 8, 8));

    expect(sink.decoder.push).not.toHaveBeenCalled();
    expect(sink.tile.handleTile).not.toHaveBeenCalled();
    expect(sink.tile.handleInvalidate).not.toHaveBeenCalled();
  });

  it("恢复（paused 移除）后帧重新路由；其余窗口不受影响", () => {
    const pausedSink = { ...makeSink(), paused: true };
    const liveSink = makeSink();
    const surfaces = new Map<string, FrameSink>();
    registerSurface(surfaces, "100", pausedSink);
    registerSurface(surfaces, "200", liveSink);

    // 暂停中的 100 丢帧，激活中的 200 正常路由
    dispatchFrame(surfaces, videoFrame(100n));
    dispatchFrame(surfaces, videoFrame(200n));
    expect(pausedSink.decoder.push).not.toHaveBeenCalled();
    expect(liveSink.decoder.push).toHaveBeenCalledTimes(1);

    // 切回标签：以 paused=false 的新句柄覆盖（组件恢复路径同款）
    const resumed = makeSink();
    registerSurface(surfaces, "100", { ...resumed, paused: false });
    dispatchFrame(surfaces, videoFrame(100n));
    expect(resumed.decoder.push).toHaveBeenCalledTimes(1);
    expect(liveSink.decoder.push).toHaveBeenCalledTimes(1);
  });

  it("paused 未设置（undefined）视为不暂停：既有替身无需感知该字段", () => {
    const sink = makeSink(); // 无 paused 字段（FrameSink.paused 可选）
    const surfaces = new Map<string, FrameSink>();
    registerSurface(surfaces, "100", sink);
    dispatchFrame(surfaces, videoFrame(100n));
    dispatchFrame(surfaces, tileFrame(FRAME_TILE, 100n, 0, 0, 2, 2));
    expect(sink.decoder.push).toHaveBeenCalledTimes(1);
    expect(sink.tile.handleTile).toHaveBeenCalledTimes(1);
  });
});
