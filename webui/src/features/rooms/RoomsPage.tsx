/** 群聊洞察：筛选、比较群维度指标，并下钻事件明细。 */

import { ArrowRightOutlined, InfoCircleOutlined } from "@ant-design/icons";
import { Select, Table, Tooltip } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { METRIC, SLA_OPTIONS } from "@/domain/definitions";
import { roomRollup, type RoomRow } from "@/domain/metrics";
import { DataGap, DurationOrNull, NumberOrNull, PercentOrNull } from "@/components/primitives";
import { EmptyState } from "@/components/states";
import { formatDuration, formatInt, shortId } from "@/lib/format";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { ContextBar } from "@/features/filters/ContextBar";
import { RoomFilters } from "./RoomFilters";
import { RoomInsightsDrawer } from "./RoomInsightsDrawer";
import "./rooms.css";

export function RoomsPage({ analytics, api }: { analytics: Analytics; api: FiltersApi }) {
  const [selectedRoom, setSelectedRoom] = useState<string | null>(null);
  const { events, days, cov, roomLabel, slaSec, lastDay, dayset, query, aliasIsAuthoritative } =
    analytics;
  const { filters, patch, hrefWith, reset } = api;

  const rows = useMemo(
    () =>
      roomRollup({
        events,
        groupDaily: analytics.dataset.groupDaily,
        rooms: analytics.dataset.meta.rooms.filter(
          (room) => !filters.room || room.roomid === filters.room,
        ),
        days,
        dayset,
        slaSec,
        lastDay,
        labelOf: roomLabel,
        query,
      }),
    [events, analytics.dataset, days, dayset, slaSec, lastDay, roomLabel, query, filters.room],
  );

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
            aria-haspopup="dialog"
            onClick={(event) => {
              event.stopPropagation();
              setSelectedRoom(r.key);
            }}
          >
            {r.label}
          </button>
          {aliasIsAuthoritative ? null : (
            <>
              {" "}
              <DataGap
                label="别名 待补"
                detail="库里只有 officialRoomId，没有群名。这是占位别名，需要一张花名册映射表。"
              />
            </>
          )}
          <span className="c2e-sub">{shortId(r.key)}</span>
        </>
      ),
    },
    {
      title: <Tooltip title={METRIC.msgCount}>消息数</Tooltip>,
      dataIndex: "msgs",
      key: "msgs",
      className: "ra-metric-emphasis",
      align: "right",
      width: 92,
      sorter: (a, b) => a.msgs - b.msgs,
    },
    {
      title: <Tooltip title={METRIC.events}>事件数</Tooltip>,
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
      title: "已解决",
      key: "resolved",
      width: 96,
      responsive: ["xxl"],
      render: () => <DataGap detail="库里没有 resolved 列，也没有「什么算解决」的定义。" />,
    },
    {
      title: "数据完整性",
      key: "coverage",
      width: 116,
      render: (_, r) =>
        r.failedDays > 0 ? (
          <Tooltip title={`${r.failedDays} 个「群 × 日」抽取失败，这一行的事件级数字不含它们`}>
            <span style={{ color: "var(--c2e-critical-ink)", fontWeight: 600 }}>
              {r.failedDays} / {r.totalDays} 日失败
            </span>
          </Tooltip>
        ) : (
          <span style={{ color: "var(--c2e-good-ink)" }}>{r.totalDays} 日完整</span>
        ),
    },
  ];

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
      <div className="od-context ra-context">
        <div>
          <span>
            当前群数 <b>{formatInt(rows.length)}</b>
          </span>
          <span>
            首响阈值 <b>{formatDuration(slaSec)}</b>
          </span>
        </div>
        <div className="ra-selected-filters" role="region" aria-label="已选筛选条件">
          <ContextBar analytics={analytics} api={api} part="filters" />
        </div>
        {cov.failed ? (
          <details className="ra-coverage">
            <summary>
              <InfoCircleOutlined /> {cov.failed} / {cov.cells} 个群日抽取失败
            </summary>
            <p>
              涉及 {cov.rooms.map(roomLabel).join("、")}，日期 {cov.days.join("、")}。
              事件指标不含失败群日，当前统计不完整。
            </p>
          </details>
        ) : (
          <span>{cov.cells ? "当前窗口抽取完整" : "当前窗口无群日记录"}</span>
        )}
      </div>

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
              <p>{rows.length} 个群 · 当前筛选范围</p>
            </div>
            <Link className="od-link" to={hrefWith({}, "/detail")}>
              事件明细 <ArrowRightOutlined />
            </Link>
          </div>
          <Table<RoomRow>
            size="small"
            rowKey="key"
            columns={columns}
            dataSource={rows}
            pagination={false}
            scroll={{ x: "max-content" }}
            onRow={(record) => ({
              className: "c2e-row-clickable",
              onClick: () => setSelectedRoom(record.key),
            })}
          />
          <p className="od-footnote">已解决状态暂无数据来源。</p>
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
