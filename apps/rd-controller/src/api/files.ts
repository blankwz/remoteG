/**
 * 文件传输命令封装（T-031）：列目录 / 推送 / 拉取 / 取消。
 *
 * 进度经 Tauri `Channel` 回传；浏览器 demo 模式无 IPC——推送/拉取在 API 层
 * 本地模拟一段进度后返回（mockInvoke 表覆盖列目录演示数据）。
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import { call, isTauriRuntime } from "./bridge";

/** 目录条目（与 Rust FileEntryDto 对齐，camelCase）。 */
export interface FileEntryDto {
  name: string;
  dir: boolean;
  size: number;
}

/** 列目录结果：path 为被控端规范化后的绝对路径（空请求 = 主目录展开），
 * 前端必须用它（而非请求时的原始串）拼接子路径——相对名会被 Agent 拒绝。 */
export interface FileListResult {
  path: string;
  entries: FileEntryDto[];
}

/** 传输进度事件（phase: sending/receiving/done/error）。 */
export interface XferProgress {
  xferId: number;
  name: string;
  bytes: number;
  total: number;
  phase: string;
  message: string | null;
}

/** 列被控端目录：path 为绝对路径，空串 = 被控端主目录。 */
export function fileList(path: string): Promise<FileListResult> {
  return call<FileListResult>("file_list", { path });
}

/** 取消进行中的传输（xferId 来自进度事件）。 */
export function fileCancel(deviceId: string, xferId: number): Promise<void> {
  return call("file_cancel", { deviceId, xferId });
}

/** 推送本地文件到被控端（落 `Downloads\rd接收`）。 */
export function fileSend(
  deviceId: string,
  paths: string[],
  onProgress: (p: XferProgress) => void,
): Promise<void> {
  if (!isTauriRuntime()) {
    return mockXfer(paths.map((p) => p.replace(/.*[\\/]/, "")), "sending", onProgress).then(
      () => undefined,
    );
  }
  const ch = new Channel<XferProgress>();
  ch.onmessage = onProgress;
  return invoke("file_send", { deviceId, paths, onProgress: ch });
}

/** 拉取被控端文件到本机目录（destDir 绝对路径），返回保存的完整路径。 */
export function filePull(
  deviceId: string,
  remotePath: string,
  destDir: string,
  onProgress: (p: XferProgress) => void,
): Promise<string> {
  if (!isTauriRuntime()) {
    const name = remotePath.replace(/.*[\\/]/, "");
    return mockXfer([name], "receiving", onProgress).then(
      () => `${destDir}/${name}`,
    );
  }
  const ch = new Channel<XferProgress>();
  ch.onmessage = onProgress;
  return invoke("file_pull", { deviceId, remotePath, destDir, onProgress: ch });
}

/** demo 模拟：按文件名假传 4 档进度。 */
async function mockXfer(
  names: string[],
  phase: string,
  onProgress: (p: XferProgress) => void,
): Promise<void> {
  names.forEach((name, i) => {
    for (const pct of [0.25, 0.5, 0.75, 1]) {
      onProgress({
        xferId: i + 1,
        name,
        bytes: Math.round(1024 * 1024 * pct),
        total: 1024 * 1024,
        phase: pct === 1 ? "done" : phase,
        message: null,
      });
    }
  });
  await new Promise((r) => setTimeout(r, 80));
}
