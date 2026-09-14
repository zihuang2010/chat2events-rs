/**
 * 客服效能：**工作量与服务质量并排，绝不用单一数字排名。**
 *
 * 两个口径必须同时出现，因为互相不能替代：
 *   活跃量     从 agents[] 现算，多人协作各自计入，相加会大于事件总数
 *   首响归属事件数 agent_metric_daily 的生产口径，无响应的事件不落在任何人头上
 * 两者都不是解决量 —— 库里根本没有解决量。
 */

import { MetricInfo as InsightInfo } from "@/components/Metric";
import { ArrowRightOutlined, DotChartOutlined, TableOutlined } from "@ant-design/icons";
import { Alert, Button, Drawer, Table, Tabs, Tag } from "antd";
import { Link } from "react-router-dom";
import type { ColumnsType } from "antd/es/table";
import { useMemo } from "react";
import { METRIC } from "@/domain/definitions";
import { agentRollup, type AgentRow } from "@/domain/metrics";
import { useAgentAggs, useRoomAggs, useSummary } from "@/api/queries";
import { ErrorState, PageSkeleton } from "@/components/states";
import { InsightsLayout, InsightMetrics, InsightSection } from "@/features/insights/InsightsLayout";
import { DurationOrNull, PercentOrNull } from "@/components/primitives";
import { EmptyState } from "@/components/states";
import { TrendChart } from "@/components/charts/TrendChart";
import { WorkloadQualityChart, type WorkloadPoint } from "@/components/charts/WorkloadQualityChart";
import { formatDuration, formatInt, formatPercent, shortId } from "@/lib/format";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { WORKBENCH_THEME, cssVars } from "@/app/theme/workbench";
import "./agents.css";

export function AgentsPage({ analytics, api }: { analytics: Analytics; api: FiltersApi }) {
  const { days, dayset, roomLabel, agentLabel, slaSec, aliasIsAuthoritative, dataset } = analytics;
  const { filters, patch, go, hrefWith, reset } = api;
  const unavailable = analytics.cov.known === 0;
  const source = dataset.source;
  const summary = useSummary(source, analytics.q);
  const aggs = useAgentAggs(source, analytics.q);

  const rows = useMemo(
    () =>
      agentRollup({
        aggs: aggs.data ?? [],
        groupDaily: dataset.groupDaily,
        days,
        dayset,
        labelOf: agentLabel,
        // 事件已按摘要、群与客服统一下推筛选，不能再把关键词缩窄为仅姓名。
        query: "",
      }).filter((row) => !filters.agent || row.key === filters.agent),
    [aggs.data, dataset, days, dayset, agentLabel, filters.agent],
  );

  const pageSize = Math.min(200, filters.pageSize);
  const currentPage = Math.min(filters.page, Math.max(1, Math.ceil(rows.length / pageSize)));

  const points: WorkloadPoint[] = useMemo(
    () =>
      rows
        .filter((r) => r.p50 !== null)
        .map((r) => ({
          key: r.key,
          label: r.label,
          involved: r.involved,
          p50Sec: r.p50 as number,
          rooms: r.rooms,
          owned: r.owned,
          overdueRate: r.overdueRate,
        })),
    [rows],
  );

  const focus = filters.focusAgent ? rows.find((r) => r.key === filters.focusAgent) : undefined;
  // 抽屉里的分群明细：**两个口径分别问**（参与过 vs 首响归属），对应后端 `Filters`
  // 的 `agent` 与 `responder`。合成一条 SQL 会把两个口径混起来，那是静默改数。
  // 没展开抽屉时把数据源置空 —— hook 的 `enabled` 就是它，于是一个请求都不发。
  const drawerSource = filters.focusAgent ? source : undefined;
  const involvedByRoom = useRoomAggs(drawerSource, {
    ...analytics.q,
    agent: filters.focusAgent,
  });
  const ownedByRoom = useRoomAggs(drawerSource, {
    ...analytics.q,
    agent: null,
    responder: filters.focusAgent,
  });
  const focusRooms = useMemo(() => {
    const owned = new Map((ownedByRoom.data ?? []).map((room) => [room.roomid, room]));
    return (involvedByRoom.data ?? [])
      .map((room) => {
        const mine = owned.get(room.roomid);
        return {
          roomId: room.roomid,
          involved: room.events,
          owned: mine?.events ?? 0,
          // 首响归属的事件必然已回复（未回复的没有 `first_responder`），
          // 所以有效样本就是其中的商家事件数，不用再减一次未回复。
          replySamples: mine?.merchant ?? 0,
          p50: mine?.p50 ?? null,
        };
      })
      .sort((a, b) => b.involved - a.involved);
  }, [involvedByRoom.data, ownedByRoom.data]);
  const failed = [summary, aggs].find((query) => query.isError);
  if (failed?.error)
    return <ErrorState error={failed.error} onRetry={() => void failed.refetch()} />;
  if (!summary.data || !aggs.data) return <PageSkeleton />;
  const agg = summary.data;

  const involvedTotal = rows.reduce((sum, row) => sum + row.involved, 0);
  const maxInvolved = Math.max(1, ...rows.map((row) => row.involved));
  const closeFocus = () => patch({ focusAgent: null, page: currentPage });
  const responseParts = [
    {
      key: "ontime",
      label: "按时回复",
      count: agg.merchant - agg.overdue,
      tone: "good" as const,
      note: `首响 ≤ ${formatDuration(slaSec)}`,
      status: "replied" as const,
      overdueOnly: false,
    },
    {
      key: "late",
      label: "超时回复",
      count: agg.overdue - agg.unreplied,
      tone: "warn" as const,
      note: `首响 > ${formatDuration(slaSec)}`,
      status: "replied" as const,
      overdueOnly: true,
    },
    {
      key: "unreplied",
      label: "无响应",
      count: agg.unreplied,
      tone: "risk" as const,
      note: `占商家事件 ${formatPercent(agg.unrepliedRate) ?? "—"}`,
      status: "unreplied" as const,
      overdueOnly: null,
    },
  ];
  // 响应构成是当前事件集合的子集，下钻必须保留已选状态和超时条件。
  const responseHref = (status: "replied" | "unreplied" | "push", overdueOnly: boolean | null) =>
    hrefWith(
      {
        focusAgent: null,
        status: status === "unreplied" && filters.status === "backlog" ? "backlog" : status,
        overdueOnly: overdueOnly ?? filters.overdueOnly,
      },
      "/detail",
    );

  const columns: ColumnsType<AgentRow> = [
    {
      title: "客服",
      dataIndex: "label",
      key: "label",
      fixed: "left",
      width: 160,
      sorter: (a, b) => a.label.localeCompare(b.label, "zh"),
      render: (_, r) => (
        <>
          <button
            type="button"
            className="ia-table-link"
            title={r.label}
            aria-haspopup="dialog"
            onClick={(event) => {
              event.stopPropagation();
              patch({ focusAgent: r.key, page: currentPage });
            }}
          >
            {r.label}
          </button>
          {/* 只在 label 真被别名替换过时才标 —— 没有账号映射的人 label 就是
              easyUserId 本身，那时挂个「账号」标签是在说谎。 */}
          {aliasIsAuthoritative || r.label === r.key ? null : (
            <>
              {" "}
              <span className="ag-alias">账号</span>
            </>
          )}
          <span className="c2e-sub">{shortId(r.key)}</span>
        </>
      ),
    },
    {
      title: "参与工作量",
      children: [
        {
          title: "活跃群",
          dataIndex: "rooms",
          key: "rooms",
          align: "right",
          width: 88,
          sorter: (a, b) => a.rooms - b.rooms,
        },
        {
          title: (
            <span className="ag-column-title">
              参与事件 <InsightInfo label="参与事件口径" text={METRIC.involved} />
            </span>
          ),
          dataIndex: "involved",
          key: "involved",
          align: "right",
          width: 136,
          defaultSortOrder: "descend",
          sorter: (a, b) => a.involved - b.involved,
          render: (v: number) => (
            <div className="ag-workload">
              <b>{formatInt(v)}</b>
              <span aria-hidden="true">
                <i style={{ width: `${(v / maxInvolved) * 100}%` }} />
              </span>
            </div>
          ),
        },
      ],
    },
    {
      title: (
        <span className="ag-column-title">
          本人首响 <InsightInfo label="本人首响口径" text={METRIC.agentReply} />
        </span>
      ),
      children: [
        {
          title: (
            <span className="ag-column-title">
              归属事件 <InsightInfo label="归属事件口径" text={METRIC.owned} />
            </span>
          ),
          dataIndex: "owned",
          key: "owned",
          align: "right",
          width: 120,
          sorter: (a, b) => a.owned - b.owned,
          render: (v: number, r) => (
            <>
              <b>{formatInt(v)}</b>
              <span className="c2e-sub">商家 {formatInt(r.merchantOwned)}</span>
            </>
          ),
        },
        {
          title: "有效样本",
          dataIndex: "replySamples",
          key: "replySamples",
          align: "right",
          width: 96,
          sorter: (a, b) => a.replySamples - b.replySamples,
          render: (v: number) => formatInt(v),
        },
        {
          title: "P50 · 中位",
          dataIndex: "p50",
          key: "p50",
          align: "right",
          width: 112,
          sorter: (a, b) => (a.p50 ?? Infinity) - (b.p50 ?? Infinity),
          render: (v: number | null) => <DurationOrNull value={v} />,
        },
        {
          title: "P90 · 长尾",
          dataIndex: "p90",
          key: "p90",
          align: "right",
          width: 132,
          sorter: (a, b) => (a.p90 ?? Infinity) - (b.p90 ?? Infinity),
          render: (v: number | null) => (
            <span className={v !== null && v > slaSec ? "ag-warn" : undefined}>
              <DurationOrNull value={v} />
            </span>
          ),
        },
        {
          title: (
            <span className="ag-column-title">
              超时率 <InsightInfo label="个人超时率口径" text={METRIC.agentOverdue} />
            </span>
          ),
          dataIndex: "overdueRate",
          key: "overdueRate",
          align: "right",
          width: 112,
          sorter: (a, b) => (a.overdueRate ?? -1) - (b.overdueRate ?? -1),
          render: (v: number | null, r) => (
            <>
              <b className={r.overdue ? "ag-warn" : undefined}>
                <PercentOrNull value={v} />
              </b>
              <span className="c2e-sub">
                {r.overdue} / {r.merchantOwned} 起
              </span>
            </>
          ),
        },
      ],
    },
    {
      title: (
        <span className="ag-column-title">
          数据完整性 <InsightInfo label="客服数据完整性口径" text={METRIC.coverage} />
        </span>
      ),
      key: "coverage",
      width: 140,
      render: (_, r) =>
        r.failedCells > 0 ? (
          <span className="ag-risk">{r.failedCells} 个群日缺失</span>
        ) : r.coverageUnknown ? (
          <span className="ag-risk">完整性未知</span>
        ) : (
          <span className="ag-muted">完整</span>
        ),
    },
  ];

  const perRoom = focusRooms;

  return (
    <InsightsLayout
      title="客服效能"
      subtitle="事件响应概况与客服个人表现"
      analytics={analytics}
      api={api}
    >
      <InsightMetrics
        unavailable={unavailable}
        className="ag-summary"
        items={[
          {
            key: "events",
            label: "事件量",
            value: formatInt(agg.events),
            unit: "起",
            info: METRIC.events,
            note: (
              <>
                <span>商家发起 {formatInt(agg.merchant)}</span> ·{" "}
                {agg.push > 0 ? (
                  <Link to={responseHref("push", null)} title="平台发起事件不计入首响时效与超时率">
                    平台发起 {formatInt(agg.push)} <ArrowRightOutlined aria-hidden="true" />
                  </Link>
                ) : (
                  <span>平台发起 0</span>
                )}
              </>
            ),
          },
          {
            key: "p50",
            label: "首响中位时长",
            value: formatDuration(agg.p50) ?? "—",
            info: METRIC.p50,
            note: `${formatInt(agg.replied)} 起商家已回复样本 · P50`,
          },
          {
            key: "overdue",
            label: "事件超时率",
            value: formatPercent(agg.overdueRate) ?? "—",
            info: METRIC.overdue,
            tone: agg.overdue ? "warn" : undefined,
            note: `${formatInt(agg.overdue)} / ${formatInt(agg.merchant)} 起商家事件 · 含无响应`,
          },
          ...responseParts.map((part) => ({
            key: part.key,
            label: part.label,
            value: formatInt(part.count),
            tone: part.count > 0 ? part.tone : undefined,
            unit: "起",
            note: part.note,
            to: part.count > 0 ? responseHref(part.status, part.overdueOnly) : undefined,
          })),
        ]}
      />
      {unavailable ? <Alert type="warning" showIcon title="当前范围事件统计暂缺" /> : null}
      {rows.length === 0 ? (
        <EmptyState
          title="没有匹配的客服"
          description="当前筛选范围内没有客服参与记录。无响应事件不归属客服；抽取失败也不会计为零。"
          onReset={reset}
        />
      ) : (
        <section
          className="ag-comparison ia-section ia-tabbed-section"
          aria-labelledby="ag-comparison-title"
        >
          <Tabs
            defaultActiveKey="table"
            animated={false}
            renderTabBar={(props, DefaultTabBar) => (
              <div className="ia-tabs-toolbar">
                <div className="ia-tabs-heading">
                  <h2 id="ag-comparison-title">客服表现对照</h2>
                  <span>{formatInt(rows.length)} 人</span>
                  <InsightInfo label="客服表现对照口径" text={METRIC.agentReply} />
                </div>
                <DefaultTabBar {...props} />
                <Link
                  className="od-link ia-tabs-extra"
                  to={hrefWith({ focusAgent: null }, "/detail")}
                >
                  事件明细 <ArrowRightOutlined aria-hidden="true" />
                </Link>
              </div>
            )}
            items={[
              {
                key: "table",
                label: "指标明细",
                icon: <TableOutlined aria-hidden="true" />,
                children: (
                  <Table<AgentRow>
                    className="ag-table"
                    size="small"
                    tableLayout="fixed"
                    rowKey="key"
                    columns={columns}
                    dataSource={rows}
                    pagination={{
                      current: currentPage,
                      pageSize,
                      showSizeChanger: true,
                      pageSizeOptions: [10, 20, 50, 100, 200],
                      showTotal: (total, range) => `${range[0]} - ${range[1]} / 共 ${total} 人`,
                      onChange: (page, size) =>
                        patch({ page: size === pageSize ? page : 1, pageSize: size }),
                    }}
                    onChange={(_, __, ___, extra) => {
                      if (extra.action === "sort") patch({ page: 1 });
                    }}
                    scroll={{ x: 1100, y: "min(560px, 60vh)" }}
                    rowClassName={(record) =>
                      record.key === filters.focusAgent ? "ant-table-row-selected" : ""
                    }
                    onRow={(record) => ({
                      className: "c2e-row-clickable",
                      onClick: () => patch({ focusAgent: record.key, page: currentPage }),
                    })}
                  />
                ),
              },
              {
                key: "chart",
                label: "工作量与时效",
                icon: <DotChartOutlined aria-hidden="true" />,
                children: (
                  <div className="ag-chart-view">
                    <div className="ag-chart-meta">
                      <span>参与事件 × 本人首响 P50</span>
                      <span>
                        {points.length} 人可比较 · {rows.length - points.length} 人无有效样本
                      </span>
                    </div>
                    {points.length ? (
                      <div className="ia-chart">
                        <WorkloadQualityChart
                          points={points}
                          onPick={(key) => patch({ focusAgent: key })}
                          height={340}
                        />
                      </div>
                    ) : (
                      <EmptyState
                        title="没有可比较的客服"
                        description="当前范围没有有效的商家首响时长样本。"
                      />
                    )}
                    <p className="ag-muted">时长越低，响应越快；气泡大小表示活跃群数。</p>
                  </div>
                ),
              },
            ]}
          />
          <p className="od-footnote">
            累计参与 {formatInt(involvedTotal)} 人次 ·
            参与事件可多人协作；首响归属包含平台事件，时效仅统计本人首响的商家事件。无响应事件不分摊至个人。
          </p>
        </section>
      )}
      <details className="ag-definitions">
        <summary>统计口径与数据边界</summary>
        <dl>
          <div>
            <dt>参与工作量</dt>
            <dd>{METRIC.involved}</dd>
          </div>
          <div>
            <dt>本人首响</dt>
            <dd>{METRIC.agentReply}</dd>
          </div>
          <div>
            <dt>个人超时率</dt>
            <dd>{METRIC.agentOverdue}</dd>
          </div>
          <div>
            <dt>数据边界</dt>
            <dd>
              客服回复消息数已入库（agent_msg_daily），本页尚未接入。
              {aliasIsAuthoritative
                ? ""
                : "客服姓名暂以平台账号 officialUserId 展示，上游没给账号的人回落到 easyUserId；尚未接入权威名册。"}
              {METRIC.coverage}
            </dd>
          </div>
        </dl>
      </details>
      <Drawer
        open={filters.focusAgent !== null}
        onClose={closeFocus}
        destroyOnHidden
        rootStyle={cssVars(WORKBENCH_THEME)}
        size="min(960px, 100vw)"
        rootClassName="ia-drawer ia-agent-drawer"
        title={focus ? `${focus.label} · 服务表现` : "客服服务表现"}
        extra={dataset.source === "mock" ? <Tag color="warning">模拟数据</Tag> : null}
      >
        {focus ? (
          <>
            <div className="od-drawer-context">
              <div>
                <strong>
                  {days[0]} 至 {days.at(-1)}
                </strong>
                <span>当前筛选范围 · UTC+8</span>
              </div>
              <Link
                className="od-link"
                to={hrefWith({ agent: focus.key, focusAgent: null }, "/detail")}
              >
                事件明细 <ArrowRightOutlined aria-hidden="true" />
              </Link>
            </div>
            <p className="od-drawer-id">
              {focus.key}
              {aliasIsAuthoritative
                ? ""
                : focus.label === focus.key
                  ? " · 上游未提供平台账号，显示 easyUserId"
                  : " · 显示名为平台账号，非权威姓名"}
            </p>
            {focus.failedCells ? (
              <Alert
                type="warning"
                showIcon
                title={`${focus.failedCells} 个群日抽取失败，事件统计不完整`}
              />
            ) : focus.coverageUnknown ? (
              <Alert type="warning" showIcon title="当前范围有失败或缺失群日，客服统计完整性未知" />
            ) : null}
            <InsightMetrics
              items={[
                {
                  key: "involved",
                  label: "参与事件",
                  value: formatInt(focus.involved),
                  unit: "起",
                  info: METRIC.involved,
                  note: `活跃群 ${focus.rooms} 个`,
                },
                {
                  key: "owned",
                  label: "首响归属",
                  value: formatInt(focus.owned),
                  unit: "起",
                  info: METRIC.owned,
                  note: `商家 ${focus.merchantOwned} 起 · 平台 ${focus.owned - focus.merchantOwned} 起`,
                },
                {
                  key: "p50",
                  label: "本人首响 P50",
                  value: formatDuration(focus.p50) ?? "—",
                  info: METRIC.agentReply,
                  note: `${focus.replySamples} 起有效样本 · P90 ${formatDuration(focus.p90) ?? "—"}`,
                },
                {
                  key: "overdue",
                  label: "本人首响超时率",
                  value: formatPercent(focus.overdueRate) ?? "—",
                  info: METRIC.agentOverdue,
                  note: `${focus.overdue} / ${focus.merchantOwned} 起 · 阈值 ${formatDuration(slaSec)}`,
                },
              ]}
            />
            <InsightSection
              title="每日变化"
              subtitle="参与事件与首响归属事件数"
              footer="按事件开始日归属；当前范围有失败或缺失群日时留空。"
            >
              <div className="ia-chart">
                <TrendChart
                  days={days}
                  series={[
                    {
                      name: "参与事件",
                      values: focus.involvedSeries,
                      area: true,
                    },
                    {
                      name: "首响归属",
                      values: focus.ownedSeries,
                    },
                  ]}
                  ariaLabel={`${focus.label} 每日活跃量与首响归属事件数`}
                  onPickDay={(day) =>
                    go("/detail", { agent: focus.key, focusAgent: null, from: day, to: day })
                  }
                />
              </div>
            </InsightSection>
            <InsightSection
              title="活跃群明细"
              subtitle={`${focus.rooms} 个群 · 当前客服参与的事件`}
            >
              <Table
                size="small"
                rowKey="roomId"
                dataSource={perRoom}
                pagination={false}
                scroll={{ x: 700 }}
                columns={[
                  {
                    title: "群",
                    dataIndex: "roomId",
                    key: "roomId",
                    width: 220,
                    render: (roomId: string) => (
                      <>
                        {roomLabel(roomId)}
                        <span className="c2e-sub">{shortId(roomId)}</span>
                      </>
                    ),
                  },
                  {
                    title: "参与事件",
                    dataIndex: "involved",
                    key: "involved",
                    align: "right",
                    width: 96,
                  },
                  {
                    title: "首响归属",
                    dataIndex: "owned",
                    key: "owned",
                    align: "right",
                    width: 96,
                  },
                  {
                    title: "有效首响样本",
                    dataIndex: "replySamples",
                    key: "replySamples",
                    align: "right",
                    width: 112,
                  },
                  {
                    title: "本人首响 P50",
                    dataIndex: "p50",
                    key: "p50",
                    align: "right",
                    width: 118,
                    render: (v: number | null) => <DurationOrNull value={v} />,
                  },
                  {
                    title: "",
                    key: "go",
                    width: 96,
                    render: (_: unknown, r) => (
                      <Button
                        size="small"
                        type="link"
                        icon={<ArrowRightOutlined />}
                        onClick={() =>
                          go("/detail", { agent: focus.key, focusAgent: null, room: r.roomId })
                        }
                      >
                        看事件
                      </Button>
                    ),
                  },
                ]}
              />
            </InsightSection>
          </>
        ) : (
          <EmptyState
            title="当前范围内没有该客服的参与记录"
            description="该客服可能不在当前筛选范围，或没有事件参与记录。"
          />
        )}
      </Drawer>
    </InsightsLayout>
  );
}
