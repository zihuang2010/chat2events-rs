/** 明细追溯：全字段表格、真分页、点行开抽屉看消息链路与统计依据。 */

import { Table, Tag, Tooltip } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useMemo, useState } from "react";
import { METRIC } from "@/domain/definitions";
import { statusOf } from "@/domain/metrics";
import type { DecoratedEvent } from "@/domain/schemas";
import { InsightsLayout, InsightMetrics, InsightSection } from "@/features/insights/InsightsLayout";
import { formatInt, formatPercent } from "@/lib/format";
import { DataGap, DurationOrNull, NullValue, StatusTag } from "@/components/primitives";
import { EmptyState } from "@/components/states";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { EventDrawer } from "./EventDrawer";
import "./detail.css";

export function DetailPage({ analytics, api }: { analytics: Analytics; api: FiltersApi }) {
  const { events, agg, roomLabel, agentLabel, slaSec } = analytics;
  const { filters, patch, reset } = api;
  const [sort, setSort] = useState<{ key: string; order: "ascend" | "descend" | null }>({
    key: "first_msg_time",
    order: "descend",
  });
  const pageSize = Math.min(200, filters.pageSize);
  const currentPage = Math.min(filters.page, Math.max(1, Math.ceil(events.length / pageSize)));

  // 抽屉里的事件在**全量数据**里找，不在筛选结果里找。
  // 可追溯的链接不该被筛选条件吞掉：别人把 ?drawer=123 发给你，
  // 哪怕它不在你当前的筛选范围内，也必须能打开，只是要说明这一点。
  const openedEvent = useMemo(
    () =>
      filters.drawer === null
        ? undefined
        : analytics.dataset.events.find((e) => e.id === filters.drawer),
    [analytics.dataset.events, filters.drawer],
  );
  const openedOutsideFilter =
    openedEvent !== undefined && !events.some((e) => e.id === openedEvent.id);

  const columns: ColumnsType<DecoratedEvent> = [
    {
      title: "开始时间",
      dataIndex: "first_msg_time",
      key: "first_msg_time",
      fixed: "left",
      width: 112,
      sorter: (a, b) => a.first_msg_time.localeCompare(b.first_msg_time),
      render: (v: string) => (
        <span className="c2e-mono" title={`${v} UTC+8`}>
          {v.slice(5, 16)}
        </span>
      ),
    },
    {
      title: "事件",
      dataIndex: "summary",
      key: "summary",
      width: 340,
      sorter: (a, b) => a.summary.localeCompare(b.summary, "zh"),
      render: (_, e) => (
        <div className="ia-trace-summary">
          <span className="ia-trace-id" title={`事件 #${e.id}`}>
            #{e.id}
          </span>
          <button
            type="button"
            className="ia-summary-link"
            aria-haspopup="dialog"
            aria-label={`查看事件 #${e.id}：${e.summary}`}
            onClick={(event) => {
              event.stopPropagation();
              patch({ drawer: e.id, page: currentPage });
            }}
          >
            <span className="ia-trace-ellipsis" title={e.summary}>
              {e.summary}
            </span>
          </button>
          <div className="ia-trace-meta">
            {e.event_types.length > 1 ? (
              <Tooltip title="副类只落在 event_types 列供下钻，不进任何指标">
                <Tag>+{e.event_types.length - 1} 副类</Tag>
              </Tooltip>
            ) : null}
            {e.crossDay ? (
              <Tooltip title="末条消息与归属日不同天，但事件只按开始日计一次">
                <Tag>跨天</Tag>
              </Tooltip>
            ) : null}
          </div>
        </div>
      ),
    },
    {
      title: "一级分类",
      dataIndex: "level1",
      key: "level1",
      width: 104,
      sorter: (a, b) => a.level1.localeCompare(b.level1, "zh"),
      render: (v: string) => (
        <span className="ia-trace-category ia-trace-ellipsis" title={v}>
          {v}
        </span>
      ),
    },
    {
      title: "二级分类",
      dataIndex: "level2",
      key: "level2",
      width: 112,
      sorter: (a, b) => a.level2.localeCompare(b.level2, "zh"),
    },
    {
      title: "群",
      dataIndex: "roomid",
      key: "roomid",
      width: 144,
      sorter: (a, b) => roomLabel(a.roomid).localeCompare(roomLabel(b.roomid), "zh"),
      render: (v: string) => (
        <span className="ia-trace-ellipsis" title={roomLabel(v)}>
          {roomLabel(v)}
        </span>
      ),
    },
    {
      title: "活跃客服",
      dataIndex: "agents",
      key: "agents",
      width: 120,
      render: (v: string[]) =>
        v.length ? (
          <span className="ia-trace-ellipsis" title={v.map(agentLabel).join("、")}>
            {v.map(agentLabel).join("、")}
          </span>
        ) : (
          <span className="c2e-null">无</span>
        ),
    },
    {
      title: "首响时间",
      dataIndex: "first_agent_reply_time",
      key: "first_agent_reply_time",
      width: 112,
      sorter: (a, b) =>
        (a.first_agent_reply_time ?? "").localeCompare(b.first_agent_reply_time ?? ""),
      render: (v: string | null) =>
        v === null ? (
          <NullValue reason="NULL：无响应，不是 0 秒" />
        ) : (
          <span className="c2e-mono">{v.slice(5, 16)}</span>
        ),
    },
    {
      title: <Tooltip title={METRIC.p50}>首响耗时</Tooltip>,
      dataIndex: "firstReplySec",
      key: "firstReplySec",
      align: "right",
      width: 104,
      sorter: (a, b) => (a.firstReplySec ?? Infinity) - (b.firstReplySec ?? Infinity),
      render: (v: number | null, event) => (
        <strong className="ia-trace-duration" data-status={statusOf(event, slaSec)}>
          {v === null ? "未响应" : <DurationOrNull value={v} />}
        </strong>
      ),
    },
    {
      title: "状态",
      key: "status",
      width: 128,
      render: (_, e) => <StatusTag status={statusOf(e, slaSec)} />,
    },
    {
      title: "已解决",
      key: "resolved",
      width: 88,
      render: () => <DataGap />,
    },
  ];

  const comparator = columns.find((column) => column.key === sort.key)?.sorter;
  const ordered =
    sort.order && typeof comparator === "function"
      ? [...events].sort(
          (a, b) => comparator(a, b, sort.order) * (sort.order === "ascend" ? 1 : -1),
        )
      : events;
  const openedIndex = openedEvent ? ordered.findIndex((event) => event.id === openedEvent.id) : -1;
  const previous = openedIndex > 0 ? ordered[openedIndex - 1] : undefined;
  const next = openedIndex >= 0 ? ordered[openedIndex + 1] : undefined;

  return (
    <InsightsLayout
      title="数据追溯"
      subtitle="事件明细、原始消息与统计依据"
      analytics={analytics}
      api={api}
    >
      <InsightMetrics
        unavailable={analytics.cov.cells === analytics.cov.failed}
        items={[
          {
            key: "events",
            label: "匹配事件",
            value: formatInt(agg.events),
            unit: "起",
            info: METRIC.events,
            note: `${agg.rooms} 个群 · ${agg.agents} 位活跃客服`,
          },
          {
            key: "merchant",
            label: "商家发起",
            value: formatInt(agg.merchant),
            unit: "起",
            info: METRIC.merchant,
            note: `平台发起 ${formatInt(agg.push)} 起`,
          },
          {
            key: "unreplied",
            label: "无响应",
            value: formatInt(agg.unreplied),
            unit: "起",
            tone: agg.unreplied ? "bad" : undefined,
            info: METRIC.unreplied,
            note: `无响应率 ${formatPercent(agg.unrepliedRate) ?? "—"}`,
          },
          {
            key: "overdue",
            label: "超时事件",
            value: formatInt(agg.overdue),
            unit: "起",
            tone: agg.overdue ? "mid" : undefined,
            info: METRIC.overdue,
            note: `超时率 ${formatPercent(agg.overdueRate) ?? "—"}`,
          },
        ]}
      />
      <InsightSection
        title="事件明细"
        info="一行是一个事件，可能由多条消息组成。首响耗时是 first_agent_reply_time 减 first_msg_time。平台发起的事件首响恒 0 秒，不进任何首响指标。"
        footer="首响按自然时间计算；平台发起事件不计入首响指标。已解决状态暂无数据来源。"
      >
        {events.length === 0 ? (
          <EmptyState
            title="没有匹配的事件"
            description="当前筛选组合下一条事件都没有。这不代表数据缺失：若要确认是不是抽取失败造成的，看上方完整性提示。"
            onReset={reset}
          />
        ) : (
          <Table<DecoratedEvent>
            className="ia-trace-table"
            size="small"
            tableLayout="fixed"
            rowKey="id"
            columns={columns.map((column) => ({
              ...column,
              ellipsis: true,
              sortOrder: column.key === sort.key ? sort.order : null,
            }))}
            dataSource={events}
            scroll={{ x: 1464 }}
            sticky
            onChange={(_, __, sorter) => {
              const selected = Array.isArray(sorter) ? sorter[0] : sorter;
              setSort({
                key: String(selected?.columnKey ?? "first_msg_time"),
                order: selected?.order ?? null,
              });
            }}
            pagination={{
              current: currentPage,
              pageSize,
              total: events.length,
              showSizeChanger: true,
              pageSizeOptions: [20, 50, 100, 200],
              showTotal: (total, range) => `${range[0]} - ${range[1]} / 共 ${total} 起`,
              onChange: (page, size) =>
                patch({ page: size === pageSize ? page : 1, pageSize: size }),
            }}
            onRow={(record) => ({
              className: `c2e-row-clickable${record.id === filters.drawer ? " ia-trace-selected" : ""}`,
              "data-status": statusOf(record, slaSec),
              onClick: () => patch({ drawer: record.id, page: currentPage }),
            })}
          />
        )}
      </InsightSection>

      <EventDrawer
        event={openedEvent}
        missingId={filters.drawer !== null && openedEvent === undefined ? filters.drawer : null}
        outsideFilter={openedOutsideFilter}
        analytics={analytics}
        position={openedIndex >= 0 ? `${openedIndex + 1} / ${ordered.length}` : undefined}
        onPrevious={
          previous
            ? () =>
                patch({ drawer: previous.id, page: Math.floor((openedIndex - 1) / pageSize) + 1 })
            : undefined
        }
        onNext={
          next
            ? () => patch({ drawer: next.id, page: Math.floor((openedIndex + 1) / pageSize) + 1 })
            : undefined
        }
        onClose={() => patch({ drawer: null, page: currentPage })}
      />
    </InsightsLayout>
  );
}
