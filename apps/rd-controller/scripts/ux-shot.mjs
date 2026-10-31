/**
 * UX 评审截图脚本（puppeteer-core 版）：真实等待 + 布局稳定轮询。
 *
 * 背景：headless 冷加载下页面布局偶发塌陷（高度链瞬间失效，stage 塌成
 * 2px）——浏览器真机从未复现，属 headless 特有的时序竞态。因此截图前
 * 轮询主容器高度，未稳定则 reload 重试，保证产出可验收的截图。
 * 用法: node scripts/ux-shot.mjs <输出目录名>
 */
import puppeteer from "puppeteer-core";
import { mkdirSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const OUT = process.argv[2];
if (!OUT) {
  console.error("用法: node scripts/ux-shot.mjs <输出目录名>");
  process.exit(1);
}

const BASE = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DIR = resolve(BASE, "screenshots", OUT);
mkdirSync(DIR, { recursive: true });

const CHROME = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
// 生产构建预览（vite preview）：静态 CSS 零竞态，截图验收用
const PORT = process.env.SHOT_PORT ?? 4173;

const shots = [
  ["01-devices", "#/devices"],
  ["02-windows", "#/windows/dev-studio"],
  ["03-session-empty", "#/session/dev-studio"],
  ["04-session-window", "#/session/dev-studio?open=3301"],
  ["05-session-term", "#/session/dev-studio?term=pwsh"],
];

const browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: "new",
  args: ["--disable-gpu", "--hide-scrollbars", "--window-size=1440,900"],
  defaultViewport: { width: 1440, height: 900 },
});

/** 等待页面布局进入稳定终态（main 高度 >300px 且连续两次采样一致）。 */
async function waitForStableLayout(page) {
  for (let attempt = 0; attempt < 5; attempt += 1) {
    let prev = -1;
    for (let i = 0; i < 20; i += 1) {
      const h = await page.evaluate(() => {
        const main = document.querySelector("main");
        return main !== null ? Math.round(main.getBoundingClientRect().height) : 0;
      });
      if (h > 300 && h === prev) {
        return true;
      }
      prev = h;
      await new Promise((r) => setTimeout(r, 250));
    }
    // 20 次采样未稳定 → reload 重试
    console.log(`  layout unstable (h=${prev}), reloading...`);
    await page.reload({ waitUntil: "networkidle2" });
  }
  return false;
}

try {
  for (const [name, hash] of shots) {
    const page = await browser.newPage();
    await page.goto(`http://localhost:${PORT}/${hash}`, { waitUntil: "networkidle2", timeout: 30000 });
    const stable = await waitForStableLayout(page);
    if (!stable) {
      console.log(`  WARN: ${name} layout never stabilized`);
    }
    // 布局稳定后再给 mock IPC / 动画留缓冲
    await new Promise((r) => setTimeout(r, 1500));
    await page.screenshot({ path: resolve(DIR, `${name}.png`) });
    console.log(`shot: ${name}`);
    await page.close();
  }
} finally {
  await browser.close();
}
console.log(`done -> ${DIR}`);
