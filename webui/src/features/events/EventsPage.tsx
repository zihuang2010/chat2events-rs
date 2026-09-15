/**
 * 一个分类层级驱动构成、响应对照与每日趋势；下钻保持当前事件集合的边界。
 */
import { MetricInfo as InsightInfo } from "@/components/Metric";
import { ArrowRightOutlined, LineChartOutlined, TableOutlined } from "@ant-design/icons";
import { Segmented, Table, Tabs } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { METRIC } from "@/domain/definitions";
import { categoryRows, type CategoryRow } from "@/domain/metrics";
import { useCategories, useSummary } from "@/api/queries";
import { ErrorState, PageSkeleton } from "@/components/states";
import { InsightsLayout, InsightMetrics } from "@/features/insights/InsightsLayout";
import { DurationOrNull, PercentOrNull } from "@/components/primitives";
import { EmptyState } from "@/components/states";
import { EventTrends } from "./EventTrends";
import { formatInt, formatPercent } from "@/lib/format";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi, FilterPatch } from "@/features/filters/useFilters";
import { msgRollup } from "@/features/overview/overviewMetrics";
import "./events.css";

export function EventsPage({ analytics, api }: { analytics: Analytics; api: FiltersApi }) {
  const { cov, taxIndex, dataset, parents } = analytics;
  const { filters, hrefWith, reset } = api;
  const source = dataset.source;
  const messages = msgRollup(analytics);
  const [level, setLevel] = useState<"level1" | "level2">("level1");
  const summary = useSummary(source, analytics.q);
  // ⚠️ **一级和二级是两次请求，不能由一次拆出来** —— 分位数不可加，
  // 一级的 P50 只能由数据库按父类现算（后端 `read_categories` 的 `groups`）。
  const level1 = useCategories(
    source,
    analytics.q,
    useMemo(() => parents.map((p) => p.types), [parents]),
  );
  const level2 = useCategories(source, analytics.q);
  const total = summary.data?.events ?? 0;
  const rows = useMemo(
    () =>
      level === "level1"
        ? categoryRows(level1.data ?? [], "level1", taxIndex, parents, total)
        : categoryRows(level2.data ?? [], "level2", taxIndex, parents, total),
    [level, level1.data, level2.data, taxIndex, parents, total],
  );
  const withoutTaxonomy = dataset.meta.taxonomy_version === "v0";
  const unclassifiedInfo = withoutTaxonomy
    ? "尚未建立词表，分类暂不可用；事件总量与首响指标仍可用。"
    : METRIC.unclassified;
  // 「未归类」是一个真实的一级分类（`buildTaxonomyIndex` 造的 `__untyped__` 落在它下面），
  // 所以直接读一级汇总那一行，不再自己数一遍。
  const unclassified = useMemo(
    () =>
      categoryRows(level1.data ?? [], "level1", taxIndex, parents, total).find(
        (row) => row.label === "未归类",
      )?.count ?? 0,
    [level1.data, taxIndex, parents, total],
  );
  const unrepliedStatus = filters.status === "backlog" ? "backlog" : "unreplied";

  const categoryPatch = (row: CategoryRow): FilterPatch =>
    level === "level1"
      ? // 一级汇总仍受已选二级约束，不能在下钻时扩大成整个一级。
        { level1: row.key }
      : { level1: null, level2: row.key };
  const categoryHref = (row: CategoryRow, onlyUnreplied = false) =>
    hrefWith(
      {
        ...categoryPatch(row),
        focusAgent: null,
        ...(onlyUnreplied ? { status: unrepliedStatus } : {}),
      },
      "/detail",
    );

  const columns: ColumnsType<CategoryRow> = [
    {
      title: level === "level1" ? "一级分类" : "二级分类",
      key: "label",
      fixed: "left",
      width: 170,
      sorter: (a, b) => a.label.localeCompare(b.label, "zh"),
      render: (_, row) => (
        <>
          <Link className="ev-category-link" title={row.label} to={categoryHref(row)}>
            {row.label}
          </Link>
          {row.parent ? <span className="c2e-sub">{row.parent}</span> : null}
        </>
      ),
    },
    {
      title: "事件构成",
      children: [
        {
          title: "事件总数",
          dataIndex: "count",
          key: "count",
          align: "right",
          width: 100,
          defaultSortOrder: "descend",
          sorter: (a, b) => a.count - b.count,
          render: (value: number) => <b>{formatInt(value)}</b>,
        },
        {
          title: (
            <span className="ev-column-title">
              事件占比{" "}
              <InsightInfo
                label="事件占比口径"
                text="该分类事件数 / 当前筛选范围事件总数。只按主分类计数；打标未完成的事件暂不归入分类，占比合计可能不足 100%。"
              />
            </span>
          ),
          dataIndex: "share",
          key: "share",
          align: "right",
          width: 155,
          sorter: (a, b) => a.share - b.share,
          render: (value: number) => (
            <div className="ev-share">
              <span>{formatPercent(value)}</span>
              <span className="ev-share-track" aria-hidden="true">
                <i style={{ width: value * 100 + "%" }} />
              </span>
            </div>
          ),
        },
        {
          title: (
            <span className="ev-column-title">
              商家发起 <InsightInfo label="商家发起口径" text={METRIC.merchant} />
            </span>
          ),
          dataIndex: "merchant",
          key: "merchant",
          align: "right",
          width: 110,
          sorter: (a, b) => a.merchant - b.merchant,
          render: (value: number) => formatInt(value),
        },
      ],
    },
    {
      title: (
        <span className="ev-column-title">
          商家首响 <InsightInfo label="商家首响口径" text={METRIC.p50} />
        </span>
      ),
      children: [
        {
          title: "已回复样本",
          key: "samples",
          align: "right",
          width: 110,
          sorter: (a, b) => a.merchant - a.unreplied - (b.merchant - b.unreplied),
          render: (_, row) => formatInt(row.merchant - row.unreplied),
        },
        {
          title: "P50 · 中位",
          dataIndex: "p50",
          key: "p50",
          align: "right",
          width: 125,
          sorter: (a, b) => (a.p50 ?? Infinity) - (b.p50 ?? Infinity),
          render: (value: number | null) => <DurationOrNull value={value} />,
        },
        {
          title: "P90 · 长尾",
          dataIndex: "p90",
          key: "p90",
          align: "right",
          width: 145,
          sorter: (a, b) => (a.p90 ?? Infinity) - (b.p90 ?? Infinity),
          render: (value: number | null) => <DurationOrNull value={value} />,
        },
      ],
    },
    {
      title: (
        <span className="ev-column-title">
          无响应 <InsightInfo label="分类无响应口径" text={METRIC.unreplied} />
        </span>
      ),
      children: [
        {
          title: "无响应事件数",
          dataIndex: "unreplied",
          key: "unreplied",
          align: "right",
          width: 125,
          sorter: (a, b) => a.unreplied - b.unreplied,
          render: (value: number, row) =>
            value ? (
              <Link
                className="ev-risk-link"
                aria-label={row.label + "无响应 " + value + " 起"}
                to={categoryHref(row, true)}
              >
                {formatInt(value)} <ArrowRightOutlined aria-hidden="true" />
              </Link>
            ) : (
              <span className="ev-muted">0</span>
            ),
        },
        {
          title: "无响应率",
          dataIndex: "unrepliedRate",
          key: "unrepliedRate",
          align: "right",
          width: 130,
          sorter: (a, b) => (a.unrepliedRate ?? -1) - (b.unrepliedRate ?? -1),
          render: (value: number | null, row) => (
            <>
              <b className={row.unreplied ? "ev-risk" : undefined}>
                <PercentOrNull value={value} />
              </b>
              <span className="c2e-sub">
                {row.unreplied} / {row.merchant} 起商家事件
              </span>
            </>
          ),
        },
      ],
    },
  ];

  const failed = [summary, level1, level2].find((query) => query.isError);
  if (failed?.error)
    return <ErrorState error={failed.error} onRetry={() => void failed.refetch()} />;
  if (!summary.data) return <PageSkeleton />;
  const agg = summary.data;

  return (
    <InsightsLayout
      title="事件洞察"
      subtitle="事件构成、分类响应与每日变化"
      analytics={analytics}
      api={api}
    >
      <InsightMetrics
        unavailable={cov.known === 0}
        items={[
          {
            key: "rooms",
            label: "活跃群",
            value: formatInt(agg.rooms),
            unit: "个",
            info: METRIC.rooms,
            note: "当前事件涉及的群 · 按群去重",
          },
          {
            key: "messages",
            label: "消息总量",
            value: cov.cells ? formatInt(messages.msgs) : "—",
            unit: "条",
            info: METRIC.msgCount,
            unavailable: false,
            note:
              "仅按日期、群统计" +
              (!cov.cells ? " · 无群日记录" : cov.missing || cov.unknown ? " · 仅已知量" : ""),
          },
          {
            key: "events",
            label: "事件量",
            value: formatInt(agg.events),
            unit: "起",
            info: METRIC.events,
            note: "按事件去重",
            to: agg.events ? hrefWith({ focusAgent: null }, "/detail") : undefined,
          },
          {
            key: "merchant",
            label: "商家发起",
            value: formatInt(agg.merchant),
            unit: "起",
            info: METRIC.merchant,
            note: "另有平台发起 " + formatInt(agg.push) + " 起",
          },
          {
            key: "unreplied",
            label: "无响应事件",
            value: formatInt(agg.unreplied),
            unit: "起",
            info: METRIC.unreplied,
            tone: agg.unreplied ? "risk" : undefined,
            note: "占商家事件 " + (formatPercent(agg.unrepliedRate) ?? "—"),
            to: agg.unreplied
              ? hrefWith({ status: unrepliedStatus, focusAgent: null }, "/detail")
              : undefined,
          },
          {
            key: "unclassified",
            label: withoutTaxonomy ? "未建词表事件" : "未归类事件",
            value: formatInt(unclassified),
            unit: "起",
            info: unclassifiedInfo,
            note:
              "占全部事件 " + (formatPercent(agg.events ? unclassified / agg.events : null) ?? "—"),
            to: unclassified
              ? hrefWith({ level1: "未归类", focusAgent: null }, "/detail")
              : undefined,
          },
        ]}
      />
      {agg.events === 0 ? (
        <EmptyState
          title="当前范围内没有事件"
          description="当前筛选条件下没有事件。抽取失败不代表业务量为零。"
          onReset={reset}
        />
      ) : (
        <section
          className="ev-analysis ia-section ia-tabbed-section"
          aria-labelledby="ev-analysis-title"
        >
          <Tabs
            animated={false}
            defaultActiveKey="comparison"
            renderTabBar={(props, DefaultTabBar) => (
              <div className="ia-tabs-toolbar">
                <h2 className="ia-tabs-heading" id="ev-analysis-title">
                  分类分析
                </h2>
                <DefaultTabBar {...props} />
                <Segmented
                  className="ia-tabs-extra"
                  aria-label="分类分析层级"
                  value={level}
                  options={[
                    { label: "一级分类", value: "level1" },
                    { label: "二级分类", value: "level2" },
                  ]}
                  onChange={setLevel}
                />
              </div>
            )}
            items={[
              {
                key: "comparison",
                label: "构成与响应",
                icon: <TableOutlined aria-hidden="true" />,
                children: (
                  <Table<CategoryRow>
                    key={level}
                    className="ev-table"
                    rowKey="key"
                    size="small"
                    columns={columns}
                    dataSource={rows}
                    pagination={{ pageSize: 20, hideOnSinglePage: true, showSizeChanger: false }}
                    scroll={{ x: 1245 }}
                  />
                ),
              },
              {
                key: "trends",
                label: "每日趋势",
                icon: <LineChartOutlined aria-hidden="true" />,
                children: (
                  <EventTrends rows={rows} analytics={analytics} categoryHref={categoryHref} />
                ),
              },
            ]}
          />
          <p className="od-footnote">
            事件占比以当前事件总数为分母；无响应率以该分类的商家事件数为分母。平台发起事件不进入首响与无响应指标。
          </p>
        </section>
      )}
      <details className="ev-definitions">
        <summary>统计口径与数据边界</summary>
        <dl>
          <div>
            <dt>分类与占比</dt>
            <dd>{METRIC.primaryOnly} 事件占比的分母为当前筛选范围事件总数。</dd>
          </div>
          <div>
            <dt>首响与样本</dt>
            <dd>{METRIC.p50} P90 表示 90 分位时长；零样本显示为「—」。</dd>
          </div>
          <div>
            <dt>无响应</dt>
            <dd>{METRIC.unreplied} 无响应率为分类内无响应事件数 / 商家发起事件数。</dd>
          </div>
          <div>
            <dt>{withoutTaxonomy ? "未建词表" : "未归类"}</dt>
            <dd>{unclassifiedInfo}</dd>
          </div>
          <div>
            <dt>每日趋势</dt>
            <dd>
              事件按开始日归属，各分类使用独立的零起点刻度；当前范围内存在抽取失败的日期保留缺口。
              {METRIC.coverage}
            </dd>
          </div>
        </dl>
      </details>
    </InsightsLayout>
  );
}
