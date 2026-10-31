/**
 * axe 扫描出口（R13 基建）：唯一入口，禁散抄配置。
 * jsdom 下部分规则（如 aria-hidden-focus 的可聚焦性评估）判 incomplete
 * 而非 violation——本出口按保守语义把 incomplete 并入 violations（门禁
 * 宁可错杀不可漏放，防橡皮图章）。
 */
import axe from "axe-core";

interface AxeNode {
  readonly target: readonly string[];
}

export interface AxeViolation {
  readonly id: string;
  readonly nodes: readonly AxeNode[];
}

export interface AxeResult {
  readonly violations: readonly AxeViolation[];
}

/** axe 原始返回（violations + incomplete 分列）。 */
interface AxeRaw {
  readonly violations: AxeViolation[];
  readonly incomplete: AxeViolation[];
}

/** 对容器跑 axe（预热 async 完成后扫描）。 */
export async function axeScan(container: HTMLElement): Promise<AxeResult> {
  // 等待一帧：RTL 渲染的 effect/poll 先落地
  await new Promise((r) => setTimeout(r, 30));
  const raw = (await new Promise<AxeRaw>((resolve, reject) => {
    // 默认 runOnly 不含 best-practice（aria-hidden-focus 属该标签），显式带上；
    // resultTypes 必须含 incomplete，否则该分栏不返回
    axe.run(
      container,
      {
        resultTypes: ["violations", "incomplete"],
        runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "best-practice"] },
      },
      (err?: Error | null, res?: unknown) => {
        if (err) {
          reject(err instanceof Error ? err : new Error(String(err)));
        } else {
          resolve(res as AxeRaw);
        }
      },
    );
  }));
  return { violations: [...raw.violations, ...raw.incomplete] };
}
