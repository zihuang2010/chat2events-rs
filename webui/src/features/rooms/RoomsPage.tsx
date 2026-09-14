/** 群聊洞察：筛选、比较群维度指标，并下钻事件明细。 */

import { ArrowRightOutlined, InfoCircleOutlined } from "@ant-design/icons";
import { Select, Table, Tooltip } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { METRIC, SLA_OPTIONS } from "@/domain/definitions";
import { roomRollup, type RoomRow } from "@/domain/metrics";
import { useRoomAggs } from "@/api/queries";
import { ErrorState, PageSkeleton } from "@/components/states";
import { DataGap, DurationOrNull, NumberOrNull, PercentOrNull } from "@/components/primitives";
import { EmptyState } from "@/components/states";
import { formatInt, shortId } from "@/lib/format";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { RoomFilters } from "./RoomFilters";
import { RoomInsightsDrawer } from "./RoomInsightsDrawer";
import "./rooms.css";

export function RoomsPage({ analytics, api }: { analytics: Analytics; api: FiltersApi }) {
  const [selectedRoom, setSelectedRoom] = useState<string | null>(null);
  const { days, roomLabel, dayset, query, roomAliasIsAuthoritative, parents } = analytics;
  const { filters, patch, hrefWith, reset } = api;
  const groups = useMemo(() => parents.map((parent) => parent.types), [parents]);
  const aggs = useRoomAggs(analytics.dataset.source, analytics.q, groups);

  const rows = useMemo(
    () =>
      roomRollup({
        aggs: aggs.data ?? [],
        groupDaily: analytics.dataset.groupDaily,
        rooms: analytics.dataset.meta.rooms.filter(
          (room) => !filters.room || room.roomid === filters.room,
        ),
        days,
        dayset,
        labelOf: roomLabel,
        parents,
        query,
      }),
    [aggs.data, analytics.dataset, days, dayset, roomLabel, parents, query, filters.room],
  );
  // 表格行数是群数（有界），所以翻页与排序都还留在前端 —— 与明细表不同。
  const pageSize = Math.min(200, filters.pageSize);
  const currentPage = Math.min(filters.page, Math.max(1, Math.ceil(rows.length / pageSize)));
  const messageTotal = rows.some((row) => row.msgs !== null)
    ? rows.reduce((sum, row) => sum + (row.msgs ?? 0), 0)
    : null;
  const messagesIncomplete = rows.some((row) => row.missingDays > 0 || row.unknownDays > 0);
  const knownEventRows = rows.filter((row) => row.events !== null);
  const activeRooms = knownEventRows.length
    ? knownEventRows.filter((row) => row.events! > 0).length
    : null;
  const eventTotal = knownEventRows.length
    ? knownEventRows.reduce((sum, row) => sum + row.events!, 0)
    : null;
  const eventsIncomplete = messagesIncomplete || rows.some((row) => row.failedDays > 0);

  const columns: ColumnsType<RoomRow> = [
    {
      title: "群",
      dataIndex: "label",
      key: "label",
      fixed: "left",
      width: 190,
      sorter: (a, b) => a.label.localeCompare(b.label, "zh"),
      render: (_, r) => (
        <>
          <button
            type="button"
            className="ra-room-link"
            title={r.label}
            aria-haspopup="dialog"
            onClick={(event) => {
              event.stopPropagation();
              setSelectedRoom(r.key);
            }}
          >
            {r.label}
          </button>
          {roomAliasIsAuthoritative(r.key) ? null : (
            <>
              {" "}
              <DataGap label="别名 待补" detail="尚未获取到该群的权威名称。" />
            </>
          )}
          <span className="c2e-sub">{shortId(r.key)}</span>
        </>
      ),
    },
    {
      title: <Tooltip title={METRIC.msgCount}>消息总量</Tooltip>,
      dataIndex: "msgs",
      key: "msgs",
      className: "ra-metric-emphasis",
      align: "right",
      width: 92,
      sorter: (a, b) => (a.msgs ?? -1) - (b.msgs ?? -1),
      render: (n: number | null) => <NumberOrNull value={n} />,
    },
    {
      title: <Tooltip title={METRIC.events}>事件量</Tooltip>,
      dataIndex: "events",
      key: "events",
      className: "ra-metric-emphasis",
      align: "right",
      width: 92,
      defaultSortOrder: "descend",
      sorter: (a, b) => (a.events ?? -1) - (b.events ?? -1),
      render: (v: number | null) => <NumberOrNull value={v} />,
    },
    {
      title: "主要事件类型",
      key: "top",
      width: 300,
      responsive: ["xl"],
      render: (_, r) =>
        r.topLevel1.length === 0 ? (
          <span className="c2e-null">—</span>
        ) : (
          <div className="ra-category-tags">
            {r.topLevel1.map((t) => (
              <Link
                key={t.name}
                to={hrefWith({ room: r.key, level1: t.name }, "/detail")}
                className="ra-category-link"
                onClick={(e) => {
                  e.stopPropagation();
                }}
              >
                <span>{t.name}</span>
                <b>{formatInt(t.count)}</b>
              </Link>
            ))}
          </div>
        ),
    },
    {
      title: <Tooltip title={METRIC.merchant}>商家发起</Tooltip>,
      dataIndex: "merchant",
      key: "merchant",
      align: "right",
      width: 96,
      responsive: ["lg"],
      sorter: (a, b) => (a.merchant ?? -1) - (b.merchant ?? -1),
      render: (v: number | null) => <NumberOrNull value={v} />,
    },
    {
      title: <Tooltip title={METRIC.p50}>首响 P50</Tooltip>,
      dataIndex: "p50",
      key: "p50",
      align: "right",
      width: 104,
      sorter: (a, b) => (a.p50 ?? Infinity) - (b.p50 ?? Infinity),
      render: (v: number | null) => <DurationOrNull value={v} />,
    },
    {
      title: <Tooltip title={METRIC.p90}>首响 P90</Tooltip>,
      dataIndex: "p90",
      key: "p90",
      align: "right",
      width: 116,
      sorter: (a, b) => (a.p90 ?? Infinity) - (b.p90 ?? Infinity),
      render: (v: number | null) => <DurationOrNull value={v} />,
    },
    {
      title: <Tooltip title={METRIC.unreplied}>无响应</Tooltip>,
      dataIndex: "unreplied",
      key: "unreplied",
      align: "right",
      width: 88,
      sorter: (a, b) => (a.unreplied ?? -1) - (b.unreplied ?? -1),
      render: (v: number | null) =>
        v === null ? (
          <NumberOrNull value={v} />
        ) : v > 0 ? (
          <b style={{ color: "var(--c2e-critical-ink)" }}>{v}</b>
        ) : (
          0
        ),
    },
    {
      title: "无响应率",
      dataIndex: "unrepliedRate",
      key: "unrepliedRate",
      align: "right",
      width: 96,
      responsive: ["lg"],
      sorter: (a, b) => (a.unrepliedRate ?? -1) - (b.unrepliedRate ?? -1),
      render: (v: number | null) => <PercentOrNull value={v} />,
    },
    {
      title: <Tooltip title={METRIC.overdue}>超时率</Tooltip>,
      dataIndex: "overdueRate",
      key: "overdueRate",
      align: "right",
      width: 90,
      sorter: (a, b) => (a.overdueRate ?? -1) - (b.overdueRate ?? -1),
      render: (v: number | null) => <PercentOrNull value={v} />,
    },
    {
      title: "数据完整性",
      key: "coverage",
      width: 116,
      render: (_, r) =>
        r.unknownDays > 0 ? (
          <span style={{ color: "var(--c2e-critical-ink)" }}>{r.unknownDays} 日最新结果未知</span>
        ) : r.missingDays > 0 ? (
          <span style={{ color: "var(--c2e-critical-ink)" }}>
            {r.missingDays} 日无记录，完整性未知{r.failedDays ? ` · ${r.failedDays} 日失败` : ""}
          </span>
        ) : r.failedDays > 0 ? (
          <Tooltip title={`${r.failedDays} 个「群 × 日」抽取失败，这一行的事件级数字不含它们`}>
            <span style={{ color: "var(--c2e-critical-ink)", fontWeight: 600 }}>
              {r.failedDays} / {r.totalDays} 日失败
            </span>
          </Tooltip>
        ) : r.pendingLabels || r.failedLabels ? (
          <span style={{ color: "var(--c2e-critical-ink)" }}>
            {[
              r.pendingLabels ? `${r.pendingLabels} 日待打标` : "",
              r.failedLabels ? `${r.failedLabels} 日打标失败` : "",
            ]
              .filter(Boolean)
              .join(" · ")}
          </span>
        ) : (
          <span style={{ color: "var(--c2e-good-ink)" }}>{r.totalDays} 日完整</span>
        ),
    },
  ];

  if (aggs.isError) return <ErrorState error={aggs.error} onRetry={() => void aggs.refetch()} />;
  if (!aggs.data) return <PageSkeleton />;

  return (
    <main className="od-overview od-room-analysis">
      <header className="od-header">
        <div>
          <h1>群聊洞察</h1>
          <p>群聊业务量与响应表现</p>
        </div>
        <section className="ra-metric-scope" aria-label="指标口径">
          <span className="ra-scope-label">指标口径</span>
          <label htmlFor="room-sla">首响阈值</label>
          <Select
            id="room-sla"
            aria-label="首响阈值"
            value={filters.slaSec}
            options={SLA_OPTIONS.map((option) => ({ value: option.value, label: option.label }))}
            onChange={(value: number) => patch({ slaSec: value })}
          />
          <Tooltip title={METRIC.overdue} mouseEnterDelay={0.8} trigger={["hover", "focus"]}>
            <button type="button" className="od-info" aria-label="首响阈值口径">
              <InfoCircleOutlined />
            </button>
          </Tooltip>
          <span>{days.length} 天</span>
          <Tooltip title={METRIC.timezone} mouseEnterDelay={0.8} trigger={["hover", "focus"]}>
            <button type="button" className="ra-timezone">
              UTC+8
            </button>
          </Tooltip>
        </section>
      </header>

      <RoomFilters meta={analytics.dataset.meta} api={api} />
      {rows.length === 0 ? (
        <EmptyState
          title="没有匹配的群"
          description="当前日期区间与筛选条件下没有任何群有指标行。放宽日期或清空筛选试试。"
          onReset={reset}
        />
      ) : (
        <section className="ra-details" aria-labelledby="room-details-title">
          <div className="od-section-head">
            <div>
              <h2 id="room-details-title">群维度指标</h2>
              <p className="ra-summary" aria-label="群指标摘要">
                <span>
                  活跃群 <b>{activeRooms === null ? "—" : formatInt(activeRooms)}</b> 个
                  <Tooltip title={METRIC.rooms} mouseEnterDelay={0.8} trigger={["hover", "focus"]}>
                    <button type="button" className="od-info" aria-label="活跃群口径">
                      <InfoCircleOutlined aria-hidden="true" />
                    </button>
                  </Tooltip>
                </span>
                <span>
                  消息总量 <b>{messageTotal === null ? "—" : formatInt(messageTotal)}</b> 条
                  <Tooltip
                    title={METRIC.roomMessageTotal}
                    mouseEnterDelay={0.8}
                    trigger={["hover", "focus"]}
                  >
                    <button type="button" className="od-info" aria-label="消息总量口径">
                      <InfoCircleOutlined aria-hidden="true" />
                    </button>
                  </Tooltip>
                </span>
                <span>
                  事件量 <b>{eventTotal === null ? "—" : formatInt(eventTotal)}</b> 起
                </span>
                <span>当前筛选范围 · {formatInt(rows.length)} 个群</span>
                {messageTotal === null ? (
                  <span className="ra-coverage">无群日记录，消息总量暂缺</span>
                ) : messagesIncomplete ? (
                  <span className="ra-coverage">数据不完整，仅已知量</span>
                ) : null}
                {eventsIncomplete ? <span className="ra-coverage">事件统计不完整</span> : null}
                {rows.some((row) => row.pendingLabels || row.failedLabels) ? (
                  <span className="ra-coverage">分类统计未完成</span>
                ) : null}
              </p>
            </div>
            <Link className="od-link" to={hrefWith({}, "/detail")}>
              事件明细 <ArrowRightOutlined />
            </Link>
          </div>
          <Table<RoomRow>
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
              showTotal: (total, range) => `${range[0]} - ${range[1]} / 共 ${total} 个群`,
              onChange: (page, size) =>
                patch({ page: size === pageSize ? page : 1, pageSize: size }),
            }}
            onChange={(_, __, ___, extra) => {
              if (extra.action === "sort") patch({ page: 1 });
            }}
            scroll={{ x: "100%" }}
            onRow={(record) => ({
              className: "c2e-row-clickable",
              onClick: () => setSelectedRoom(record.key),
            })}
          />
        </section>
      )}
      <RoomInsightsDrawer
        roomId={selectedRoom}
        analytics={analytics}
        api={api}
        onClose={() => setSelectedRoom(null)}
      />
    </main>
  );
}
