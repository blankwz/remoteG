/**
 * 视频画布（T-005）：
 * - canvas 像素尺寸 = 视频尺寸（decoder 内部设置）；
 * - CSS 尺寸 = 像素 / devicePixelRatio，保证物理像素 1:1（无插值模糊）；
 * - 缩放比为整数时启用 pixelated，非整数缩放关闭避免采样伪影。
 */
import {
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type RefObject,
} from "react";
import { H264Decoder, type DecodeStats } from "./decoder";
import styles from "./VideoCanvas.module.css";

interface Props {
  /** 父组件持有 decoder 以推送帧（Channel onmessage → push）。 */
  decoderRef: RefObject<H264Decoder | null>;
  /** 解码统计回调（HUD 用）。 */
  onStats: (stats: DecodeStats) => void;
}

export function VideoCanvas({ decoderRef, onStats }: Props) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  // 用 ref 保存最新回调，避免 effect 因回调身份变化反复重建 decoder
  const onStatsRef = useRef(onStats);
  useEffect(() => {
    onStatsRef.current = onStats;
  }, [onStats]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    const decoder = new H264Decoder(canvas, (stats) => {
      setSize({ width: stats.width, height: stats.height });
      onStatsRef.current(stats);
    });
    decoderRef.current = decoder;
    return () => {
      decoder.close();
      decoderRef.current = null;
    };
  }, [decoderRef]);

  // 注：dpr 运行中变化（拖动跨屏）不触发重渲染，M0 可接受；T-021 视口上报时统一处理
  const dpr = window.devicePixelRatio || 1;
  const style: CSSProperties | undefined =
    size.width > 0
      ? {
          width: `${size.width / dpr}px`,
          height: `${size.height / dpr}px`,
          imageRendering: Number.isInteger(dpr) ? "pixelated" : "auto",
        }
      : undefined;

  return <canvas ref={canvasRef} className={styles.canvas} style={style} />;
}
