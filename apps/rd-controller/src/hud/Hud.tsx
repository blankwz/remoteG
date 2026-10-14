/**
 * 解码 HUD（T-005）：fps / 解码耗时 / 分辨率 / pts 与本地时钟差。
 *
 * ptsΔ 需要两端时钟同步才准确（rd-agent 当前无同步），仅作参考。
 */
import type { DecodeStats } from "../video/decoder";
import styles from "./Hud.module.css";

interface Props {
  stats: DecodeStats;
  /** 最近帧 pts 与本地时钟差（微秒）；尚无帧时为 null。 */
  ptsDriftUs: number | null;
}

export function Hud({ stats, ptsDriftUs }: Props) {
  return (
    <div className={styles.hud}>
      <span className={styles.item}>fps {stats.fps}</span>
      <span className={styles.item}>decode {stats.decodeMs.toFixed(1)} ms</span>
      <span className={styles.item}>
        {stats.width > 0 ? `${stats.width}x${stats.height}` : "-"}
      </span>
      {ptsDriftUs !== null && (
        <span className={styles.item} title="pts 与本地时钟差（未同步，仅参考）">
          ptsΔ {(ptsDriftUs / 1000).toFixed(0)} ms
        </span>
      )}
    </div>
  );
}
