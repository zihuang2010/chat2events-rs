/**
 * ECharts 的唯一挂载点。按需引入图表与组件，不整包 import（整包约 1MB）。
 *
 * 两件事在这里一次性做对，视图不再各自操心：
 *   容器尺寸变化时 resize（ResizeObserver，不监听 window.resize）
 *   卸载时 dispose（否则切页会漏 canvas 和事件）
 */

import { BarChart, LineChart, PieChart, ScatterChart } from "echarts/charts";
import {
  DataZoomComponent,
  GridComponent,
  LegendComponent,
  TooltipComponent,
} from "echarts/components";
import * as echarts from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";
import { LabelLayout } from "echarts/features";
import { useEffect, useRef } from "react";
import { ECHARTS_THEME_NAME } from "@/app/theme/echartsTheme";

echarts.use([
  LineChart,
  BarChart,
  PieChart,
  ScatterChart,
  GridComponent,
  TooltipComponent,
  LegendComponent,
  DataZoomComponent,
  LabelLayout,
  CanvasRenderer,
]);

export type EChartsOption = Parameters<echarts.ECharts["setOption"]>[0];

export interface EChartProps {
  option: EChartsOption;
  height: number;
  /** 图表的可访问描述。屏幕阅读器只能读到它，所以必须具体到「这张图讲什么」 */
  ariaLabel: string;
  onEvent?: { type: string; handler: (params: unknown) => void } | undefined;
  className?: string | undefined;
}

export function EChart({ option, height, ariaLabel, onEvent, className }: EChartProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<echarts.ECharts | null>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const chart = echarts.init(host, ECHARTS_THEME_NAME, { renderer: "canvas" });
    chartRef.current = chart;
    const ro = new ResizeObserver(() => {
      if (!chart.isDisposed()) chart.resize();
    });
    ro.observe(host);
    return () => {
      ro.disconnect();
      chart.dispose();
      chartRef.current = null;
    };
  }, []);

  useEffect(() => {
    // notMerge：切换筛选后系列数量可能变少，不清空会残留上一次的系列
    chartRef.current?.setOption(option, { notMerge: true, lazyUpdate: false });
  }, [option]);

  useEffect(() => {
    const chart = chartRef.current;
    if (!chart || !onEvent) return;
    chart.on(onEvent.type, onEvent.handler);
    return () => {
      // React 按 effect 声明顺序清理，实例可能已被上方的 effect 销毁。
      if (!chart.isDisposed()) chart.off(onEvent.type, onEvent.handler);
    };
  }, [onEvent]);

  return (
    <div
      ref={hostRef}
      className={className ? `c2e-chart ${className}` : "c2e-chart"}
      style={{ height }}
      role="img"
      aria-label={ariaLabel}
    />
  );
}
