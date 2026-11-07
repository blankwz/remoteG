/**
 * 应用更新（T-038）：tauri-plugin-updater 封装。
 *
 * - 端点与验签公钥在 src-tauri/tauri.conf.json `plugins.updater`（发布签名
 *   由 CI 完成，见任务卡 T-039；私钥在密码管理器，绝不进仓库）。
 * - 纯浏览器（demo 预览）环境一切 no-op：检查返回 null、不写节流戳。
 * - 静默检查节流 24h：`shouldCheckNow` 纯函数单测锁定；手动「检查更新」
 *   不受节流限制。
 */
import { call, isTauriRuntime } from "./bridge";

/** 发现的可用更新（信息视图；下载句柄由封装内部持有）。 */
export interface UpdateInfo {
  version: string;
  /** 更新说明（Release notes；可为空）。 */
  notes: string | null;
}

/** 下载进度（total 来自 Started 事件；Finished 时 received=total）。 */
export interface DownloadProgress {
  received: number;
  total: number | null;
}

/** 一个待安装的更新：checkForUpdate 的返回值。 */
export interface PendingUpdate {
  info: UpdateInfo;
  /** 下载并安装（验签由插件强制；passive 模式装完不重启）。 */
  install(onProgress?: (p: DownloadProgress) => void): Promise<void>;
}

/** 静默检查间隔：24h。 */
export const UPDATE_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

const LAST_CHECK_KEY = "rd.update.lastCheck";

/** 纯函数：距上次检查是否过了节流窗口（从未检查过 = 该检查）。 */
export function shouldCheckNow(
  lastCheckMs: number | null,
  nowMs: number,
): boolean {
  return (
    lastCheckMs === null || nowMs - lastCheckMs >= UPDATE_CHECK_INTERVAL_MS
  );
}

/** 上次静默检查时间戳（localStorage；损坏/缺失一律视为从未检查）。 */
export function readLastCheck(): number | null {
  try {
    const raw = window.localStorage.getItem(LAST_CHECK_KEY);
    if (raw === null) {
      return null;
    }
    const n = Number(raw);
    return Number.isFinite(n) && n > 0 ? n : null;
  } catch {
    return null; // 存储不可用仅失去节流记忆
  }
}

function markChecked(nowMs: number = Date.now()): void {
  try {
    window.localStorage.setItem(LAST_CHECK_KEY, String(nowMs));
  } catch {
    // 存储不可用仅失去节流记忆
  }
}

/**
 * 检查更新。返回 null = 无更新（或非 Tauri 环境）。
 * 手动与静默共用；两者都刷新节流戳（刚查过就无需再查）。
 * 网络/端点错误原样抛出——静默调用方自行吞错，手动调用方展示。
 */
export async function checkForUpdate(): Promise<PendingUpdate | null> {
  if (!isTauriRuntime()) {
    return null;
  }
  const mod = await import("@tauri-apps/plugin-updater");
  // 先查后记：失败（网络断/端点 404）不写节流戳，24h 短路会漏掉恢复窗口
  // （code-review H-2）
  const update = await mod.check();
  markChecked();
  if (update === null) {
    return null;
  }
  const info: UpdateInfo = {
    version: update.version,
    notes: update.body ?? null,
  };
  return {
    info,
    install: async (onProgress?: (p: DownloadProgress) => void) => {
      let received = 0;
      let total: number | null = null;
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") {
          total = event.data.contentLength ?? null;
          received = 0;
        } else if (event.event === "Progress") {
          received += event.data.chunkLength;
        } else {
          // Finished
          received = total ?? received;
        }
        onProgress?.({ received, total });
      });
    },
  };
}

/** 重启应用以完成更新（passive 安装模式装完不自动退出）。 */
export function restartApp(): Promise<void> {
  return call<void>("restart_app");
}
