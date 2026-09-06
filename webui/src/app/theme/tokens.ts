/**
 * 外壳、公共原语及公共图表的固定浅色令牌。
 * 已确认页面的局部主题在 workbench.ts；保留两者现有视觉，统一由 app/theme 管理。
 */

export interface Palette {
  /** 页面底板，比 surface 略暗，用来把面板托出来 */
  plane: string;
  /** 面板、表格、抽屉的底色 */
  surface: string;
  /** 表头、输入框、次级块 */
  raised: string;

  ink: string;
  inkSecondary: string;
  inkMuted: string;

  hairline: string;
  hairlineSoft: string;
  gridline: string;
  axis: string;

  /** 唯一强调色，同时是数据系列 1 */
  accent: string;
  accentWash: string;
  accentLine: string;

  good: string;
  warning: string;
  critical: string;
  goodInk: string;
  warningInk: string;
  criticalInk: string;

  /** 分类系列色。**最多两条**：超过两条改用分面小多图，不靠颜色区分身份 */
  series: readonly [string, string];
}

export const PALETTE: Palette = {
  plane: "#f7f6f3",
  surface: "#ffffff",
  raised: "#f2f0eb",

  ink: "#1a1917",
  inkSecondary: "#56534d",
  inkMuted: "#8a867e",

  hairline: "rgba(26,25,23,0.11)",
  hairlineSoft: "rgba(26,25,23,0.06)",
  gridline: "#e6e3dc",
  axis: "#c6c2b8",

  accent: "#2a78d6",
  accentWash: "rgba(42,120,214,0.09)",
  accentLine: "rgba(42,120,214,0.32)",

  good: "#0ca30c",
  warning: "#fab219",
  critical: "#d03b3b",
  goodInk: "#006300",
  warningInk: "#7d5800",
  criticalInk: "#a82f2f",

  series: ["#2a78d6", "#eb6834"],
};

/** 字体。拉丁与数字用自托管的 IBM Plex，中文回落系统字体（不打包 CJK 字重）。 */
export const FONT_SANS =
  '"IBM Plex Sans", -apple-system, BlinkMacSystemFont, "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif';
export const FONT_MONO = '"IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, monospace';

export const RADIUS = { box: 4, bar: 2, pill: 999 } as const;

export const FONT_SIZE = { xs: 11, sm: 12, base: 13, md: 14, lg: 16, xl: 20, hero: 28 } as const;

const CSS_VAR_PREFIX = "--c2e";
const kebab = (s: string) => s.replace(/[A-Z]/g, (m) => `-${m.toLowerCase()}`);

/** 把令牌刷成 CSS 变量，供 global.css 与少量手写样式使用。 */
export function applyCssVariables(root: HTMLElement = document.documentElement): void {
  const p = PALETTE;
  const entries = Object.entries(p) as [string, string | readonly string[]][];
  for (const [key, value] of entries) {
    if (typeof value === "string") {
      root.style.setProperty(`${CSS_VAR_PREFIX}-${kebab(key)}`, value);
    } else {
      value.forEach((v, i) => root.style.setProperty(`${CSS_VAR_PREFIX}-${kebab(key)}-${i}`, v));
    }
  }
  root.style.setProperty(`${CSS_VAR_PREFIX}-font-sans`, FONT_SANS);
  root.style.setProperty(`${CSS_VAR_PREFIX}-font-mono`, FONT_MONO);
  root.style.setProperty(`${CSS_VAR_PREFIX}-radius-box`, `${RADIUS.box}px`);
  root.style.setProperty("color-scheme", "light");
}
