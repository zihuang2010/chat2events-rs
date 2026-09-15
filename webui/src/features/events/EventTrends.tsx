import { ArrowRightOutlined } from "@ant-design/icons";
import { useMemo } from "react";
import { Link } from "react-router-dom";
import { WORKBENCH_THEME, chartBase } from "@/app/theme/workbench";
import { EChart, type EChartsOption } from "@/components/charts/EChart";
import type { CategoryRow } from "@/domain/metrics";
import type { Analytics } from "@/features/filters/useAnalytics";
import { formatInt, formatPercent, weekdayOf } from "@/lib/format";

export function EventTrends({
  rows,
  analytics,
  categoryHref,
}: {
  rows: CategoryRow[];
  analytics: Analytics;
  categoryHref: (row: CategoryRow) => string;
}) {
  const { days, cov } = analytics;
  // 序列由 `/api/categories` 带回来（分类 × 天，行数有界）。
  // **没有事件的天补 0，完整性存疑的天留 null** —— 两者在图上必须长得不一样。
  const trends = useMemo(
    () =>
      rows.map((row) => {
        const byDay = new Map(row.series.map((point) => [point.day, point.events]));
        return {
          row,
          values: days.map((day) => (cov.days.includes(day) ? null : (byDay.get(day) ?? 0))),
        };
      }),
    [rows, days, cov.days],
  );
  const skin = WORKBENCH_THEME;
  const base = chartBase(skin);

  return (
    <div className="ev-trends-view">
      <p className="ev-trend-context">
        <span>
          {rows.length} 个分类 · {days.length} 天
        </span>
        <span>各分类独立刻度 · 仅比较走势</span>
      </p>
      {/* ⚠️ 条件是「有没有留空的天」而不是 `cov.failed` —— 抽取失败、缺记录、最新
          结果未知都会让那一天留空（见 `coverage`），只盯 failed 的话另外两种会画出
          一张空图、一个字解释都没有。打标未完成**不**在这几种里：事件计数是抽取的
          产物，跟标签算没算完无关。 */}
      {cov.days.length ? (
        <p className="ev-trend-gap">
          抽取结果不确定的 {cov.days.length} 天留空（失败 · 无记录 ·
          最新结果未知）；区间事件量仅含已抽取记录。
        </p>
      ) : null}
      <div className="ev-trends">
        {trends.map(({ row, values }) => {
          const option: EChartsOption = {
            animation: false,
            backgroundColor: "transparent",
            textStyle: base.textStyle,
            grid: { left: 12, right: 16, top: 28, bottom: 8, containLabel: true },
            tooltip: {
              ...base.tooltip,
              trigger: "axis",
              confine: true,
              renderMode: "richText",
              formatter: (params: unknown) => {
                const [point] = params as { dataIndex: number }[];
                const day = days[point?.dataIndex ?? 0] ?? "";
                const value = values[point?.dataIndex ?? 0];
                return `${day} ${weekdayOf(day)}\n${row.label}：${value == null ? "数据不完整" : formatInt(value) + " 起"}`;
              },
            },
            xAxis: {
              type: "category",
              data: days,
              boundaryGap: days.length === 1,
              axisTick: { show: false },
              axisLine: { lineStyle: { color: skin.c.rule } },
              axisLabel: {
                formatter: (day: string) => day.slice(5),
                color: (day: string) => (cov.days.includes(day) ? skin.c.warn : skin.c.ink2),
                fontSize: 11,
                hideOverlap: true,
              },
            },
            yAxis: {
              type: "value",
              name: "事件 / 起",
              nameTextStyle: { color: skin.c.ink2, align: "left" },
              min: 0,
              minInterval: 1,
              splitNumber: 3,
              axisLabel: { color: skin.c.ink2, fontSize: 11 },
              splitLine: { lineStyle: { color: skin.c.rule, type: "dashed" } },
            },
            series: [
              {
                name: row.label,
                type: "line",
                data: values,
                connectNulls: false,
                smooth: false,
                showAllSymbol: days.length <= 14,
                symbol: "circle",
                symbolSize: 6,
                itemStyle: { color: skin.c.accent },
                lineStyle: { width: 2 },
                areaStyle: { color: skin.c.accent, opacity: 0.06 },
              },
            ],
          };
          return (
            <section key={row.key} className="ev-trend" aria-label={row.label + "每日趋势"}>
              <header>
                <div className="ev-trend-heading">
                  <Link title={row.label} to={categoryHref(row)}>
                    {row.label}
                    <ArrowRightOutlined aria-hidden="true" />
                  </Link>
                  <span className="ev-trend-parent">
                    {row.parent ?? "一级分类"} · 占比 {formatPercent(row.share)}
                  </span>
                </div>
                <div className="ev-trend-total">
                  <span>区间事件量</span>
                  <b>
                    {formatInt(row.count)}
                    <small>起</small>
                  </b>
                </div>
              </header>
              <EChart
                option={option}
                height={184}
                ariaLabel={
                  row.label +
                  "每日事件量：" +
                  values
                    .map((value, index) => days[index] + " " + (value ?? "数据不完整"))
                    .join("，")
                }
              />
              <details className="ev-trend-data">
                <summary>逐日数据</summary>
                <dl>
                  {days.map((day, index) => (
                    <div key={day}>
                      <dt>
                        {day} <span>{weekdayOf(day)}</span>
                      </dt>
                      <dd>
                        {values[index] == null ? "数据不完整" : formatInt(values[index]) + " 起"}
                      </dd>
                    </div>
                  ))}
                </dl>
              </details>
            </section>
          );
        })}
      </div>
    </div>
  );
}
