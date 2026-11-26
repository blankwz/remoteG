/**
 * 远程画面色彩与清晰度实时增强配置（纯逻辑，零外部依赖）。
 *
 * - 小白一键预设：原色（natural）/ 文字锐利（crisp）/ 鲜艳通透（vivid）/ 护眼暖色（eyecare）；
 * - 高级微调：锐化强度（SVG 3×3 反锐化卷积核，核权重和严格为 1.0，不改变整体亮度）、
 *   亮度、对比度、饱和度、暖色度；
 * - 持久化到 localStorage，跨标签即时广播同步。
 */

export type PicturePreset = "natural" | "crisp" | "vivid" | "eyecare" | "custom";

export interface PictureSettings {
  preset: PicturePreset;
  /** 锐化强度 0..100（0 = 关闭 SVG 卷积锐化，推荐缩放时 25..45）。 */
  sharpness: number;
  /** 亮度百分比 80..120（默认 100）。 */
  brightness: number;
  /** 对比度百分比 80..120（默认 100）。 */
  contrast: number;
  /** 饱和度百分比 80..130（默认 100）。 */
  saturation: number;
  /** 护眼暖色度 0..40（默认 0）。 */
  warmth: number;
}

export const PICTURE_PRESET_LABEL: Record<Exclude<PicturePreset, "custom">, string> = {
  natural: "原色",
  crisp: "文字锐利",
  vivid: "鲜艳通透",
  eyecare: "护眼暖色",
};

export const PICTURE_PRESETS: Record<Exclude<PicturePreset, "custom">, PictureSettings> = {
  natural: {
    preset: "natural",
    sharpness: 0,
    brightness: 100,
    contrast: 100,
    saturation: 100,
    warmth: 0,
  },
  crisp: {
    preset: "crisp",
    sharpness: 38,
    brightness: 100,
    contrast: 103,
    saturation: 102,
    warmth: 0,
  },
  vivid: {
    preset: "vivid",
    sharpness: 22,
    brightness: 101,
    contrast: 105,
    saturation: 112,
    warmth: 0,
  },
  eyecare: {
    preset: "eyecare",
    sharpness: 18,
    brightness: 98,
    contrast: 98,
    saturation: 95,
    warmth: 18,
  },
};

export const DEFAULT_PICTURE_SETTINGS: PictureSettings = PICTURE_PRESETS.natural;

const STORAGE_KEY = "rd.pictureSettings.v1";

function clamp(v: number, min: number, max: number): number {
  if (!Number.isFinite(v)) return min;
  return Math.min(max, Math.max(min, Math.round(v)));
}

export function sanitizePictureSettings(raw: Partial<PictureSettings> | null | undefined): PictureSettings {
  if (!raw || typeof raw !== "object") {
    return { ...DEFAULT_PICTURE_SETTINGS };
  }
  const preset: PicturePreset =
    raw.preset === "natural" ||
    raw.preset === "crisp" ||
    raw.preset === "vivid" ||
    raw.preset === "eyecare" ||
    raw.preset === "custom"
      ? raw.preset
      : "natural";
  return {
    preset,
    sharpness: clamp(raw.sharpness ?? 0, 0, 100),
    brightness: clamp(raw.brightness ?? 100, 80, 120),
    contrast: clamp(raw.contrast ?? 100, 80, 120),
    saturation: clamp(raw.saturation ?? 100, 80, 130),
    warmth: clamp(raw.warmth ?? 0, 0, 40),
  };
}

export function loadPictureSettings(): PictureSettings {
  try {
    const text = window.localStorage.getItem(STORAGE_KEY);
    if (!text) return { ...DEFAULT_PICTURE_SETTINGS };
    return sanitizePictureSettings(JSON.parse(text) as Partial<PictureSettings>);
  } catch {
    return { ...DEFAULT_PICTURE_SETTINGS };
  }
}

type Listener = (next: PictureSettings) => void;
const listeners = new Set<Listener>();

export function savePictureSettings(next: PictureSettings): PictureSettings {
  const clean = sanitizePictureSettings(next);
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(clean));
  } catch {
    // 忽略存储受限环境
  }
  for (const fn of listeners) {
    fn(clean);
  }
  return clean;
}

export function subscribePictureSettings(fn: Listener): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

/**
 * 根据锐化强度（0..100）生成 3×3 反锐化卷积核（十字四邻域拉普拉斯核）。
 *
 * 矩阵形式：
 *   [  0, -a,  0 ]
 *   [ -a, 1+4a, -a ]
 *   [  0, -a,  0 ]
 *
 * 权重之和恒等于 1.0，不改变整体亮度；最大 a = 0.6（对应 100% 强度），
 * 避免过度锐化产生振铃/白边光晕，适合 IDE 代码与桌面文字增强。
 */
export function buildSharpenKernel(sharpness: number): string {
  const clamped = clamp(sharpness, 0, 100);
  if (clamped <= 0) {
    return "0 0 0 0 1 0 0 0 0";
  }
  const a = (clamped / 100) * 0.6;
  const center = 1 + 4 * a;
  const neg = (-a).toFixed(4);
  const c = center.toFixed(4);
  return `0 ${neg} 0 ${neg} ${c} ${neg} 0 ${neg} 0`;
}

/**
 * 合成作用在视频+瓦片容器上的 CSS `filter` 字符串。
 * 全默认（无增强）时返回 `undefined`，避免浏览器触发多余的离屏滤镜合成。
 */
export function buildCanvasFilter(
  settings: PictureSettings,
  svgFilterId?: string,
): string | undefined {
  const parts: string[] = [];
  if (settings.sharpness > 0 && svgFilterId) {
    parts.push(`url(#${svgFilterId})`);
  }
  if (settings.brightness !== 100) {
    parts.push(`brightness(${(settings.brightness / 100).toFixed(2)})`);
  }
  if (settings.contrast !== 100) {
    parts.push(`contrast(${(settings.contrast / 100).toFixed(2)})`);
  }
  if (settings.saturation !== 100) {
    parts.push(`saturate(${(settings.saturation / 100).toFixed(2)})`);
  }
  if (settings.warmth > 0) {
    // 温和暖色过滤蓝光（sepia 控制在 0..0.28，避免画面发黄失真）
    parts.push(`sepia(${((settings.warmth / 40) * 0.28).toFixed(2)})`);
  }
  return parts.length > 0 ? parts.join(" ") : undefined;
}
