/**
 * AntD 主题映射：把设计令牌翻成 ThemeConfig。
 *
 * **刻意避开 AntD 默认观感**：默认那套是电光蓝 `#1677ff` + 冷灰 + 6px 圆角 + 系统字体，
 * 一眼就是「中台模板」。这里换成低饱和钢蓝、暖中性灰、4px 圆角、IBM Plex，
 * 并把控件高度和表格行距压紧到分析工作台该有的密度。
 */

import { theme as antdTheme } from "antd";
import type { ThemeConfig } from "antd";
import { FONT_SANS, FONT_SIZE, PALETTE, RADIUS } from "./tokens";

export function buildAntdTheme(): ThemeConfig {
  const p = PALETTE;
  return {
    algorithm: antdTheme.defaultAlgorithm,
    token: {
      colorPrimary: p.accent,
      colorInfo: p.accent,
      colorSuccess: p.good,
      colorWarning: p.warning,
      colorError: p.critical,

      colorBgBase: p.surface,
      colorBgLayout: p.plane,
      colorBgContainer: p.surface,
      colorBgElevated: p.surface,
      colorTextBase: p.ink,
      colorText: p.ink,
      colorTextSecondary: p.inkSecondary,
      colorTextTertiary: p.inkMuted,
      colorTextQuaternary: p.inkMuted,
      colorBorder: p.hairline,
      colorBorderSecondary: p.hairlineSoft,
      colorSplit: p.hairlineSoft,

      fontFamily: FONT_SANS,
      fontSize: FONT_SIZE.base,
      fontSizeHeading1: FONT_SIZE.hero,
      fontSizeHeading4: FONT_SIZE.md,
      fontSizeHeading5: FONT_SIZE.base,

      borderRadius: RADIUS.box,
      borderRadiusLG: RADIUS.box,
      borderRadiusSM: RADIUS.box - 1,
      borderRadiusXS: RADIUS.box - 2,

      controlHeight: 30,
      controlHeightSM: 26,
      lineWidth: 1,
      wireframe: false,

      // 分析工作台不靠投影分层，靠发丝线。投影一律收到几乎不可见。
      boxShadow: "0 1px 2px rgba(0,0,0,0.05)",
      boxShadowSecondary: "0 4px 16px rgba(0,0,0,0.08)",
      boxShadowTertiary: "0 1px 2px rgba(0,0,0,0.04)",
    },
    components: {
      Layout: { headerBg: p.surface, bodyBg: p.plane, headerHeight: 52, headerPadding: "0 16px" },
      Table: {
        headerBg: p.raised,
        headerColor: p.inkSecondary,
        headerSplitColor: "transparent",
        borderColor: p.hairlineSoft,
        rowHoverBg: p.raised,
        cellPaddingBlock: 7,
        cellPaddingInline: 10,
        cellPaddingBlockSM: 5,
        cellPaddingInlineSM: 8,
        headerBorderRadius: 0,
        footerBg: "transparent",
      },
      Card: { headerHeight: 40, headerFontSize: FONT_SIZE.md, paddingLG: 14 },
      Segmented: { itemSelectedBg: p.accentWash, itemSelectedColor: p.accent, trackBg: p.raised },
      Tabs: { horizontalItemPadding: "8px 0", horizontalMargin: "0 0 12px 0" },
      Statistic: { contentFontSize: FONT_SIZE.hero, titleFontSize: FONT_SIZE.sm },
      Tag: { defaultBg: p.raised, defaultColor: p.inkSecondary },
      Alert: { withDescriptionPadding: "10px 14px" },
      Drawer: { footerPaddingBlock: 10, footerPaddingInline: 16 },
      Select: { optionSelectedBg: p.accentWash },
      Tooltip: { colorBgSpotlight: p.ink, colorTextLightSolid: p.plane },
      Descriptions: { itemPaddingBottom: 8, titleMarginBottom: 8 },
      Empty: { controlHeightLG: 40 },
    },
  };
}
