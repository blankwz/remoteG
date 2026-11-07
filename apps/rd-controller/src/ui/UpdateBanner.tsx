/**
 * 更新横幅（T-038）：发现新版本时在导航栏下方常驻一条，下载/安装/重启
 * 全程内联完成，不打断当前页面。设置页有同款完整入口（手动检查）。
 */
import { useUpdater } from "./useUpdater";
import styles from "../App.module.css";

export function UpdateBanner() {
  const { state, checkNow, install, restart } = useUpdater();
  void checkNow; // 静默检查由 hook 挂载时自触发；此处仅消费状态

  if (state.phase === "idle" || state.phase === "checking") {
    return null;
  }
  if (state.phase === "available") {
    return (
      <div className={styles.updateBanner} role="status">
        <span>
          新版本 <b>v{state.pending.info.version}</b> 可用
          {state.pending.info.notes !== null &&
            ` · ${state.pending.info.notes}`}
        </span>
        <button
          type="button"
          className={styles.updateBtn}
          onClick={() => void install()}
        >
          下载并安装
        </button>
      </div>
    );
  }
  if (state.phase === "downloading") {
    return (
      <div className={styles.updateBanner} role="status">
        <span>
          正在下载更新{state.pct !== null ? `（${state.pct}%）` : "…"}
        </span>
      </div>
    );
  }
  if (state.phase === "ready") {
    return (
      <div className={styles.updateBanner} role="status">
        <span>更新已安装，重启后生效</span>
        <button type="button" className={styles.updateBtn} onClick={restart}>
          重启应用
        </button>
      </div>
    );
  }
  // error：静默路径失败不弹横幅打扰（设置页手动检查才展示错误），静默降级
  return null;
}
