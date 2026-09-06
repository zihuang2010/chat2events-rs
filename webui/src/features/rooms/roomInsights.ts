import type { LoadedDataset } from "@/api/source";
import { aggregate, categoryRollup } from "@/domain/metrics";
import { addDays } from "@/lib/format";
import type { EChartsOption } from "@/components/charts/EChart";
import { chartBase, WORKBENCH_THEME } from "@/app/theme/workbench";

/** 独立于表格筛选，始终使用最新数据日结束的七个自然日。 */
export function buildRoomInsights(dataset: LoadedDataset, roomId: string, slaSec: number) {
  const end = dataset.meta.days.at(-1)!;
  const days = Array.from({ length: 7 }, (_, i) => addDays(end, i - 6));
  const dayset = new Set(days);
  const records = dataset.groupDaily.filter((row) => row.roomid === roomId && dayset.has(row.dt));
  const roomEvents = dataset.events.filter(
    (event) => event.roomid === roomId && dayset.has(event.occurred_on),
  );
  const daily = days.map((day) => {
    const cells = records.filter((row) => row.dt === day);
    const status = !cells.length
      ? "missing"
      : cells.some((row) => row.extraction_status === "failed")
        ? "failed"
        : "ok";
    return {
      day,
      status,
      msgs: cells.length ? cells.reduce((sum, row) => sum + row.msg_count, 0) : null,
      senders: cells.length ? cells.reduce((sum, row) => sum + row.sender_count, 0) : null,
      metrics:
        status === "ok"
          ? aggregate(
              roomEvents.filter((event) => event.occurred_on === day),
              slaSec,
              day,
            )
          : null,
    };
  });
  const knownDays = new Set(daily.filter((day) => day.status === "ok").map((day) => day.day));
  const events = roomEvents.filter((event) => knownDays.has(event.occurred_on));
  return {
    days,
    daily,
    events,
    roomId,
    slaSec,
    from: days[0]!,
    to: end,
    msgs: records.length ? records.reduce((sum, row) => sum + row.msg_count, 0) : null,
    failed: daily.filter((day) => day.status === "failed").length,
    missing: daily.filter((day) => day.status === "missing").length,
    // 分位数在七天事件上重算，不平均每日 P50 / P90。
    metrics: knownDays.size ? aggregate(events, slaSec, end) : null,
    categories: {
      level1: categoryRollup(events, "level1", dataset.taxIndex),
      level2: categoryRollup(events, "level2", dataset.taxIndex),
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
        name: "消息数",
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
        name: "事件数",
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
        name: "事件数",
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
