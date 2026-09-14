import type { LoadedDataset } from "@/api/source";
import { categoryRows, groupDayStatus, type TaxonomyIndex } from "@/domain/metrics";
import type { CategoryAgg, SummaryRow } from "@/domain/schemas";
import { addDays } from "@/lib/format";
import type { EChartsOption } from "@/components/charts/EChart";
import type { ParentGroup } from "@/features/filters/useAnalytics";
import { chartBase, WORKBENCH_THEME } from "@/app/theme/workbench";

/** 抽屉固定看最新数据日结束的七个自然日，与表格的筛选条件无关。 */
export function roomInsightsWindow(dataset: LoadedDataset) {
  const end = dataset.meta.days.at(-1)!;
  const days = Array.from({ length: 7 }, (_, i) => addDays(end, i - 6));
  return { days, from: days[0]!, to: end };
}

/**
 * 群抽屉的模型 = **`/api/summary?room=X` ＋ `/api/categories?room=X` ＋ 群日记录**。
 *
 * ⚠️ **每日的 P50 / P90 来自 `summary.byDay`，是数据库按天现算的** ——
 * 不能拿区间分位数摊到每天，也不能对每日 p50 求平均：分位数不可加。
 *
 * ⚠️ **`daily[].metrics` 只在那天「本轮已知成功」时才给值**，否则是 `null`：
 * 聚合接口只统计成功的群日，把「没有行」读成 0 会让抽取失败的那天显示成清静的一天。
 */
export function buildRoomInsights(params: {
  dataset: LoadedDataset;
  roomId: string;
  slaSec: number;
  summary: SummaryRow;
  level1: readonly CategoryAgg[];
  level2: readonly CategoryAgg[];
  tax: TaxonomyIndex;
  parents: readonly ParentGroup[];
}) {
  const { dataset, roomId, slaSec, summary, level1, level2, tax, parents } = params;
  const { days, from, to } = roomInsightsWindow(dataset);
  const dayset = new Set(days);
  const records = dataset.groupDaily.filter((row) => row.roomid === roomId && dayset.has(row.dt));
  const byDay = new Map(summary.byDay.map((row) => [row.day, row]));
  const daily = days.map((day) => {
    const cells = records.filter((row) => row.dt === day);
    const status = groupDayStatus(cells[0]);
    const point = byDay.get(day);
    return {
      day,
      status,
      classificationStatus: cells[0]?.classification_status,
      msgs: cells.length ? cells.reduce((sum, row) => sum + row.msg_count, 0) : null,
      senders: cells.length ? cells.reduce((sum, row) => sum + row.sender_count, 0) : null,
      metrics:
        status === "ok"
          ? {
              events: point?.events ?? 0,
              merchant: point?.merchant ?? 0,
              unreplied: point?.unreplied ?? 0,
              overdue: point?.overdue ?? 0,
              overdueRate: point?.overdueRate ?? null,
              p50: point?.p50 ?? null,
              p90: point?.p90 ?? null,
            }
          : null,
    };
  });
  const knownDays = daily.filter((day) => day.status === "ok").length;
  return {
    days,
    daily,
    roomId,
    slaSec,
    from,
    to,
    msgs: records.length ? records.reduce((sum, row) => sum + row.msg_count, 0) : null,
    failed: daily.filter((day) => day.status === "failed").length,
    missing: daily.filter((day) => day.status === "missing").length,
    unknown: daily.filter((day) => day.status === "unknown").length,
    pendingLabels: daily.filter(
      (day) => day.status === "ok" && day.classificationStatus === "pending",
    ).length,
    failedLabels: daily.filter(
      (day) => day.status === "ok" && day.classificationStatus === "failed",
    ).length,
    // 分位数由数据库在七天的事件明细上重算，不是每日 P50 的平均。
    metrics: knownDays ? summary : null,
    categories: {
      level1: categoryRows(level1, "level1", tax, parents, summary.events),
      level2: categoryRows(level2, "level2", tax, parents, summary.events),
    },
  };
}

export type RoomInsights = ReturnType<typeof buildRoomInsights>;

export function buildRoomCharts(model: RoomInsights, level: "level1" | "level2") {
  const skin = WORKBENCH_THEME;
  const base = chartBase(skin);
  const common: EChartsOption = {
    animation: false,
    textStyle: base.textStyle,
    tooltip: { ...base.tooltip, trigger: "axis", confine: true },
    legend: { ...base.legend, top: 0 },
    grid: { left: 54, right: 48, top: 58, bottom: 30 },
    xAxis: {
      ...base.axis,
      type: "category",
      data: model.days,
      axisLabel: {
        color: (day: string) =>
          model.daily.find((row) => row.day === day)?.status === "ok" ? skin.c.ink2 : skin.c.crit,
        fontSize: 11,
        formatter: (day: string) => day.slice(5),
      },
    },
  };
  const axis = { ...base.axis, type: "value" as const, min: 0, minInterval: 1, splitNumber: 3 };
  const categories = model.categories[level];
  const messages: EChartsOption = {
    ...common,
    yAxis: [
      { ...axis, name: "消息 / 条" },
      { ...axis, name: "发言 / 人", splitLine: { show: false } },
    ],
    series: [
      {
        name: "消息总量",
        type: "bar",
        data: model.daily.map((day) => day.msgs),
        barMaxWidth: 28,
        itemStyle: { color: skin.c.barSoftHover },
      },
      {
        name: "发言人数",
        type: "line",
        yAxisIndex: 1,
        data: model.daily.map((day) => day.senders),
        connectNulls: false,
        itemStyle: { color: skin.c.good },
      },
    ],
  };
  const events: EChartsOption = {
    ...common,
    yAxis: { ...axis, name: "事件 / 起" },
    series: [
      {
        name: "事件量",
        type: "bar",
        barMaxWidth: 28,
        data: model.daily.map((day) => day.metrics?.events ?? null),
        itemStyle: { color: skin.c.accent },
      },
      {
        name: "无响应",
        type: "line",
        connectNulls: false,
        data: model.daily.map((day) => day.metrics?.unreplied ?? null),
        itemStyle: { color: skin.c.crit },
      },
    ],
  };
  const response: EChartsOption = {
    ...common,
    yAxis: { ...axis, name: "首响 / 分钟" },
    series: [
      {
        name: "P50",
        type: "line",
        connectNulls: false,
        data: model.daily.map((day) => (day.metrics?.p50 == null ? null : day.metrics.p50 / 60)),
        itemStyle: { color: skin.c.good },
      },
      {
        name: "P90",
        type: "line",
        connectNulls: false,
        data: model.daily.map((day) => (day.metrics?.p90 == null ? null : day.metrics.p90 / 60)),
        itemStyle: { color: skin.c.s2 },
      },
    ],
  };
  const classification: EChartsOption = {
    ...common,
    legend: { show: false },
    grid: { left: 120, right: 42, top: 8, bottom: 28 },
    xAxis: { ...axis, name: "起" },
    yAxis: {
      ...base.axis,
      type: "category",
      inverse: true,
      data: categories.map((row) => row.label),
      axisLabel: { color: skin.c.ink2, fontSize: 11, width: 108, overflow: "truncate" },
    },
    series: [
      {
        name: "事件量",
        type: "bar",
        barMaxWidth: 16,
        data: categories.map((row) => row.count),
        itemStyle: { color: skin.c.accent },
        label: { show: true, position: "right", color: skin.c.ink2 },
      },
    ],
  };
  return { messages, events, response, classification };
}
