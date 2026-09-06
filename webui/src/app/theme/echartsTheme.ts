/**
 * ECharts 主题：与 AntD 共用同一批设计令牌，图表和界面不会出现两套灰、两套蓝。
 *
 * 图表纪律（与配色规则同源）：
 *   坐标轴与网格是背景，不抢数据 —— 网格线用 gridline，轴线用 axis，都不用 ink。
 *   分类系列最多两条，超过两条改用分面小多图。
 *   顺序色阶（热力图）单一色相，零到大；缺格（NULL）不画成 0，由调用方传 null。
 */

import * as echarts from "echarts/core";
import { FONT_MONO, FONT_SANS, FONT_SIZE, PALETTE } from "./tokens";

export const ECHARTS_THEME_NAME = "c2e-light";

let registered = false;

export function registerEchartsThemes(): void {
  if (registered) return;
  registered = true;
  const p = PALETTE;
  const axisCommon = {
    axisLine: { show: true, lineStyle: { color: p.axis } },
    axisTick: { show: false },
    axisLabel: { color: p.inkMuted, fontSize: FONT_SIZE.xs, fontFamily: FONT_SANS },
    splitLine: { show: true, lineStyle: { color: p.gridline, width: 1 } },
    splitArea: { show: false },
    nameTextStyle: { color: p.inkSecondary, fontSize: FONT_SIZE.xs },
  };
  echarts.registerTheme(ECHARTS_THEME_NAME, {
    color: [...p.series],
    backgroundColor: "transparent",
    textStyle: { fontFamily: FONT_SANS, color: p.ink },
    title: {
      textStyle: { color: p.ink, fontSize: FONT_SIZE.md, fontWeight: 600 },
      subtextStyle: { color: p.inkMuted, fontSize: FONT_SIZE.xs },
    },
    valueAxis: axisCommon,
    categoryAxis: { ...axisCommon, splitLine: { show: false } },
    logAxis: axisCommon,
    timeAxis: axisCommon,
    legend: { textStyle: { color: p.inkSecondary, fontSize: FONT_SIZE.sm } },
    tooltip: {
      backgroundColor: p.ink,
      borderWidth: 0,
      padding: [8, 10],
      textStyle: { color: p.plane, fontSize: FONT_SIZE.sm, fontFamily: FONT_SANS },
      extraCssText: "box-shadow:0 8px 26px rgba(0,0,0,0.22);border-radius:4px;",
      axisPointer: {
        lineStyle: { color: p.axis, width: 1, type: "dashed" },
        crossStyle: { color: p.axis, width: 1, type: "dashed" },
        label: { backgroundColor: p.inkSecondary, fontFamily: FONT_MONO },
      },
    },
    line: { symbolSize: 7, symbol: "circle", smooth: false, lineStyle: { width: 2 } },
    // 条形只在数据端倒角，基线端保持方角，读者一眼看到起点在哪
    bar: { itemStyle: { barBorderRadius: [0, 2, 2, 0] } },
    scatter: { symbolSize: 12 },
    visualMap: { textStyle: { color: p.inkSecondary, fontSize: FONT_SIZE.xs } },
    dataZoom: {
      borderColor: p.hairline,
      fillerColor: p.accentWash,
      handleStyle: { color: p.accent, borderColor: p.accent },
      moveHandleStyle: { color: p.accentLine },
      textStyle: { color: p.inkMuted, fontSize: FONT_SIZE.xs, fontFamily: FONT_MONO },
      dataBackground: {
        lineStyle: { color: p.axis },
        areaStyle: { color: p.gridline },
      },
      selectedDataBackground: {
        lineStyle: { color: p.accent },
        areaStyle: { color: p.accentWash },
      },
    },
  });
}
