/**
 * 工作量 × 服务质量散点。**单系列**：身份靠直接标注，不靠颜色。
 * 三个量同屏（横轴工作量、纵轴时效、圆面积活跃群数），就是为了防止只看一个数排名。
 */

import { useMemo } from "react";
import { EChart, type EChartsOption } from "./EChart";
import { PALETTE } from "@/app/theme/tokens";
import { formatDuration, formatPercent } from "@/lib/format";

export interface WorkloadPoint {
  key: string;
  label: string;
  involved: number;
  p50Sec: number;
  rooms: number;
  owned: number;
  overdueRate: number | null;
}

export function WorkloadQualityChart({
  points,
  onPick,
  height = 300,
}: {
  points: WorkloadPoint[];
  onPick?: ((key: string) => void) | undefined;
  height?: number | undefined;
}) {
  const p = PALETTE;

  const option = useMemo<EChartsOption>(
    () => ({
      grid: { left: 58, right: 28, top: 22, bottom: 46 },
      tooltip: {
        trigger: "item",
        renderMode: "richText",
        formatter: (params: unknown) => {
          const i = (params as { dataIndex: number }).dataIndex;
          const pt = points[i];
          if (!pt) return "";
          return [
            pt.label,
            `参与事件 ${pt.involved} 起 · 首响归属 ${pt.owned} 起`,
            `活跃群 ${pt.rooms} 个 · 本人首响 P50 ${formatDuration(pt.p50Sec) ?? "—"}`,
            `本人首响超时率 ${formatPercent(pt.overdueRate) ?? "—"}`,
          ].join("\n");
        },
      },
      xAxis: {
        type: "value",
        name: "参与事件（起）",
        nameLocation: "middle" as const,
        nameGap: 28,
        minInterval: 1,
        axisLabel: { color: p.inkSecondary },
      },
      yAxis: {
        type: "value",
        name: "本人首响 P50（分钟）",
        nameLocation: "middle" as const,
        nameGap: 40,
        axisLabel: {
          color: p.inkSecondary,
          formatter: (v: number) => String(Number((v / 60).toFixed(1))),
        },
      },
      series: [
        {
          type: "scatter" as const,
          data: points.map((pt) => [pt.involved, pt.p50Sec, pt.rooms]),
          symbolSize: (val: number[]) => 10 + Math.sqrt(val[2] ?? 1) * 3.4,
          itemStyle: { color: p.accent, opacity: 0.28, borderColor: p.accent, borderWidth: 2 },
          label: {
            show: true,
            position: "top" as const,
            formatter: (params: unknown) =>
              points[(params as { dataIndex: number }).dataIndex]?.label ?? "",
            color: p.inkSecondary,
            opacity: 1,
            fontSize: 11,
          },
          labelLayout: { hideOverlap: true },
          cursor: onPick ? "pointer" : "default",
        },
      ],
    }),
    [points, p, onPick],
  );

  const onEvent = useMemo(
    () =>
      onPick
        ? {
            type: "click",
            handler: (params: unknown) => {
              const pt = points[(params as { dataIndex: number }).dataIndex];
              if (pt) onPick(pt.key);
            },
          }
        : undefined,
    [onPick, points],
  );

  return (
    <EChart
      option={option}
      height={height}
      ariaLabel="客服工作量与响应时效对照散点图，横轴参与事件，纵轴本人首响 P50，气泡大小为活跃群数"
      onEvent={onEvent}
    />
  );
}
