/**
 * 工作量 × 服务质量散点。**身份靠直接标注，颜色只编码达标与否** ——
 * 颜色是质量维度（阈值上下），不是人的身份，75 个人不可能用色相区分。
 * 三个量同屏（横轴工作量、纵轴时效、圆面积活跃群数），就是为了防止只看一个数排名。
 *
 * 纵轴取**对数刻度**：首响时长严重右偏（多数人一两分钟，个别上百分钟），
 * 线性轴会把九成的人压成贴着零线的一条，读不出任何差别。
 */

import { useMemo } from "react";
import { EChart, type EChartsOption } from "./EChart";
import { PALETTE } from "@/app/theme/tokens";
import { formatDuration, formatDurationCompact, formatInt, formatPercent } from "@/lib/format";

export interface WorkloadPoint {
  key: string;
  label: string;
  involved: number;
  p50Sec: number;
  rooms: number;
  owned: number;
  overdueRate: number | null;
}

/** 每侧标注的人数上限。标满 75 个名字就是一堵字墙，剩下的交给 hover。 */
const LABELS_PER_SIDE = 6;

export function WorkloadQualityChart({
  points,
  slaSec,
  onPick,
  height = 380,
}: {
  points: WorkloadPoint[];
  /** 首响阈值（秒），决定参考线位置与配色分界 */
  slaSec: number;
  onPick?: ((key: string) => void) | undefined;
  height?: number | undefined;
}) {
  const p = PALETTE;

  const option = useMemo<EChartsOption>(() => {
    // 对数轴不接受 0。P50 为 0 秒实际上不会出现（同秒回复），这里只是兜底，
    // 位置按 1 秒画，tooltip 仍显示真值，不改数。
    const rows = points.map((pt) => ({
      ...pt,
      minutes: Math.max(pt.p50Sec, 1) / 60,
      overdue: pt.p50Sec > slaSec,
    }));
    const hasOverdue = rows.some((r) => r.overdue);
    const slaMin = slaSec / 60;

    const sorted = [...rows].map((r) => r.involved).sort((a, b) => a - b);
    const medianInvolved = sorted[Math.floor(sorted.length / 2)] ?? 0;

    // 两侧各取工作量最高的几个：右上角（量大又慢）必然在标注里，
    // 右下角（量大且快）作为对照也在，中间那坨小工作量的不标。
    const topOf = (overdue: boolean) =>
      rows
        .filter((r) => r.overdue === overdue)
        .sort((a, b) => b.involved - a.involved)
        .slice(0, LABELS_PER_SIDE);
    const labeled = new Set([...topOf(true), ...topOf(false)].map((r) => r.key));

    const markLines: Record<string, unknown>[] = [
      {
        xAxis: medianInvolved,
        lineStyle: { color: p.axis, type: "dashed", width: 1, opacity: 0.8 },
        label: {
          formatter: `工作量中位 ${formatInt(medianInvolved)}`,
          position: "insideEndTop",
          color: p.inkMuted,
          fontSize: 11,
        },
      },
    ];
    // 没人超时就不画阈值线 —— 画了 ECharts 会把纵轴拉到阈值，
    // 反而把真实数据又压扁一次。
    if (hasOverdue)
      markLines.push({
        yAxis: slaMin,
        lineStyle: { color: p.critical, type: "dashed", width: 1, opacity: 0.6 },
        label: {
          formatter: `阈值 ${formatDuration(slaSec)}`,
          position: "insideStartTop",
          color: p.criticalInk,
          fontSize: 11,
        },
      });

    return {
      grid: { left: 64, right: 36, top: 30, bottom: 50 },
      tooltip: {
        trigger: "item",
        renderMode: "richText",
        formatter: (params: unknown) => {
          const i = (params as { dataIndex: number }).dataIndex;
          const pt = rows[i];
          if (!pt) return "";
          return [
            `${pt.label}${pt.overdue ? "  ● 超时" : ""}`,
            `参与事件 ${formatInt(pt.involved)} 起 · 活跃群 ${pt.rooms} 个`,
            `首响归属 ${formatInt(pt.owned)} 起`,
            `本人首响 P50 ${formatDuration(pt.p50Sec) ?? "—"}`,
            `本人首响超时率 ${formatPercent(pt.overdueRate) ?? "—"}`,
          ].join("\n");
        },
      },
      xAxis: {
        type: "value",
        name: "参与事件（起）",
        nameLocation: "middle" as const,
        nameGap: 30,
        minInterval: 1,
        splitLine: { lineStyle: { color: p.hairlineSoft } },
      },
      yAxis: {
        // 对数刻度：下方那一大坨达标的人才展得开
        type: "log",
        logBase: 10,
        name: "本人首响 P50 · 对数刻度",
        nameLocation: "middle" as const,
        nameGap: 48,
        axisLabel: { formatter: (v: number) => formatDurationCompact(v * 60) ?? "" },
        splitLine: { lineStyle: { color: p.hairlineSoft } },
        minorSplitLine: { show: true, lineStyle: { color: p.hairlineSoft, opacity: 0.45 } },
      },
      series: [
        {
          type: "scatter" as const,
          data: rows.map((r) => ({
            value: [r.involved, r.minutes, r.rooms],
            itemStyle: {
              color: r.overdue ? p.critical : p.accent,
              opacity: 0.16,
              borderColor: r.overdue ? p.critical : p.accent,
              borderWidth: 1.5,
            },
            label: {
              show: labeled.has(r.key),
              color: r.overdue ? p.criticalInk : p.inkSecondary,
            },
          })),
          symbolSize: (val: number[]) => Math.min(34, 9 + Math.sqrt(val[2] ?? 1) * 2.6),
          label: {
            position: "top" as const,
            distance: 5,
            formatter: (params: unknown) =>
              rows[(params as { dataIndex: number }).dataIndex]?.label ?? "",
            fontSize: 11,
          },
          labelLayout: { hideOverlap: true },
          // 没标注的人 hover 时把名字亮出来，不必只靠 tooltip 认人
          emphasis: { scale: 1.25, label: { show: true, fontWeight: 600 as const } },
          markLine: {
            silent: true,
            symbol: "none",
            animation: false,
            data: markLines,
          },
          ...(hasOverdue
            ? {
                markArea: {
                  silent: true,
                  itemStyle: { color: p.critical, opacity: 0.04 },
                  data: [[{ yAxis: slaMin }, { yAxis: "max" }]],
                },
              }
            : {}),
          cursor: onPick ? "pointer" : "default",
        },
      ],
    };
  }, [points, slaSec, p, onPick]);

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
      ariaLabel="客服工作量与响应时效对照散点图，横轴参与事件，纵轴本人首响 P50 对数刻度，气泡大小为活跃群数，红色表示首响超过阈值"
      onEvent={onEvent}
    />
  );
}
