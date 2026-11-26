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

/** H.264 Baseline 4:2:0，level 5.1（1080p60；无 description → Annex-B）。 */
export const H264_CODEC_420 = "avc1.42E033";
/** H.264 High 4:4:4 Predictive，level 5.1（与 probeDecodeCaps 的 444 探测一致）。 */
export const H264_CODEC_444 = "avc1.F40033";
/** HEVC (H.265) Main Profile Level 5.1（4K@144Hz 硬件解码）。 */
export const HEVC_CODEC = "hvc1.1.6.L153.B0";
/** AV1 Main Profile Level 5.0 8-bit（4K@144Hz 硬件解码）。 */
export const AV1_CODEC = "av01.0.12M.08";

/**
 * OpenWindowAck 的 codec 枚举名 → WebCodecs codec string。
 *
 * 支持 `AV1` / `HEVC_444` / `H264_444` / `H264_420`；未知值或空值一律回退 420 Baseline。
 */
export function codecStringFor(codec: string | null | undefined): string {
  switch (codec) {
    case "AV1":
      return AV1_CODEC;
    case "HEVC_444":
      return HEVC_CODEC;
    case "H264_444":
      return H264_CODEC_444;
    default:
      return H264_CODEC_420;
  }
}

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
  /** 解码帧携带的色彩空间摘要（如 `BT.709 Full` / `BT.709`），用于 HUD 排障与核验。 */
  colorSpace?: string;
}

/** 格式化 VideoFrame.colorSpace 为简洁人类可读标签。 */
export function formatColorSpace(cs: VideoColorSpace | null | undefined): string | undefined {
  if (!cs) return undefined;
  const rawMatrix: string | null | undefined = cs.matrix;
  const matrix =
    rawMatrix === "bt709"
      ? "BT.709"
      : rawMatrix === "smpte170m" || rawMatrix === "bt470bg"
        ? "BT.601"
        : rawMatrix === "bt2020-ncl"
          ? "BT.2020"
          : rawMatrix === "rgb"
            ? "RGB"
            : cs.primaries === "bt709"
              ? "BT.709"
              : undefined;
  if (!matrix) return undefined;
  const range = cs.fullRange === true ? " Full" : cs.fullRange === false ? " Limited" : "";
  return `${matrix}${range}`;
}

/** fps 滑动窗口长度。 */
const FPS_WINDOW_MS = 1000;
/** stats 回调节流周期（避免每帧触发 React 重渲染）。 */
const STATS_INTERVAL_MS = 250;
/** 解码器错误自愈重建上限（防止无限死循环）。 */
const MAX_RECOVER_ATTEMPTS = 4;

export class H264Decoder {
  private decoder: VideoDecoder;
  private readonly canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D | null = null;
  private readonly onStats: (stats: DecodeStats) => void;
  private readonly codec: string;
  private hwAccel: HardwareAcceleration = "prefer-hardware";
  private recoverCount = 0;

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
  /** 最近一帧的色彩空间标签。 */
  private lastColorSpace: string | undefined = undefined;
  /** 最近一帧头部（HUD 估算 pts 差用）。 */
  private latestHeader: VideoFrameHeader | null = null;
  private lastStatsEmit = 0;
  private closed = false;
  /** 高频 144Hz 渲染合批槽位：同一垂直同步周期内若吐出多帧，仅绘制最新帧并立即释放旧帧。 */
  private pendingFrame: VideoFrame | null = null;
  private rafId: number | null = null;

  /**
   * @param codec WebCodecs codec string；默认 420 Baseline。444 协商命中时
   *   由调用方传 `codecStringFor(ack.codec)` 的结果（High 4:4:4），否则解码器
   *   以错误 profile 建立会导致 444 码流解码失败（黑屏）。
   */
  constructor(
    canvas: HTMLCanvasElement,
    onStats: (stats: DecodeStats) => void,
    codec: string = H264_CODEC_420,
  ) {
    this.canvas = canvas;
    this.onStats = onStats;
    this.codec = codec;
    this.decoder = this.createRawDecoder();
    this.configureDecoder();
  }

  private createRawDecoder(): VideoDecoder {
    return new VideoDecoder({
      output: (frame) => {
        this.onOutput(frame);
      },
      error: (err) => {
        // 解码器致命错误：记录并尝试自愈重建（硬件失败自动降级到 no-preference），
        // 避免 GPU 驱动偶发异常后永久黑屏；不 emitStats 防止上报 300x150 假尺寸。
        console.error("VideoDecoder error", err);
        this.recoverFromError();
      },
    });
  }

  private decoderConfig(): VideoDecoderConfig {
    return {
      // profile/level 由协商结果决定；无 description → Annex-B
      codec: this.codec,
      optimizeForLatency: true,
      hardwareAcceleration: this.hwAccel,
      // 码流未携带 VUI 时按 BT.709 Full 还原；码流带 VUI 时浏览器优先尊从码流 VUI
      colorSpace: {
        primaries: "bt709",
        transfer: "bt709",
        matrix: "bt709",
        fullRange: true,
      },
    };
  }

  private configureDecoder(): void {
    try {
      this.decoder.configure(this.decoderConfig());
    } catch (err) {
      // 硬件加速配置失败时自动降级到 no-preference 重试一次（兼容不支持 4:4:4 硬解的显卡）
      if (this.hwAccel === "prefer-hardware") {
        this.hwAccel = "no-preference";
        try {
          this.decoder.configure(this.decoderConfig());
          console.warn("VideoDecoder prefer-hardware 失败，已自动降级为 no-preference");
          return;
        } catch (fallbackErr) {
          console.error("VideoDecoder configure failed (fallback)", fallbackErr);
          this.closed = true;
          return;
        }
      }
      // configure 失败（WebCodecs 不可用/老 GPU/无硬解）：标记关闭并静默——
      // 抛出去会崩掉 WindowSurface 的挂载 effect，引发整页布局抖动（R5 实测）
      console.error("VideoDecoder configure failed", err);
      this.closed = true;
    }
  }

  /** 解码器运行时报错自愈：重建实例、降级硬件加速、重置等待下一个关键帧。 */
  private recoverFromError(): void {
    if (this.closed || this.recoverCount >= MAX_RECOVER_ATTEMPTS) {
      return;
    }
    this.recoverCount += 1;
    try {
      this.decoder.close();
    } catch {
      // 忽略已关闭状态
    }
    this.hwAccel = "no-preference";
    this.seenKeyframe = false;
    this.droppedBeforeKey = 0;
    this.decodeStarts.length = 0;
    try {
      this.decoder = this.createRawDecoder();
      this.configureDecoder();
      console.warn(`VideoDecoder 已自动重建恢复（第 ${this.recoverCount} 次）`);
    } catch (e) {
      console.error("VideoDecoder recover failed", e);
      this.closed = true;
    }
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
    // 低延迟防积压：若解码队列已堆积且恰逢新关键帧到达，立即清空旧队列直解最新关键帧
    if (header.keyframe && this.decoder.decodeQueueSize > 3) {
      try {
        this.decoder.reset();
        this.decoder.configure(this.decoderConfig());
        this.decodeStarts.length = 0;
      } catch {
        // reset/configure 不支持时忽略，照常送入
      }
    }
    this.latestHeader = header;
    // slice 拷贝出独立 ArrayBuffer，避免与 Channel 复用缓冲冲突
    const au = buf.slice(VIDEO_HEADER_LEN);
    this.decodeStarts.push(performance.now());
    try {
      this.decoder.decode(
        new EncodedVideoChunk({
          type: header.keyframe ? "key" : "delta",
          timestamp: Number(header.ptsUs),
          data: new Uint8Array(au),
        }),
      );
    } catch (err) {
      console.error("VideoDecoder.decode threw", err);
      this.recoverFromError();
    }
  }

  /** 最近一帧头部；尚无帧时为 null。 */
  lastHeader(): VideoFrameHeader | null {
    return this.latestHeader;
  }

  /**
   * 重置关键帧等待状态（后台标签降流恢复用，P0-3）：跳帧期间参考链已断，
   * 之后到达的 delta 直到下一个 keyframe 都不可解——复用首帧等待逻辑，
   * 丢弃 delta 并在 keyframe 到达时续播。尚未见到关键帧时调用无副作用。
   */
  resetForKeyframe(): void {
    if (!this.seenKeyframe) {
      return;
    }
    this.seenKeyframe = false;
    this.droppedBeforeKey = 0;
  }

  /** 关闭解码器；重复调用安全。 */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.rafId !== null && typeof cancelAnimationFrame === "function") {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
    if (this.pendingFrame !== null) {
      this.pendingFrame.close();
      this.pendingFrame = null;
    }
    try {
      this.decoder.close();
    } catch {
      // 已关闭 / 未初始化，忽略
    }
  }

  private onOutput(frame: VideoFrame): void {
    if (this.closed) {
      frame.close();
      return;
    }
    const start = this.decodeStarts.shift();
    this.lastDecodeMs = start !== undefined ? performance.now() - start : 0;
    const cs = formatColorSpace(frame.colorSpace);
    if (cs !== undefined) {
      this.lastColorSpace = cs;
    }

    const now = performance.now();
    this.outputTimes.push(now);
    this.outputTimes = this.outputTimes.filter(
      (t) => now - t <= FPS_WINDOW_MS,
    );

    // 浏览器环境按显示刷新率合批绘制（144Hz 防 burst 主线程拥堵，中间过时帧直接释放 GPU 句柄）
    if (typeof requestAnimationFrame === "function") {
      if (this.pendingFrame !== null) {
        this.pendingFrame.close();
      }
      this.pendingFrame = frame;
      if (this.rafId === null) {
        this.rafId = requestAnimationFrame(() => {
          this.rafId = null;
          const next = this.pendingFrame;
          this.pendingFrame = null;
          if (next !== null) {
            this.paintFrame(next);
          }
        });
      }
    } else {
      this.paintFrame(frame);
    }
  }

  private paintFrame(frame: VideoFrame): void {
    try {
      const { canvas } = this;
      if (
        canvas.width !== frame.displayWidth ||
        canvas.height !== frame.displayHeight
      ) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
        // 尺寸变化后重新获取/确保低延迟不透明 2D 上下文
        this.ctx = null;
      }
      if (this.ctx === null && typeof canvas.getContext === "function") {
        this.ctx =
          canvas.getContext("2d", {
            alpha: false,
            desynchronized: true,
          }) ?? canvas.getContext("2d");
      }
      if (this.ctx) {
        this.ctx.drawImage(frame, 0, 0);
      }
    } finally {
      frame.close();
    }
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
      colorSpace: this.lastColorSpace,
    });
  }
}
