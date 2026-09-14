import { InfoCircleOutlined } from "@ant-design/icons";
import { Select, Tooltip } from "antd";
import { useId, type ComponentProps, type ReactNode } from "react";
import { METRIC, SLA_OPTIONS } from "@/domain/definitions";
import { coverageLabel } from "@/domain/metrics";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { RoomFilters } from "@/features/rooms/RoomFilters";
import { Metric, MetricInfo as InsightInfo } from "@/components/Metric";
import "./insights.css";

type InsightMetric = ComponentProps<typeof Metric> & { key: string };

export function InsightsLayout({
  title,
  subtitle,
  analytics,
  api,
  children,
}: {
  title: string;
  subtitle: string;
  analytics: Analytics;
  api: FiltersApi;
  children: ReactNode;
}) {
  const { days } = analytics;
  const id = useId();
  return (
    <main className="od-overview ia-workbench">
      <header className="od-header">
        <div>
          <h1>{title}</h1>
          <p>{subtitle}</p>
        </div>
        <section className="ra-metric-scope" aria-label="指标口径">
          <span className="ra-scope-label">指标口径</span>
          <label htmlFor={id}>首响阈值</label>
          <Select
            id={id}
            aria-label="首响阈值"
            value={api.filters.slaSec}
            options={SLA_OPTIONS.map((option) => ({ value: option.value, label: option.label }))}
            onChange={(value: number) => api.patch({ slaSec: value })}
          />
          <InsightInfo label="首响阈值口径" text={METRIC.overdue} />
          <span>{days.length} 天</span>
          <Tooltip title={METRIC.timezone} mouseEnterDelay={0.8} trigger={["hover", "focus"]}>
            <button type="button" className="ra-timezone">
              UTC+8
            </button>
          </Tooltip>
        </section>
      </header>
      <RoomFilters meta={analytics.dataset.meta} api={api} label={`${title}筛选条件`} />
      {/* 条件是整个 coverage，不只是打标 —— 抽取失败、缺记录、结果未知都必须出现在
          顶部。文案全部由 coverageLabel 拼，这里不再补写死的后缀：只有抽取失败时
          「分类统计未完成」是假的。 */}
      {analytics.cov.complete ? null : (
        <p className="od-footnote" role="status">
          <InfoCircleOutlined /> {coverageLabel(analytics.cov)}
        </p>
      )}
      {children}
    </main>
  );
}

export function InsightMetrics({
  items,
  className = "",
  unavailable = false,
}: {
  items: InsightMetric[];
  className?: string;
  unavailable?: boolean;
}) {
  return (
    <section
      className={`od-metrics ia-metrics ${className}`}
      aria-label="关键指标"
      data-count={items.length}
    >
      {items.map(({ key, ...item }) => (
        <Metric key={key} {...item} unavailable={item.unavailable ?? unavailable} />
      ))}
    </section>
  );
}

export function InsightSection({
  title,
  subtitle,
  info,
  extra,
  footer,
  children,
}: {
  title: string;
  subtitle?: ReactNode;
  info?: string;
  extra?: ReactNode;
  footer?: ReactNode;
  children: ReactNode;
}) {
  const id = useId();
  return (
    <section className="ia-section" aria-labelledby={id}>
      <div className="od-section-head">
        <div>
          <h2 id={id}>
            {title}
            {info ? <InsightInfo label={`${title}口径`} text={info} /> : null}
          </h2>
          {subtitle ? <div className="ia-section-subtitle">{subtitle}</div> : null}
        </div>
        {extra}
      </div>
      {children}
      {footer ? <p className="od-footnote">{footer}</p> : null}
    </section>
  );
}
