/**
 * 日趋势。最多两条系列（色板已过色觉安全六项校验），末点直接标注，
 * 天数超过 14 天自动挂上缩放条，避免密到看不清。
 */

import { useMemo } from "react";
import { EChart, type EChartsOption } from "./EChart";
import { PALETTE } from "@/app/theme/tokens";
import { weekdayOf } from "@/lib/format";

export interface TrendSeries {
  name: string;
  values: (number | null)[];
  /** 只给第一条系列铺面积，两条都铺会互相盖住 */
  area?: boolean | undefined;
}

export interface TrendChartProps {
  days: string[];
  series: TrendSeries[];
  ariaLabel: string;
  height?: number | undefined;
  onPickDay?: ((day: string) => void) | undefined;
  valueSuffix?: string | undefined;
}

export function TrendChart({
  days,
  series,
  ariaLabel,
  height = 240,
  onPickDay,
  valueSuffix = " 起",
}: TrendChartProps) {
  const p = PALETTE;

  const option = useMemo<EChartsOption>(() => {
    const withZoom = days.length > 14;
    return {
      grid: { left: 44, right: 68, top: 16, bottom: withZoom ? 52 : 30, containLabel: false },
      tooltip: {
        trigger: "axis",
        renderMode: "richText",
        axisPointer: { type: "line" },
        formatter: (params: unknown) => {
          const rows = params as {
            axisValue: string;
            seriesName: string;
            value: number | null;
          }[];
          const day = rows[0]?.axisValue ?? "";
          const head = `${day} ${weekdayOf(day)}`;
          const body = rows
            .map(
              (r) =>
                `${r.seriesName}：${r.value ?? "数据不完整"}${r.value == null ? "" : valueSuffix}`,
            )
            .join("\n");
          return `${head}\n${body}`;
        },
      },
      xAxis: {
        type: "category",
        data: days,
        boundaryGap: false,
        axisLabel: { formatter: (v: string) => v.slice(5), color: p.inkMuted },
      },
      yAxis: { type: "value", minInterval: 1 },
      dataZoom: withZoom
        ? [{ type: "slider", height: 18, bottom: 12 }, { type: "inside" }]
        : undefined,
      series: series.map((s, i) => ({
        name: s.name,
        type: "line" as const,
        data: s.values,
        smooth: false,
        symbolSize: 7,
        lineStyle: { width: 2 },
        areaStyle: s.area ? { opacity: 0.09 } : undefined,
        endLabel: {
          show: true,
          formatter: `${s.name} {c}`,
          color: p.inkSecondary,
          fontFamily: "var(--c2e-font-mono)",
          fontWeight: 600,
          fontSize: 11,
          distance: 8,
        },
        emphasis: { focus: "series" as const },
        z: series.length - i,
      })),
    };
  }, [days, series, p, valueSuffix]);

  const onEvent = useMemo(
    () =>
      onPickDay
        ? {
            type: "click",
            handler: (params: unknown) => {
              const name = (params as { name?: string }).name;
              if (name) onPickDay(name);
            },
          }
        : undefined,
    [onPickDay],
  );

  return <EChart option={option} height={height} ariaLabel={ariaLabel} onEvent={onEvent} />;
}
