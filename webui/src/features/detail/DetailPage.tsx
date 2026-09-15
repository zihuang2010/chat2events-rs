/**
 * 明细追溯：全字段表格、**服务端分页与服务端排序**、点行开抽屉看消息链路与统计依据。
 *
 * ⚠️ **排序在服务端，而且只有几列能排**（`EVENT_SORTS`，与后端白名单同一份）。
 * 在浏览器里排只会把**当前这一页**重排一遍，而表头看起来像排了全部 ——
 * 那种错没人看得出来，所以不在索引里的列宁可不给排，也不给一个假的。
 */

import { Table, Tag, Tooltip } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useMemo } from "react";
import { EVENT_SORTS, MAX_PAGE, METRIC, PAGE_SIZE_MAX, type EventSort } from "@/domain/definitions";
import { decorate, statusOf } from "@/domain/metrics";
import type { DecoratedEvent } from "@/domain/schemas";
import { useEventsPage, useSummary } from "@/api/queries";
import { ErrorState, PageSkeleton } from "@/components/states";
import { InsightsLayout, InsightMetrics, InsightSection } from "@/features/insights/InsightsLayout";
import { formatInt, formatPercent } from "@/lib/format";
import { DataGap, DurationOrNull, NullValue, StatusTag } from "@/components/primitives";
import { EmptyState } from "@/components/states";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { EventDrawer } from "./EventDrawer";
import "./detail.css";

export function DetailPage({ analytics, api }: { analytics: Analytics; api: FiltersApi }) {
  const { roomLabel, agentLabel, slaSec } = analytics;
  const { filters, patch, reset } = api;
  const source = analytics.dataset.source;
  const summary = useSummary(source, analytics.q);
  // 翻页护栏与后端 `Paging::window` 同一组数：越界那边是 **400 不是截断**，先在这边拦住。
  // ⚠️ 这两个 `Math.min` 夹的是**请求参数**，不是页数 —— 页数由后端算好（`pages`），
  // 这里一个算术都不做。手改 URL 到 `?page=999` 因此看到的是空表（越过实际页数的空一页），
  // 不是一屏 `ErrorState`：一次误操作不该长得像故障。
  const pageSize = Math.min(PAGE_SIZE_MAX, filters.pageSize);
  const sorting = useMemo(
    () => ({ sort: filters.sort, dir: filters.dir }),
    [filters.sort, filters.dir],
  );
  const currentPage = Math.min(filters.page, MAX_PAGE);
  const pageQuery = useEventsPage(source, analytics.q, currentPage, pageSize, sorting);
  // ⚠️ **总数、页数都来自 `/api/events` 自己**，不再借 `/api/summary` 的事件量。
  // 那个数只算**已知成功群日**上的事件，而明细表翻的是窗口内全部事件 ——
  // 窗口里一有抽取失败的群日，两个数就不等，取较小值那步把人夹在更早的页码上，
  // 尾部的行永远翻不到，而页面看起来一切正常。页数由后端夹好护栏，这里不做算术。
  const page = pageQuery.data;
  const total = page?.total ?? 0;
  const events = useMemo(
    () => decorate(page?.rows ?? [], analytics.taxIndex),
    [page, analytics.taxIndex],
  );
  // 抽屉里的事件**按 id 单独取**，不在当前页里找：别人把 ?drawer=123 发给你，
  // 哪怕它不在这一页、不在你的筛选范围内，也必须能打开（`EventDrawer` 自己会拉）。
  const openedEvent = events.find((e) => e.id === filters.drawer);

  // antd 的三态排序（升→降→无）直接映射成 URL 上的 sort/dir。
  const sortOrderOf = (key: EventSort) =>
    filters.sort === key ? (filters.dir === "desc" ? "descend" : "ascend") : null;

  const columns: ColumnsType<DecoratedEvent> = [
    {
      title: "开始时间",
      dataIndex: "first_msg_time",
      key: "first_msg_time",
      sorter: true,
      sortOrder: sortOrderOf("time"),
      fixed: "left",
      width: 112,
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
    },
    {
      title: "群",
      dataIndex: "roomid",
      key: "roomid",
      sorter: true,
      sortOrder: sortOrderOf("room"),
      width: 144,
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
      sorter: true,
      sortOrder: sortOrderOf("reply"),
      width: 112,
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
      sorter: true,
      sortOrder: sortOrderOf("wait"),
      align: "right",
      width: 104,
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
      title: <Tooltip title={METRIC.tail}>尾部</Tooltip>,
      dataIndex: "last_msg_role",
      key: "last_msg_role",
      width: 104,
      render: (v: "EXTERNAL" | "INTERNAL" | null, e) =>
        v === null ? (
          <DataGap detail="加这一列之前抽取的历史行，冻结区不可回填。" />
        ) : (
          <span title={`末条来源消息 ${e.last_msg_time} UTC+8`}>
            {v === "EXTERNAL" ? "商家最后" : "客服收尾"}
          </span>
        ),
    },
    {
      title: <Tooltip title={METRIC.followupWait}>后续等待</Tooltip>,
      dataIndex: "followup_wait_max_sec",
      key: "followup_wait_max_sec",
      width: 120,
      render: (v: number | null) =>
        v === null ? (
          <DataGap detail="加这一列之前抽取的历史行，冻结区不可回填。" />
        ) : v === 0 ? (
          // 0 是算出来的事实，不是缺数据 —— 显示成「0 秒」会让人以为秒回。
          <span title="首响之后没有第二轮">无后续轮次</span>
        ) : (
          <span title="最长的一次「商家说话 → 客服接话」，只算 08:30–21:00 之内的时间">
            <DurationOrNull value={v} />
          </span>
        ),
    },
  ];

  // 上一条 / 下一条**要能跨页**：抽屉是用来连续核对的，走到页尾就断掉等于把这件事废掉。
  // 相邻页各多取一次 —— 行数是一页，缓存也顺带把翻页预热了。
  const openedIndex = openedEvent ? events.findIndex((event) => event.id === openedEvent.id) : -1;
  const previous = openedIndex > 0 ? events[openedIndex - 1] : undefined;
  const next = openedIndex >= 0 ? events[openedIndex + 1] : undefined;
  const prevPage = useEventsPage(
    currentPage > 1 ? source : undefined,
    analytics.q,
    currentPage - 1,
    pageSize,
    sorting,
  );
  const nextPage = useEventsPage(
    currentPage < (page?.pages ?? 0) ? source : undefined,
    analytics.q,
    currentPage + 1,
    pageSize,
    sorting,
  );
  const previousAcross =
    previous ?? (openedIndex === 0 ? prevPage.data?.rows[pageSize - 1] : undefined);
  const nextAcross =
    next ?? (openedIndex === events.length - 1 ? nextPage.data?.rows[0] : undefined);

  if (summary.isError)
    return <ErrorState error={summary.error} onRetry={() => void summary.refetch()} />;
  if (pageQuery.isError)
    return <ErrorState error={pageQuery.error} onRetry={() => void pageQuery.refetch()} />;
  if (!summary.data || !page) return <PageSkeleton />;
  const agg = summary.data;

  return (
    <InsightsLayout
      title="数据追溯"
      subtitle="事件明细、原始消息与统计依据"
      analytics={analytics}
      api={api}
    >
      <InsightMetrics
        unavailable={analytics.cov.known === 0}
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
            key: "sourceMessages",
            label: "来源消息数",
            value: formatInt(agg.sourceMessages),
            unit: "条",
            info: METRIC.sourceMessages,
            note: "当前匹配事件 · 按消息去重",
          },
          {
            key: "events",
            label: "事件量",
            value: formatInt(agg.events),
            unit: "起",
            info: METRIC.events,
            note: `${agg.agents} 位活跃客服 · 按匹配事件去重`,
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
            tone: agg.unreplied ? "risk" : undefined,
            info: METRIC.unreplied,
            note: `无响应率 ${formatPercent(agg.unrepliedRate) ?? "—"}`,
          },
          {
            key: "overdue",
            label: "超时事件",
            value: formatInt(agg.overdue),
            unit: "起",
            tone: agg.overdue ? "warn" : undefined,
            info: METRIC.overdue,
            note: `超时率 ${formatPercent(agg.overdueRate) ?? "—"}`,
          },
        ]}
      />
      <InsightSection
        title="事件明细"
        info="一行是一个事件，可能由多条消息组成。首响耗时是 first_agent_reply_time 减 first_msg_time。平台发起的事件首响恒 0 秒，不进任何首响指标。"
        footer={
          // ⚠️ 被翻页护栏夹过时要说一句**能让人知道该怎么做**的话 ——
          // 否则「只能翻到这里」看起来就是「数据到头了」，而那两件事差得很远。
          page.truncated
            ? `结果超过可翻页的范围，只能翻到第 ${page.pages} 页（共 ${total} 起）；请缩小日期范围或加筛选。首响按工作时段 08:30–21:00 计算，时段外的等待不计；平台发起事件不计入首响指标。`
            : "首响按工作时段 08:30–21:00 计算，时段外的等待不计；平台发起事件不计入首响指标。"
        }
      >
        {total === 0 ? (
          <EmptyState
            title="没有匹配的事件"
            description="当前筛选条件下没有事件。抽取失败不代表业务量为零。"
            onReset={reset}
          />
        ) : (
          <Table<DecoratedEvent>
            className="ia-trace-table"
            size="small"
            tableLayout="fixed"
            rowKey="id"
            columns={columns.map((column) => ({ ...column, ellipsis: true }))}
            onChange={(_, __, sorter, extra) => {
              // ⚠️ `Table.onChange` 对**每一种**表格变化都触发，翻页也算。
              // 不拦住就会在点第 2 页时把 `page` 复位成 1 —— 分页彻底点不动，
              // 而排序看起来一切正常。群 / 客服两张表同样这么拦。
              if (extra.action !== "sort") return;
              const picked = Array.isArray(sorter) ? sorter[0] : sorter;
              const key = String(picked?.columnKey ?? "");
              const column = (Object.keys(EVENT_SORTS) as EventSort[]).find(
                (name) => EVENT_SORTS[name] === key,
              );
              // 取消排序（antd 的第三态）回到默认的归属日序，不是「保持上一列」。
              patch(
                picked?.order && column
                  ? { sort: column, dir: picked.order === "descend" ? "desc" : "asc", page: 1 }
                  : { sort: null, dir: null, page: 1 },
              );
            }}
            dataSource={events}
            loading={pageQuery.isFetching}
            scroll={{ x: 1464 }}
            sticky
            pagination={{
              current: currentPage,
              pageSize,
              // 总数来自**这一页自己那条响应**，与表格必然同集合。
              total,
              showSizeChanger: true,
              // 上限跟后端的 `PAGE_SIZE_MAX` 走，200 那一档会被那边 400 掉。
              pageSizeOptions: [20, 50, 100],
              showTotal: (n, range) => `${range[0]} - ${range[1]} / 共 ${n} 起`,
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
        missingId={filters.drawer}
        analytics={analytics}
        position={
          openedIndex >= 0
            ? `${(currentPage - 1) * pageSize + openedIndex + 1} / ${total}`
            : undefined
        }
        onPrevious={
          previousAcross
            ? () =>
                patch({
                  drawer: previousAcross.id,
                  page: previous ? currentPage : currentPage - 1,
                })
            : undefined
        }
        onNext={
          nextAcross
            ? () => patch({ drawer: nextAcross.id, page: next ? currentPage : currentPage + 1 })
            : undefined
        }
        onClose={() => patch({ drawer: null, page: currentPage })}
      />
    </InsightsLayout>
  );
}
