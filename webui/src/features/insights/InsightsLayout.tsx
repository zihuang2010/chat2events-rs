import { ArrowRightOutlined, InfoCircleOutlined } from "@ant-design/icons";
import { Select, Tooltip } from "antd";
import { useId, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { METRIC, SLA_OPTIONS } from "@/domain/definitions";
import { coverageLabel } from "@/domain/metrics";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { RoomFilters } from "@/features/rooms/RoomFilters";
import "../rooms/rooms.css";
import "./insights.css";

interface InsightMetric {
  key: string;
  label: string;
  value: ReactNode;
  unit?: string | undefined;
  note?: ReactNode | undefined;
  info?: string | undefined;
  tone?: "bad" | "mid" | undefined;
  to?: string | undefined;
  unavailable?: boolean;
}

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
    <main className="od-overview od-room-analysis ia-workbench">
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
      {analytics.cov.pendingLabels || analytics.cov.failedLabels ? (
        <p className="od-footnote" role="status">
          <InfoCircleOutlined /> {coverageLabel(analytics.cov)} · 分类统计未完成
        </p>
      ) : null}
      {children}
    </main>
  );
}

export function InsightInfo({ label, text }: { label: string; text: string }) {
  return (
    <Tooltip title={text} mouseEnterDelay={0.8} trigger={["hover", "focus"]}>
      <button type="button" className="od-info" aria-label={label}>
        <InfoCircleOutlined aria-hidden="true" />
      </button>
    </Tooltip>
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
      {items.map((item) => {
        const isUnavailable = item.unavailable ?? unavailable;
        return (
          <div
            key={item.key}
            className="od-metric"
            data-tone={item.tone === "bad" ? "risk" : item.tone === "mid" ? "warn" : undefined}
          >
            <div className="od-metric-label">
              {item.label}
              {item.info ? <InsightInfo label={`${item.label}口径`} text={item.info} /> : null}
            </div>
            {item.to && !isUnavailable ? (
              <Link className="od-metric-value" to={item.to}>
                {item.value}
                <small>{item.unit}</small>
                <ArrowRightOutlined className="od-metric-arrow" />
              </Link>
            ) : (
              <div className="od-metric-value">
                {isUnavailable ? "—" : item.value}
                <small>{item.unit}</small>
              </div>
            )}
            <p>{isUnavailable ? "当前范围事件统计暂缺" : item.note}</p>
          </div>
        );
      })}
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
