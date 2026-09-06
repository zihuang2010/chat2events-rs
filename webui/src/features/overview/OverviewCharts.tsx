/* eslint-disable react-refresh/only-export-components -- Export the option builder for real ECharts rendering tests. */
import { useState } from "react";
import { DownOutlined } from "@ant-design/icons";
import { Link } from "react-router-dom";
import { EChart, type EChartsOption } from "@/components/charts/EChart";
import {
  type categoryRollup,
  dailyCounts,
  isMerchant,
  isOverdue,
  isUnreplied,
} from "@/domain/metrics";
import { formatPercent } from "@/lib/format";
import type { DecoratedEvent } from "@/domain/schemas";
import { msgRollup, type OverviewProps } from "./overviewMetrics";
import { WORKBENCH_THEME, chartBase } from "@/app/theme/workbench";

const skin = WORKBENCH_THEME;

export function buildOverviewTrend(
  analytics: OverviewProps["analytics"],
  msgs: ReturnType<typeof msgRollup>["byDay"],
  hourly: boolean,
): EChartsOption {
  const { events, days, slaSec } = analytics;
  const base = chartBase(skin);
  const axis = {
    type: "value" as const,
    min: 0,
    minInterval: 1,
    axisLabel: { color: skin.c.ink2, fontSize: 11 },
    splitLine: { lineStyle: { color: skin.c.rule } },
    axisLine: { show: false },
    axisTick: { show: false },
  };
  const category = {
    type: "category" as const,
    axisTick: { show: false },
    axisLine: { lineStyle: { color: skin.c.rule } },
    axisLabel: { color: skin.c.ink2, fontSize: 11 },
  };
  const common = {
    animation: false,
    backgroundColor: "transparent",
    textStyle: base.textStyle,
    tooltip: { ...base.tooltip, trigger: "axis" as const, confine: true },
  };
  if (hourly) {
    const counts = Array.from({ length: 24 }, () => 0);
    const overdue = Array.from({ length: 24 }, () => 0);
    for (const e of events) {
      const h = Number(e.first_msg_time.slice(11, 13));
      counts[h] = (counts[h] ?? 0) + 1;
      if (isOverdue(e, slaSec)) overdue[h] = (overdue[h] ?? 0) + 1;
    }
    return {
      ...common,
      grid: { left: 44, right: 12, top: 34, bottom: 28 },
      xAxis: { ...category, data: counts.map((_, h) => String(h).padStart(2, "0")) },
      yAxis: { ...axis, name: "事件 / 起", nameTextStyle: { color: skin.c.ink2 } },
      series: [
        {
          name: "首响超时（含无响应）",
          type: "bar",
          stack: "events",
          data: overdue,
          barMaxWidth: 20,
          itemStyle: { color: skin.c.warn },
        },
        {
          name: "其他事件",
          type: "bar",
          stack: "events",
          data: counts.map((n, h) => n - (overdue[h] ?? 0)),
          itemStyle: { color: skin.c.barSoft },
        },
      ],
    };
  }
  // 全部群日失败时断开事件线；部分失败仍展示已知量并标记日期。
  const known = (counts: number[]) =>
    counts.map((n, i) => (!msgs[i]?.cells || msgs[i].failed === msgs[i].cells ? null : n));
  const dates = {
    ...category,
    data: days,
    axisLabel: {
      ...category.axisLabel,
      formatter: (day: string) => day.slice(5),
      color: (day: string) => (msgs[days.indexOf(day)]?.failed ? skin.c.crit : skin.c.ink2),
    },
  };
  return {
    ...common,
    grid: [
      { left: 44, right: 12, top: 34, height: 146 },
      { left: 44, right: 12, top: 236, height: 74 },
    ],
    xAxis: [
      { ...dates, gridIndex: 0 },
      { ...dates, gridIndex: 1 },
    ],
    yAxis: [
      { ...axis, gridIndex: 0, name: "事件 / 起", nameTextStyle: { color: skin.c.ink2 } },
      {
        ...axis,
        gridIndex: 1,
        name: "消息 / 条",
        nameTextStyle: { color: skin.c.ink2 },
        splitNumber: 2,
      },
    ],
    series: [
      {
        name: "事件量",
        type: "line",
        data: known(dailyCounts(events, days)),
        smooth: false,
        showSymbol: true,
        symbolSize: 6,
        itemStyle: { color: skin.c.accent },
        lineStyle: { width: 2 },
      },
      {
        name: "无响应",
        type: "line",
        data: known(dailyCounts(events, days, isUnreplied)),
        smooth: false,
        symbolSize: 5,
        itemStyle: { color: skin.c.crit },
        lineStyle: { width: 2, type: "dashed" },
      },
      {
        name: "消息量",
        type: "bar",
        xAxisIndex: 1,
        yAxisIndex: 1,
        data: msgs.map((m) => (m.cells ? m.msgs : null)),
        barMaxWidth: 28,
        itemStyle: { color: skin.c.barSoft, borderRadius: [3, 3, 0, 0] },
      },
    ],
  };
}

export function OverviewTrend({ analytics, api }: OverviewProps) {
  const [hourly, setHourly] = useState(false);
  const msgs = msgRollup(analytics, api).byDay;
  const option = buildOverviewTrend(analytics, msgs, hourly);
  return (
    <>
      <div className="od-chart-toolbar">
        <div className="od-modes" role="group" aria-label="趋势维度">
          <button type="button" aria-pressed={!hourly} onClick={() => setHourly(false)}>
            每日趋势
          </button>
          <button type="button" aria-pressed={hourly} onClick={() => setHourly(true)}>
            事件到达节奏
          </button>
        </div>
        <div className="od-chart-legend">
          {hourly ? (
            <>
              <span>
                <i data-tone="warn" />
                首响超时
              </span>
              <span>
                <i data-tone="muted" />
                其他事件
              </span>
            </>
          ) : (
            <>
              <span>
                <i />
                事件
              </span>
              <span>
                <i data-tone="risk" />
                无响应
              </span>
              <span>
                <i data-tone="muted" />
                消息
              </span>
            </>
          )}
        </div>
      </div>
      <EChart
        option={option}
        height={338}
        ariaLabel={
          hourly
            ? "事件到达节奏：按 UTC+8 小时统计事件量，标出其中首响超时的事件"
            : "每日业务量：上图为事件与无响应起数，下图为消息条数，各自从零起算；红色日期有抽取失败"
        }
        onEvent={
          hourly
            ? undefined
            : {
                type: "click",
                handler: (params: unknown) => {
                  const day = (params as { name?: string }).name;
                  if (day && analytics.days.includes(day))
                    api.go("/detail", { from: day, to: day });
                },
              }
        }
      />
      <p className="od-footnote">
        {hourly
          ? "按首条消息时间归入小时 · UTC+8 · 首响超时含无响应"
          : analytics.cov.failed
            ? "红色日期含抽取失败，事件量为已知部分；消息量不受抽取影响。"
            : "事件按发生日期归属；消息量按群日记录汇总。"}
        {!hourly && msgs.some((m) => !m.cells) ? " 缺少记录的日期留空，不按零计。" : null}
      </p>
    </>
  );
}

type CategoryRow = ReturnType<typeof categoryRollup>[number];
const CATEGORY_COLORS = [
  skin.c.accent,
  skin.c.good,
  skin.c.s2,
  skin.c.ink2,
  skin.c.warn,
  skin.c.barSoft,
];

export function categorySlices(rows: CategoryRow[]) {
  const head = rows.slice(0, 5).map((r, i) => ({
    key: r.key,
    label: r.label,
    count: r.count,
    share: r.share,
    color: CATEGORY_COLORS[i]!,
  }));
  const rest = rows.slice(5);
  return rest.length
    ? [
        ...head,
        {
          key: null,
          label: `其他 ${rest.length} 类`,
          count: rest.reduce((s, r) => s + r.count, 0),
          share: rest.reduce((s, r) => s + r.share, 0),
          color: CATEGORY_COLORS[5]!,
        },
      ]
    : head;
}

export function buildCategoryPie(rows: CategoryRow[]): EChartsOption {
  return {
    animation: false,
    backgroundColor: "transparent",
    tooltip: {
      ...chartBase(skin).tooltip,
      trigger: "item",
      confine: true,
      formatter: "{b}<br/>{c} 起 · {d}%",
    },
    series: [
      {
        name: "事件类型",
        type: "pie",
        radius: "84%",
        center: ["50%", "50%"],
        stillShowZeroSum: false,
        label: { show: false },
        labelLine: { show: false },
        emphasis: { scale: false },
        itemStyle: { borderColor: skin.c.surface, borderWidth: 2 },
        data: categorySlices(rows).map((s) => ({
          name: s.label,
          value: s.count,
          itemStyle: { color: s.color },
        })),
      },
    ],
  };
}

export function CategoryPie({ rows, api }: { rows: CategoryRow[]; api: OverviewProps["api"] }) {
  const [expanded, setExpanded] = useState(false);
  const slices = categorySlices(rows);
  return (
    <div className="od-categories">
      <EChart
        option={buildCategoryPie(rows)}
        height={196}
        ariaLabel={`事件类型分布饼图：${slices.map((s) => `${s.label} ${s.count} 起`).join("，")}`}
        onEvent={{
          type: "click",
          handler: (params: unknown) => {
            const i = (params as { dataIndex?: number }).dataIndex;
            const slice = i === undefined ? undefined : slices[i];
            if (!slice) return;
            if (slice.key === null) setExpanded(true);
            else api.go("/events", { level1: slice.key, level2: null });
          },
        }}
      />
      <div className="od-pie-legend">
        {slices.map((s) => {
          const content = (
            <>
              <i style={{ background: s.color }} />
              <span>
                {s.label}
                {s.key === null ? (
                  <DownOutlined rotate={expanded ? 180 : 0} style={{ marginLeft: 8 }} />
                ) : null}
              </span>
              <b className="od-category-count">{s.count}</b>
              <small>{formatPercent(s.share) ?? "—"}</small>
            </>
          );
          return s.key === null ? (
            <button
              key="other"
              type="button"
              className="od-pie-row"
              aria-expanded={expanded}
              onClick={() => setExpanded(!expanded)}
            >
              {content}
            </button>
          ) : (
            <Link
              key={s.key}
              className="od-pie-row"
              to={api.hrefWith({ level1: s.key, level2: null }, "/events")}
            >
              {content}
            </Link>
          );
        })}
        {expanded ? (
          <div className="od-pie-other">
            {rows.slice(5).map((r) => (
              <Link
                className="od-pie-row"
                key={r.key}
                to={api.hrefWith({ level1: r.key, level2: null }, "/events")}
              >
                <span />
                <span>{r.label}</span>
                <b>{r.count}</b>
                <small>{formatPercent(r.share) ?? "—"}</small>
              </Link>
            ))}
          </div>
        ) : null}
      </div>
    </div>
  );
}

const RESPONSE_BINS = [
  { min: 0, max: 60, label: "0–1 分" },
  { min: 60, max: 300, label: "1–5 分" },
  { min: 300, max: 900, label: "5–15 分" },
  { min: 900, max: 1800, label: "15–30 分" },
  { min: 1800, max: 3600, label: "30–60 分" },
  { min: 3600, max: 7200, label: "1–2 小时" },
  { min: 7200, max: 14400, label: "2–4 小时" },
  { min: 14400, max: Infinity, label: ">4 小时" },
];

export function ResponseDistribution({
  events,
  slaSec,
}: {
  events: readonly DecoratedEvent[];
  slaSec: number;
}) {
  const replied = events.filter((e) => isMerchant(e) && e.firstReplySec !== null);
  const bins = RESPONSE_BINS.map((bin) => ({
    ...bin,
    n: replied.filter(
      (e) =>
        e.firstReplySec !== null &&
        (bin.min === 0 ? e.firstReplySec >= 0 : e.firstReplySec > bin.min) &&
        e.firstReplySec <= bin.max,
    ).length,
  }));
  const max = Math.max(1, ...bins.map((b) => b.n));
  return (
    <div className="od-distribution" aria-label="已回复商家事件的首响时长分布">
      {bins.map((b) => (
        <div className="od-bin" key={b.label} data-tone={b.min >= slaSec ? "warn" : undefined}>
          <span>{b.label}</span>
          <span className="od-bin-track">
            <i style={{ width: `${(b.n / max) * 100}%` }} />
          </span>
          <b className="od-bin-count">{b.n}</b>
        </div>
      ))}
      {!replied.length ? <p className="od-footnote">暂无已回复的商家事件</p> : null}
    </div>
  );
}
