/**
 * 自定义常用语存储（P1-3 同源化）：悬浮工具栏面板与设置页共用一份
 * localStorage `rd-phrases`（上限 20 条），内置 5 条不可删、不落盘。
 */

export const PHRASE_DEFAULTS = ["你好", "请稍等", "我来看一下", "操作完成", "谢谢"];
const PHRASES_KEY = "rd-phrases";
export const PHRASES_MAX = 20;

/** 读自定义常用语；脏数据（非数组/非字符串项）逐项过滤。 */
export function readCustomPhrases(): readonly string[] {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(PHRASES_KEY) ?? "[]");
    return Array.isArray(parsed)
      ? parsed.filter((p): p is string => typeof p === "string")
      : [];
  } catch {
    return [];
  }
}

/** 写自定义常用语（存储不可用仅失去持久化）。 */
export function saveCustomPhrases(phrases: readonly string[]): void {
  try {
    localStorage.setItem(PHRASES_KEY, JSON.stringify(phrases));
  } catch {
    // 存储不可用仅失去持久化
  }
}
