import { theme as antdTheme, type ThemeConfig } from "antd";

export interface WorkbenchColors {
  paper: string;
  surface: string;
  raised: string;
  ink: string;
  ink2: string;
  muted: string;
  rule: string;
  ruleSoft: string;
  accent: string;
  accentWash: string;
  good: string;
  warn: string;
  crit: string;
  /** 数据系列色，最多两条；更多身份靠分面或直接标注，不靠颜色 */
  s2: string;
  /** 图表里「量纲柱」的中性色。**刻意不是系列色也不是色阶** —— 消息量是底噪不是身份，
   *  用同色相的浅色会和事件线糊在一起，用第三个色相又会破坏两条系列色的上限。 */
  barSoft: string;
  barSoftHover: string;
}

export interface WorkbenchTheme {
  font: { display: string; body: string; mono: string };
  c: WorkbenchColors;
  radius: { card: number; ctl: number };
}

const SANS_CJK = '"Noto Sans SC"';
const SYS = "-apple-system, BlinkMacSystemFont, sans-serif";

/**
 * 已确认的 Signal 工作台主题，布局样式在 app/workbench.css。
 */
export const WORKBENCH_THEME: WorkbenchTheme = {
  font: {
    display: `"Space Grotesk", ${SANS_CJK}, ${SYS}`,
    body: `"Space Grotesk", ${SANS_CJK}, ${SYS}`,
    mono: '"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
  },
  c: {
    paper: "#fbfbfd", // oklch 98.6% .003 285
    surface: "#ffffff",
    raised: "#f2f3f8",
    ink: "#101119", // oklch 18% .02 280
    ink2: "#565b6e",
    muted: "#8b90a3",
    rule: "#e7e8f0",
    ruleSoft: "#f1f2f7",
    accent: "#5b4bf5", // oklch 54% .23 275 靛紫
    accentWash: "#efedff",
    good: "#0f9d6f",
    warn: "#d97706",
    crit: "#e0353c",
    s2: "#ff7a45",
    barSoft: "#d8dae8",
    barSoftHover: "#c2c5da",
  },
  radius: { card: 14, ctl: 9 },
};

/** 令牌刷成内联 CSS 变量：CSS 与 TS 只有一处值，改一个地方两边同时动。 */
export function cssVars(s: WorkbenchTheme): Record<string, string> {
  const v: Record<string, string> = {
    "--pv-font-display": s.font.display,
    "--pv-font-body": s.font.body,
    "--pv-font-mono": s.font.mono,
    "--pv-radius": `${s.radius.card}px`,
    "--pv-radius-ctl": `${s.radius.ctl}px`,
  };
  for (const [k, val] of Object.entries(s.c)) {
    if (typeof val === "string") v[`--pv-${k}`] = val;
  }
  return v;
}

/** D 工作台的 AntD 主题，仅覆盖 Workbench 子树。 */
export function antdThemeFor(s: WorkbenchTheme): ThemeConfig {
  return {
    algorithm: antdTheme.defaultAlgorithm,
    token: {
      colorPrimary: s.c.accent,
      colorInfo: s.c.accent,
      colorSuccess: s.c.good,
      colorWarning: s.c.warn,
      colorError: s.c.crit,
      colorBgBase: s.c.surface,
      colorBgLayout: s.c.paper,
      colorBgContainer: s.c.surface,
      colorBgElevated: s.c.surface,
      colorTextBase: s.c.ink,
      colorText: s.c.ink,
      colorTextSecondary: s.c.ink2,
      colorTextTertiary: s.c.muted,
      colorTextQuaternary: s.c.muted,
      colorBorder: s.c.rule,
      colorBorderSecondary: s.c.ruleSoft,
      colorSplit: s.c.ruleSoft,
      fontFamily: s.font.body,
      fontSize: 13,
      borderRadius: s.radius.card,
      borderRadiusLG: s.radius.card,
      borderRadiusSM: s.radius.ctl,
      controlHeight: 32,
      wireframe: false,
      boxShadow: "none",
      boxShadowSecondary: "none",
      boxShadowTertiary: "none",
    },
    components: {
      Table: {
        headerBg: "transparent",
        headerColor: s.c.muted,
        headerSplitColor: "transparent",
        borderColor: s.c.ruleSoft,
        rowHoverBg: s.c.raised,
        cellPaddingBlock: 9,
        cellPaddingInline: 12,
        cellPaddingBlockSM: 7,
        cellPaddingInlineSM: 10,
        headerBorderRadius: 0,
        footerBg: "transparent",
      },
      Tag: { defaultBg: s.c.raised, defaultColor: s.c.ink2 },
      Tooltip: { colorBgSpotlight: s.c.ink, colorTextLightSolid: s.c.paper },
      Alert: { withDescriptionPadding: "12px 16px" },
      Empty: { controlHeightLG: 40 },
    },
  };
}

/**
 * 图表的公共 option 片段。**不注册新的 ECharts 主题** —— 全局那套仍在 init 时生效，
 * 这里把每个视觉属性显式写进 option 覆盖掉它，省掉一层主题注册与实例重建。
 */
export function chartBase(s: WorkbenchTheme) {
  const axis = {
    axisLine: { show: true, lineStyle: { color: s.c.rule } },
    axisTick: { show: false },
    axisLabel: { color: s.c.muted, fontSize: 11, fontFamily: s.font.mono },
    splitLine: { show: true, lineStyle: { color: s.c.ruleSoft } },
    nameTextStyle: { color: s.c.muted, fontSize: 11 },
  };
  return {
    axis,
    textStyle: { fontFamily: s.font.body, color: s.c.ink },
    tooltip: {
      backgroundColor: s.c.ink,
      borderWidth: 0,
      padding: [9, 11] as [number, number],
      textStyle: { color: s.c.paper, fontSize: 12, fontFamily: s.font.body },
    },
    legend: { textStyle: { color: s.c.ink2, fontSize: 11, fontFamily: s.font.body } },
  };
}
