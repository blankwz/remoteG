/**
 * WebCodecs H.264 解码封装（T-005）。
 *
 * - 帧格式：25 字节视频帧头 + H.264 Annex-B AU（05_api/协议契约.md §3，小端）；
 * - configure 不传 description → WebCodecs 按 Annex-B 解析输入；
 * - 首帧必须是 keyframe，之前的 delta 帧丢弃（M0 只记日志，不回发 IDR 请求）；
 * - 输出 VideoFrame 用 drawImage 画到 canvas 后立即 close（释放 GPU 资源）。
 */

/** 视频帧头长度：u64 window_id + u32 seq + u64 pts_us + u8 flags + u32 len。 */
export const VIDEO_HEADER_LEN = 25;

/** 视频帧头字段（小端，05_api §3）。u64 字段用 bigint 保留精度。 */
export interface VideoFrameHeader {
  windowId: bigint;
  seq: number;
  ptsUs: bigint;
  keyframe: boolean;
  payloadLen: number;
}

/** 解析 25 字节视频帧头；过短抛错。 */
export function parseVideoHeader(buf: ArrayBuffer): VideoFrameHeader {
  const view = new DataView(buf);
  if (view.byteLength < VIDEO_HEADER_LEN) {
    throw new Error(
      `video frame header too short: ${view.byteLength} < ${VIDEO_HEADER_LEN}`,
    );
  }
  return {
    windowId: view.getBigUint64(0, true),
    seq: view.getUint32(8, true),
    ptsUs: view.getBigUint64(12, true),
    keyframe: (view.getUint8(20) & 0x01) !== 0,
    payloadLen: view.getUint32(21, true),
  };
}

/** 解码统计（HUD 展示）。 */
export interface DecodeStats {
  fps: number;
  decodeMs: number;
  width: number;
  height: number;
}

/** fps 滑动窗口长度。 */
const FPS_WINDOW_MS = 1000;
/** stats 回调节流周期（避免每帧触发 React 重渲染）。 */
const STATS_INTERVAL_MS = 250;

export class H264Decoder {
  private readonly decoder: VideoDecoder;
  private readonly canvas: HTMLCanvasElement;
  private readonly onStats: (stats: DecodeStats) => void;

  /** 等待首 keyframe 标志。 */
  private seenKeyframe = false;
  /** 丢弃的 keyframe 前帧数（日志用）。 */
  private droppedBeforeKey = 0;
  /** 最近一次 push 的时间戳队列，output 时对齐求解码耗时。 */
  private readonly decodeStarts: number[] = [];
  /** fps 滑动窗口内的 output 时间戳。 */
  private outputTimes: number[] = [];
  /** 最近一次解码耗时（ms）。 */
  private lastDecodeMs = 0;
  /** 最近一帧头部（HUD 估算 pts 差用）。 */
  private latestHeader: VideoFrameHeader | null = null;
  private lastStatsEmit = 0;
  private closed = false;

  constructor(canvas: HTMLCanvasElement, onStats: (stats: DecodeStats) => void) {
    this.canvas = canvas;
    this.onStats = onStats;
    this.decoder = new VideoDecoder({
      output: (frame) => this.onOutput(frame),
      error: (err) => {
        // 解码器致命错误：M0 仅记录与上报，不自动重建
        console.error("VideoDecoder error", err);
        this.emitStats(true);
      },
    });
    this.decoder.configure({
      // baseline profile level 5.1（1080p60）；无 description → Annex-B
      codec: "avc1.42E033",
      optimizeForLatency: true,
      hardwareAcceleration: "prefer-hardware",
    });
  }

  /** 推入一帧完整数据（25B 帧头 + AU）。 */
  push(buf: ArrayBuffer): void {
    if (this.closed) return;
    let header: VideoFrameHeader;
    try {
      header = parseVideoHeader(buf);
    } catch (err) {
      console.error("drop malformed video frame", err);
      return;
    }
    const auLen = buf.byteLength - VIDEO_HEADER_LEN;
    if (auLen < header.payloadLen) {
      console.error(
        `drop truncated frame seq=${header.seq}: have ${auLen}B, want ${header.payloadLen}B`,
      );
      return;
    }
    if (!this.seenKeyframe) {
      if (!header.keyframe) {
        this.droppedBeforeKey += 1;
        console.warn(
          `waiting for keyframe, dropped seq=${header.seq} (total ${this.droppedBeforeKey})`,
        );
        return;
      }
      this.seenKeyframe = true;
      console.debug(
        `first keyframe received (windowId=${header.windowId}), decoder starts`,
      );
    }
    this.latestHeader = header;
    // slice 拷贝出独立 ArrayBuffer，避免与 Channel 复用缓冲冲突
    const au = buf.slice(VIDEO_HEADER_LEN);
    this.decodeStarts.push(performance.now());
    this.decoder.decode(
      new EncodedVideoChunk({
        type: header.keyframe ? "key" : "delta",
        timestamp: Number(header.ptsUs),
        data: new Uint8Array(au),
      }),
    );
  }

  /** 最近一帧头部；尚无帧时为 null。 */
  lastHeader(): VideoFrameHeader | null {
    return this.latestHeader;
  }

  /** 关闭解码器；重复调用安全。 */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    try {
      this.decoder.close();
    } catch {
      // 已关闭 / 未初始化，忽略
    }
  }

  private onOutput(frame: VideoFrame): void {
    const start = this.decodeStarts.shift();
    this.lastDecodeMs = start !== undefined ? performance.now() - start : 0;

    const { canvas } = this;
    if (
      canvas.width !== frame.displayWidth ||
      canvas.height !== frame.displayHeight
    ) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
    }
    const ctx = canvas.getContext("2d");
    if (ctx) {
      ctx.drawImage(frame, 0, 0);
    }
    frame.close();

    const now = performance.now();
    this.outputTimes.push(now);
    this.outputTimes = this.outputTimes.filter(
      (t) => now - t <= FPS_WINDOW_MS,
    );
    this.emitStats(false);
  }

  private emitStats(force: boolean): void {
    const now = performance.now();
    if (!force && now - this.lastStatsEmit < STATS_INTERVAL_MS) return;
    this.lastStatsEmit = now;
    this.onStats({
      fps: this.outputTimes.length,
      decodeMs: this.lastDecodeMs,
      width: this.canvas.width,
      height: this.canvas.height,
    });
  }
}
