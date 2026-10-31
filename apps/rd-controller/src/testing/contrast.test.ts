/**
 * 对比度自动防线（R14 无障碍终审配额）：解析 App.module.css 颜色令牌，
 * 对全部 文字×表面 令牌对算 WCAG 相对亮度对比度——文字级令牌一律 ≥4.5:1。
 * 纯数学零依赖 axe/jsdom；改暗色令牌若跌破门限，测试当场红。
 */
import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

function parseTokens(css: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of css.matchAll(/(--rd-[a-z-]+):\s*(#[0-9a-fA-F]{6})/g)) {
    out[m[1] ?? ""] = m[2] ?? "";
  }
  return out;
}

function luminance(hex: string): number {
  const n = Number.parseInt(hex.slice(1), 16);
  const ch = [(n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff].map((v) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * (ch[0] ?? 0) + 0.7152 * (ch[1] ?? 0) + 0.0722 * (ch[2] ?? 0);
}

function contrast(a: string, b: string): number {
  const la = luminance(a);
  const lb = luminance(b);
  const [hi, lo] = la >= lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

describe("对比度门禁（令牌对 WCAG）", () => {
  const css = readFileSync(
    fileURLToPath(new URL("../App.module.css", import.meta.url)),
    "utf8",
  );
  const tokens = parseTokens(css);

  it("文字级令牌 × 表面令牌 全对 ≥ 4.5:1", () => {
    const texts = ["--rd-text", "--rd-text-dim", "--rd-text-faint"];
    const surfaces = ["--rd-bg", "--rd-bg-deep", "--rd-panel", "--rd-card", "--rd-card-hover"];
    const bad: string[] = [];
    for (const t of texts) {
      for (const s of surfaces) {
        const ta = tokens[t];
        const sa = tokens[s];
        if (ta === undefined || sa === undefined) {
          bad.push(`令牌缺失: ${t}/${s}`);
          continue;
        }
        const ratio = contrast(ta, sa);
        if (ratio < 4.5) {
          bad.push(`${t} on ${s} = ${ratio.toFixed(2)}:1 (<4.5)`);
        }
      }
    }
    expect(bad, `对比度不足：${bad.join("；")}`).toEqual([]);
  });

  it("白字 × 彩底按钮对 ≥ 4.5:1（R14 a11y 认证残项：曾 3.73/3.03/2.93 逃逸门禁）", () => {
    const whiteOn: ReadonlyArray<readonly [string, string]> = [
      ["--rd-accent", "添加设备/连接 CTA"],
      ["--rd-accent-strong", "CTA hover"],
      ["--rd-err-solid", "删除确认实心钮"],
    ];
    const bad: string[] = [];
    for (const [token, usage] of whiteOn) {
      const bg = tokens[token];
      if (bg === undefined) {
        bad.push(`令牌缺失: ${token}`);
        continue;
      }
      const ratio = contrast("#ffffff", bg);
      if (ratio < 4.5) {
        bad.push(`#fff on ${token}（${usage}）= ${ratio.toFixed(2)}:1`);
      }
    }
    // 渐变单独算两端 stop（parseTokens 只抓 --rd-*: #hex 单值，渐变人工锚定）
    for (const stop of ["#3269e6", "#2b5fd0"]) {
      if (contrast("#ffffff", stop) < 4.5) {
        bad.push(`#fff on accent-grad stop ${stop} = ${contrast("#ffffff", stop).toFixed(2)}:1`);
      }
    }
    expect(bad, `白字彩底违规：${bad.join("；")}`).toEqual([]);
  });

  it("活性验证：若把 faint 调到 3:1 档必须红（扫描器防呆）", () => {
    // 用同公式验证一个已知失败对（白底白字），确保计算路径真能报红
    expect(contrast("#ffffff", "#ffffff")).toBe(1);
    expect(contrast("#ffffff", "#ffffff") < 4.5).toBe(true);
  });
});
