/**
 * 无损瓦片层（T-021）：帧类型 2 绘制（putImageData）、类型 3 失效清除
 * （clearRect）、视频尺寸变化（新 IDR）时整层清空。
 *
 * 跨流时序（T-016 既定语义，不做过期丢弃）：
 * - 视频 seq = 编码帧序，瓦片 frame_seq 对齐产生它的视频帧；
 * - refine 关闭时瓦片流恒无数据。
 * 瓦片晚于视频帧几毫秒属可接受时序（任务卡风险条目），残影风险留联调观察；
 * 如需按 frame_seq 丢弃过期瓦片，在 handleTile 前置校验即可（记入 RUN_LOG）。
 *
 * 绘制逻辑与 Canvas 解耦（TileSurface 接口）：纯逻辑（头解析/负载校验/边界
 * 裁剪/尺寸清层）可在 node 环境单测；CanvasTileSurface 为浏览器实现。
 */

/** 瓦片帧头长度：u64 window_id + u32 frame_seq + u16 x,y,w,h + u8 fmt + u32 len。 */
export const TILE_HEADER_LEN = 25;

/** 前端帧类型前缀：`[1][VideoHeader][AU]`。 */
export const FRAME_VIDEO = 1;
/** 前端帧类型前缀：`[2][TileHeader][RGBA]`（Rust 侧已解码为 RGBA）。 */
export const FRAME_TILE = 2;
/** 前端帧类型前缀：`[3][TileHeader]` 失效清除。 */
export const FRAME_INVALIDATE = 3;

/** 瓦片帧头字段（小端，05_api §3）。u64 字段用 bigint 保留精度。 */
export interface TileHeader {
  windowId: bigint;
  frameSeq: number;
  x: number;
  y: number;
  w: number;
  h: number;
}

/** 解析 25 字节瓦片帧头（自 offset 起，默认 0）。过短抛错。 */
export function parseTileHeader(buf: ArrayBuffer, offset = 0): TileHeader {
  const view = new DataView(buf, offset);
  if (view.byteLength < TILE_HEADER_LEN) {
    throw new Error(
      `tile frame header too short: ${view.byteLength} < ${TILE_HEADER_LEN}`,
    );
  }
  return {
    windowId: view.getBigUint64(0, true),
    frameSeq: view.getUint32(8, true),
    x: view.getUint16(12, true),
    y: view.getUint16(14, true),
    w: view.getUint16(16, true),
    h: view.getUint16(18, true),
  };
}

/** 瓦片 RGBA 负载字节数（w*h*4）。 */
export function tilePayloadLen(t: Pick<TileHeader, "w" | "h">): number {
  return t.w * t.h * 4;
}

/** 矩形（画布像素坐标）。 */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** 把矩形裁剪到 (0,0,boundsW,boundsH)；完全界外或空矩形返回 null。 */
export function clipRect(r: Rect, boundsW: number, boundsH: number): Rect | null {
  const x0 = Math.max(0, r.x);
  const y0 = Math.max(0, r.y);
  const x1 = Math.min(boundsW, r.x + r.w);
  const y1 = Math.min(boundsH, r.y + r.h);
  if (x1 <= x0 || y1 <= y0) {
    return null;
  }
  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}

/**
 * 瓦片绘制面（canvas 或测试替身）。
 * putTile 的 `rgba` 按完整 `tileRect` 布局，`clip` 为其中实际要绘制（已裁剪）
 * 的子区域——负坐标瓦片只需绘制落在画布内的部分。
 */
export interface TileSurface {
  /** rgba 为完整 tileRect 的像素数据（w*h*4），clip 为其中实际绘制的子区域。 */
  putTile(tileRect: Rect, clip: Rect, rgba: Uint8ClampedArray<ArrayBuffer>): void;
  clearRegion(r: Rect): void;
  clearAll(): void;
  /** 画布物理尺寸（与视频对齐；改变即清层）。 */
  resize(w: number, h: number): void;
}

/** 瓦片层：帧类型 2/3 的绘制与清除决策（纯逻辑，可注入测试替身）。 */
export class TileLayer {
  private readonly surface: TileSurface;
  private width = 0;
  private height = 0;

  constructor(surface: TileSurface) {
    this.surface = surface;
  }

  /** 对齐视频物理尺寸；尺寸变化时整层清空（视频重新 IDR 语义）。 */
  setSize(w: number, h: number): void {
    if (w === this.width && h === this.height) {
      return;
    }
    this.width = w;
    this.height = h;
    this.surface.resize(w, h);
    this.surface.clearAll();
  }

  /** 当前对齐的画布尺寸（测试用）。 */
  size(): { width: number; height: number } {
    return { width: this.width, height: this.height };
  }

  /** 帧类型 2：绘制瓦片（负载长度与边界校验，越界部分裁剪）。返回是否实际绘制。 */
  handleTile(header: TileHeader, rgba: Uint8ClampedArray<ArrayBuffer>): boolean {
    if (header.w === 0 || header.h === 0) {
      return false;
    }
    if (rgba.length !== tilePayloadLen(header)) {
      return false;
    }
    if (this.width === 0 || this.height === 0) {
      return false;
    }
    const tileRect: Rect = { x: header.x, y: header.y, w: header.w, h: header.h };
    const clip = clipRect(tileRect, this.width, this.height);
    if (clip === null) {
      return false;
    }
    this.surface.putTile(tileRect, clip, rgba);
    return true;
  }

  /** 帧类型 3：失效清除（clearRect 语义）。返回是否实际清除。 */
  handleInvalidate(header: TileHeader): boolean {
    const clip = clipRect(
      { x: header.x, y: header.y, w: header.w, h: header.h },
      this.width,
      this.height,
    );
    if (clip === null) {
      return false;
    }
    this.surface.clearRegion(clip);
    return true;
  }

  /** 整层清空（重连/模式重置等场景）。 */
  clear(): void {
    this.surface.clearAll();
  }
}

/** 浏览器实现：把瓦片画到 <canvas>（与视频画布 1:1 像素对齐）。 */
export class CanvasTileSurface implements TileSurface {
  private readonly canvas: HTMLCanvasElement;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
  }

  resize(w: number, h: number): void {
    // 赋值 width/height 会重置画布内容（透明），与整层清空语义一致
    this.canvas.width = w;
    this.canvas.height = h;
  }

  putTile(tileRect: Rect, clip: Rect, rgba: Uint8ClampedArray<ArrayBuffer>): void {
    const ctx = this.canvas.getContext("2d");
    if (!ctx) {
      return;
    }
    // putImageData(img, dx, dy, dirtyX, dirtyY, dirtyW, dirtyH)：
    // 图像原点放 tileRect，dirty 子矩形只落已裁剪区域
    ctx.putImageData(
      new ImageData(rgba, tileRect.w, tileRect.h),
      tileRect.x,
      tileRect.y,
      clip.x - tileRect.x,
      clip.y - tileRect.y,
      clip.w,
      clip.h,
    );
  }

  clearRegion(r: Rect): void {
    this.canvas.getContext("2d")?.clearRect(r.x, r.y, r.w, r.h);
  }

  clearAll(): void {
    this.canvas
      .getContext("2d")
      ?.clearRect(0, 0, this.canvas.width, this.canvas.height);
  }
}
