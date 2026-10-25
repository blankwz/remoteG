// 把规范注册表同步进 Vite 静态目录（dev/build 前自动执行）
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, '..', '..', 'registry', 'index.json');
const outDir = join(here, '..', 'public');
mkdirSync(outDir, { recursive: true });
copyFileSync(src, join(outDir, 'registry.json'));
console.log('registry.json 已同步');
