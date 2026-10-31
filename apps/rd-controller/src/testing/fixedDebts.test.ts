/**
 * 已修债务回归门禁测试（R11 Phase 1）：实态扫描 + 注入活性验证。
 * 探针 = 修复时删除的源码串，回潮即红；扫描器为纯函数，自身防橡皮图章。
 * 扫描排除 testing/ 与 *.test.ts(x)（门禁自身不是债务）。
 */
/// <reference types="node" />
import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  DEBT_GATES,
  SOURCE_INVARIANTS,
  scanGate,
  scanInvariant,
  type DebtGate,
  type SourceInvariant,
} from "./fixedDebts";

/** 递归收集 src/ 下生产源码（排除 testing/ 与 *.test.ts(x)）。 */
function collectProductionSources(): Map<string, string> {
  const root = fileURLToPath(new URL("..", import.meta.url)).replace(/\\/g, "/").replace(/\/$/, "");
  const out = new Map<string, string>();
  const walk = (dir: string): void => {
    for (const name of readdirSync(dir)) {
      const p = `${dir}/${name}`.replace(/\\/g, "/");
      if (statSync(p).isDirectory()) {
        walk(p);
        continue;
      }
      const rel = p.slice(root.length + 1);
      const excluded = rel.startsWith("testing/") || /\.test\.tsx?$/.test(rel);
      if (!excluded) {
        out.set(rel, readFileSync(p, "utf8"));
      }
    }
  };
  walk(root);
  return out;
}

describe("已修债务回归门禁（实态扫描）", () => {
  it("探针保持消失：每项已修债务的探针不得回潮", () => {
    const contents = collectProductionSources();
    const violations = DEBT_GATES.map((g) => scanGate(contents, g)).filter(
      (v) => v !== null,
    );
    const text = violations.map((v) => `${v.id}: ${v.detail}`).join("\n");
    expect(violations.length, `门禁回潮：\n${text}`).toBe(0);
  });

  it("跨文件不变量：bridge 收口/单监听/console.log 禁令", () => {
    const contents = collectProductionSources();
    const violations = SOURCE_INVARIANTS.map((i) =>
      scanInvariant(contents, i),
    ).filter((v) => v !== null);
    const text = violations.map((v) => `${v.id}: ${v.detail}`).join("\n");
    expect(violations.length, `不变量回潮：\n${text}`).toBe(0);
  });

  it("清单↔实态双向锁定：每项债务声明的文件必须存在", () => {
    const contents = collectProductionSources();
    const missing = DEBT_GATES.filter((g) => !contents.has(g.file));
    expect(missing.map((g) => `${g.id} → ${g.file}`).join(", ")).toBe("");
  });
});

// ---------------------------------------------------------------------------
// 注入活性验证（扫描器防橡皮图章）
// ---------------------------------------------------------------------------

const G: DebtGate = {
  id: "T",
  title: "测试门禁",
  file: "session/a.tsx",
  probe: "const clean",
};

const INV: SourceInvariant = {
  id: "TI",
  title: "测试不变量",
  scope: "pages/",
  probe: 'from "@tauri-apps/api/core"',
  maxCount: 0,
};

describe("注入活性验证（扫描器防橡皮图章）", () => {
  it("种探针必须红，干净必须放行，清单失实必须红", () => {
    const contents = new Map<string, string>([
      ["session/a.tsx", "const clean = 1;\n"],
      ["pages/b.tsx", 'import { invoke } from "@tauri-apps/api/core";\n'],
    ]);
    expect(scanGate(contents, G)).not.toBeNull();
    expect(scanInvariant(contents, INV)).not.toBeNull();

    const clean = new Map(contents);
    clean.set("session/a.tsx", "const other = 2;\n");
    clean.set("pages/b.tsx", 'import { call } from "./bridge";\n');
    expect(scanGate(clean, G)).toBeNull();
    expect(scanInvariant(clean, INV)).toBeNull();

    const gone: DebtGate = { ...G, file: "nope/missing.ts" };
    expect(scanGate(new Map(), gone)).not.toBeNull();
  });

  it("计数语义：合法残留恰 N 处放行，超出即红", () => {
    const gate: DebtGate = {
      id: "T2",
      title: "t",
      file: "session/a.tsx",
      probe: 'aria-hidden="true"',
      maxCount: 1,
    };
    const contents = new Map<string, string>([
      ["session/a.tsx", 'x aria-hidden="true" y'],
    ]);
    expect(scanGate(contents, gate)).toBeNull();
    contents.set("session/a.tsx", 'aria-hidden="true" aria-hidden="true"');
    expect(scanGate(contents, gate)).not.toBeNull();
  });
});
