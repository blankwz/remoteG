import { describe, expect, it } from "vitest";
import {
  buildCanvasFilter,
  buildSharpenKernel,
  DEFAULT_PICTURE_SETTINGS,
  PICTURE_PRESETS,
  sanitizePictureSettings,
} from "./pictureEnhancer";

describe("pictureEnhancer", () => {
  it("keeps sharpen convolution kernel sum equal to 1.0 (zero brightness shift)", () => {
    for (const s of [0, 18, 38, 50, 100]) {
      const nums = buildSharpenKernel(s)
        .split(/\s+/)
        .map((n) => Number(n));
      expect(nums).toHaveLength(9);
      const sum = nums.reduce((acc, n) => acc + n, 0);
      expect(sum).toBeCloseTo(1.0, 3);
    }
  });

  it("returns undefined filter for default natural settings (zero overhead)", () => {
    expect(buildCanvasFilter(DEFAULT_PICTURE_SETTINGS, "rd-sharpen")).toBeUndefined();
  });

  it("composes SVG sharpen + CSS color filters for crisp/vivid/eyecare presets", () => {
    const crisp = buildCanvasFilter(PICTURE_PRESETS.crisp, "rd-sharpen-1");
    expect(crisp).toContain("url(#rd-sharpen-1)");
    expect(crisp).toContain("contrast(");

    const vivid = buildCanvasFilter(PICTURE_PRESETS.vivid, "rd-sharpen-1");
    expect(vivid).toContain("saturate(1.12)");

    const eye = buildCanvasFilter(PICTURE_PRESETS.eyecare, "rd-sharpen-1");
    expect(eye).toContain("sepia(");
  });

  it("clamps out-of-range settings safely", () => {
    const s = sanitizePictureSettings({
      preset: "custom",
      sharpness: 999,
      brightness: 10,
      contrast: 500,
      saturation: -5,
      warmth: 99,
    });
    expect(s).toEqual({
      preset: "custom",
      sharpness: 100,
      brightness: 80,
      contrast: 120,
      saturation: 80,
      warmth: 40,
    });
  });
});
