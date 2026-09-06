/**
 * 客服效能：**工作量与服务质量并排，绝不用单一数字排名。**
 *
 * 两个口径必须同时出现，因为互相不能替代：
 *   活跃量     从 agents[] 现算，多人协作各自计入，相加会大于事件总数
 *   首响归属事件数 agent_metric_daily 的生产口径，无响应的事件不落在任何人头上
 * 两者都不是解决量 —— 库里根本没有解决量。
 */

import { ArrowRightOutlined, DotChartOutlined, TableOutlined } from "@ant-design/icons";
import { Alert, Button, Drawer, Table, Tabs, Tag } from "antd";
import { Link } from "react-router-dom";
import type { ColumnsType } from "antd/es/table";
import { useMemo } from "react";
import { METRIC } from "@/domain/definitions";
import { agentRollup, isMerchant, quantile, type AgentRow } from "@/domain/metrics";
import {
  InsightsLayout,
  InsightMetrics,
  InsightSection,
  InsightInfo,
} from "@/features/insights/InsightsLayout";
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
  const {
    events,
    days,
    dayset,
    roomLabel,
    agentLabel,
    slaSec,
    aliasIsAuthoritative,
    dataset,
    agg,
  } = analytics;
  const { filters, patch, go, hrefWith, reset } = api;
  const unavailable = analytics.cov.cells === analytics.cov.failed;

  const rows = useMemo(
    () =>
      agentRollup({
        events,
        groupDaily: dataset.groupDaily,
        agents: dataset.meta.agents,
        days,
        dayset,
        slaSec,
        labelOf: agentLabel,
        // events 已按摘要、群与客服统一筛选，不能再把关键词缩窄为仅姓名。
        query: "",
      }).filter((row) => !filters.agent || row.key === filters.agent),
    [events, dataset, days, dayset, slaSec, agentLabel, filters.agent],
  );

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
  const involvedTotal = rows.reduce((sum, row) => sum + row.involved, 0);
  const maxInvolved = Math.max(1, ...rows.map((row) => row.involved));
  const closeFocus = () => patch({ focusAgent: null, page: filters.page });
  const responseParts = [
    {
      key: "ontime",
      label: "按时回复",
      count: agg.merchant - agg.overdue,
      tone: "good",
      status: "replied" as const,
      overdueOnly: false,
    },
    {
      key: "late",
      label: "超时回复",
      count: agg.overdue - agg.unreplied,
      tone: "warn",
      status: "replied" as const,
      overdueOnly: true,
    },
    {
      key: "unreplied",
      label: "无响应",
      count: agg.unreplied,
      tone: "risk",
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
      width: 180,
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
              patch({ focusAgent: r.key, page: filters.page });
            }}
          >
            {r.label}
          </button>
          {aliasIsAuthoritative ? null : (
            <>
              {" "}
              <span className="ag-alias">别名</span>
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
          title: (
            <span className="ag-column-title">
              参与事件 <InsightInfo label="参与事件口径" text={METRIC.involved} />
            </span>
          ),
          dataIndex: "involved",
          key: "involved",
          align: "right",
          width: 150,
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
        {
          title: "服务群",
          dataIndex: "rooms",
          key: "rooms",
          align: "right",
          width: 90,
          sorter: (a, b) => a.rooms - b.rooms,
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
          width: 100,
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
          width: 100,
          sorter: (a, b) => a.replySamples - b.replySamples,
          render: (v: number) => formatInt(v),
        },
        {
          title: "P50 · 中位",
          dataIndex: "p50",
          key: "p50",
          align: "right",
          width: 125,
          sorter: (a, b) => (a.p50 ?? Infinity) - (b.p50 ?? Infinity),
          render: (v: number | null) => <DurationOrNull value={v} />,
        },
        {
          title: "P90 · 长尾",
          dataIndex: "p90",
          key: "p90",
          align: "right",
          width: 145,
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
          width: 120,
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
      width: 130,
      render: (_, r) =>
        r.failedCells > 0 ? (
          <span className="ag-risk">{r.failedCells} 个群日缺失</span>
        ) : (
          <span className="ag-muted">完整</span>
        ),
    },
  ];

  const perRoom = focus
    ? focus.roomIds
        .map((roomId) => {
          const mine = events.filter((e) => e.agents.includes(focus.key) && e.roomid === roomId);
          const owned = mine.filter((e) => e.first_responder === focus.key && isMerchant(e));
          const secs = owned
            .filter((e) => e.firstReplySec !== null)
            .map((e) => e.firstReplySec as number)
            .sort((a, b) => a - b);
          return {
            roomId,
            involved: mine.length,
            owned: mine.filter((e) => e.first_responder === focus.key).length,
            replySamples: secs.length,
            p50: quantile(secs, 0.5),
          };
        })
        .sort((a, b) => b.involved - a.involved)
    : [];

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
            label: "事件总数",
            value: formatInt(agg.events),
            unit: "起",
            info: METRIC.events,
            note: `覆盖 ${agg.rooms} 个群 · ${filters.agent ? "所选客服参与 · " : ""}按事件去重`,
          },
          {
            key: "agents",
            label: "活跃客服",
            value: formatInt(rows.length),
            unit: "人",
            info: METRIC.agentsInvolved,
            note: `累计参与 ${formatInt(involvedTotal)} 人次`,
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
            tone: agg.overdue ? "mid" : undefined,
            note: `${formatInt(agg.overdue)} / ${formatInt(agg.merchant)} 起商家事件 · 含无响应`,
          },
        ]}
      />
      {unavailable ? (
        <Alert type="warning" showIcon title="当前范围事件统计暂缺" />
      ) : (
        <section className="ag-response" aria-label="事件响应构成">
          <div className="ag-response-main">
            <div className="ag-response-heading">
              <h2>
                商家发起 <b>{formatInt(agg.merchant)}</b>
                <small>起</small>
              </h2>
              <span>首响阈值 {formatDuration(slaSec)}</span>
            </div>
            <div
              className="ag-response-track"
              role="img"
              aria-label={responseParts.map((part) => `${part.label} ${part.count} 起`).join("，")}
            >
              {responseParts.map((part) => (
                <span key={part.key} data-tone={part.tone} style={{ flexGrow: part.count }} />
              ))}
            </div>
            <div className="ag-response-parts">
              {responseParts.map((part) => {
                const content = (
                  <>
                    <span>
                      <i data-tone={part.tone} />
                      {part.label}
                    </span>
                    <b>
                      {formatInt(part.count)}
                      <small>起</small>
                    </b>
                    {part.count > 0 ? <ArrowRightOutlined aria-hidden="true" /> : null}
                  </>
                );
                return part.count > 0 ? (
                  <Link
                    key={part.key}
                    className="ag-response-part"
                    to={responseHref(part.status, part.overdueOnly)}
                  >
                    {content}
                  </Link>
                ) : (
                  <div key={part.key} className="ag-response-part" data-empty="true">
                    {content}
                  </div>
                );
              })}
            </div>
          </div>
          <div className="ag-response-platform">
            <h2>
              平台发起 <b>{formatInt(agg.push)}</b>
              <small>起</small>
            </h2>
            <p>不计入首响时效与超时率</p>
            {agg.push > 0 ? (
              <Link className="od-link" to={responseHref("push", null)}>
                查看事件 <ArrowRightOutlined aria-hidden="true" />
              </Link>
            ) : (
              <span className="ag-muted">当前范围无平台事件</span>
            )}
          </div>
        </section>
      )}
      {rows.length === 0 ? (
        <EmptyState
          title="没有匹配的客服"
          description="当前筛选范围内没有客服参与记录。无响应事件不归属客服；抽取失败也不会计为零。"
          onReset={reset}
        />
      ) : (
        <>
          <div className="ag-comparison">
            <InsightSection
              title="客服表现对照"
              info={METRIC.agentReply}
              subtitle={`${rows.length} 人 · 参与工作量与本人首响分别统计`}
              extra={
                <Link className="od-link" to={hrefWith({ focusAgent: null }, "/detail")}>
                  事件明细 <ArrowRightOutlined aria-hidden="true" />
                </Link>
              }
              footer="参与事件可多人协作；首响归属包含平台事件，时效仅统计本人首响的商家事件。无响应事件不分摊至个人。"
            >
              <Tabs
                defaultActiveKey="table"
                animated={false}
                items={[
                  {
                    key: "table",
                    label: "指标明细",
                    icon: <TableOutlined aria-hidden="true" />,
                    children: (
                      <Table<AgentRow>
                        className="ag-table"
                        size="small"
                        rowKey="key"
                        columns={columns}
                        dataSource={rows}
                        pagination={{
                          pageSize: 20,
                          hideOnSinglePage: true,
                          showSizeChanger: false,
                        }}
                        scroll={{ x: 1240 }}
                        rowClassName={(record) =>
                          record.key === filters.focusAgent ? "ant-table-row-selected" : ""
                        }
                        onRow={(record) => ({
                          className: "c2e-row-clickable",
                          onClick: () => patch({ focusAgent: record.key, page: filters.page }),
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
                        <p className="ag-muted">时长越低，响应越快；气泡大小表示服务群数。</p>
                      </div>
                    ),
                  },
                ]}
              />
            </InsightSection>
          </div>
        </>
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
              已解决事件数、客服回复消息数暂无数据来源。
              {aliasIsAuthoritative ? "" : "客服姓名为占位别名，尚未接入权威名册。"}
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
            <div className="ri-context">
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
            <p className="ri-room-id">
              {focus.key}
              {aliasIsAuthoritative ? "" : " · 姓名为占位别名"}
            </p>
            {focus.failedCells ? (
              <Alert
                type="warning"
                showIcon
                title={`${focus.failedCells} 个服务群日抽取失败，事件统计不完整`}
              />
            ) : null}
            <InsightMetrics
              items={[
                {
                  key: "involved",
                  label: "参与事件",
                  value: formatInt(focus.involved),
                  unit: "起",
                  info: METRIC.involved,
                  note: `服务 ${focus.rooms} 个群`,
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
              footer="按事件开始日归属；服务群抽取失败日留空。"
            >
              <div className="ia-chart">
                <TrendChart
                  days={days}
                  series={[
                    {
                      name: "参与事件",
                      values: focus.involvedSeries.map((value, index) =>
                        dataset.groupDaily.some(
                          (day) =>
                            day.dt === days[index] &&
                            focus.roomIds.includes(day.roomid) &&
                            day.extraction_status === "failed",
                        )
                          ? null
                          : value,
                      ),
                      area: true,
                    },
                    {
                      name: "首响归属",
                      values: focus.ownedSeries.map((value, index) =>
                        dataset.groupDaily.some(
                          (day) =>
                            day.dt === days[index] &&
                            focus.roomIds.includes(day.roomid) &&
                            day.extraction_status === "failed",
                        )
                          ? null
                          : value,
                      ),
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
              title="服务群明细"
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
