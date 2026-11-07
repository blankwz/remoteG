/**
 * 崩溃报告（T-040）前端封装：本地 dump 列表 / 清空 / 预填上报链接。
 * dump 只含元数据（app/版本/时间/线程/panic 消息/位置），无内存内容；
 * 上报 = 用户点「在浏览器上报」后由 opener 打开预填 Issue，可检查再提交。
 */
import { call } from "./bridge";

/** 与 Rust rd_crash::DumpEntry 对齐。 */
export interface CrashEntry {
  app: string;
  version: string;
  time_unix: number;
  thread: string;
  message: string;
  location: string;
  file: string;
}

/** 列出崩溃记录（时间倒序；浏览器 demo 环境 mockInvoke 兜底返回 undefined）。 */
export function crashDumps(): Promise<CrashEntry[] | undefined> {
  return call<CrashEntry[] | undefined>("crash_dumps");
}

/** 清空崩溃记录，返回删除条数。 */
export function crashClear(): Promise<number> {
  return call<number>("crash_clear");
}

/** 组装「在浏览器上报」的预填 Issue 链接。 */
export function crashReportUrl(): Promise<string> {
  return call<string>("crash_report_url");
}
