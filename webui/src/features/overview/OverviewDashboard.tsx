/**
 * Overview: workload, response quality, then drill-down.
 * Metrics stay in the domain layer; this view only groups and formats them.
 */
import { ArrowRightOutlined } from "@ant-design/icons";
import { useMemo, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { METRIC, SLA_OPTIONS } from "@/domain/definitions";
import { agentRollup, categoryRows, decorate, roomRollup } from "@/domain/metrics";
import { useAgentAggs, useCategories, useEventsPage, useRoomAggs, useSummary } from "@/api/queries";
import { ErrorState, PageSkeleton } from "@/components/states";
import { formatDuration, formatDurationCompact, formatInt, formatPercent } from "@/lib/format";
import { Metric, MetricInfo as Info } from "@/components/Metric";
import { EventDrawer } from "@/features/detail/EventDrawer";
import { CategoryPie, OverviewTrend, ResponseDistribution } from "./OverviewCharts";
import { MSG_INFO, msgRollup, waitedSecFrom, type OverviewProps } from "./overviewMetrics";

/** 待跟进队列在概览上只露这几条，全部走「查看全部」跳明细。 */
const QUEUE_PREVIEW = 6;

function SectionHead({ title, note, to }: { title: string; note?: ReactNode; to?: string }) {
  return (
    <div className="od-section-head">
      <div>
        <h2>{title}</h2>
        {note ? <p>{note}</p> : null}
      </div>
      {to ? (
        <Link className="od-link" to={to}>
          查看全部 <ArrowRightOutlined />
        </Link>
      ) : null}
    </div>
  );
}

export function OverviewDashboard({ analytics, api }: OverviewProps) {
  const { days, roomLabel, slaSec, lastDay, cov, parents } = analytics;
  const { hrefWith } = api;
  const [roomOrder, setRoomOrder] = useState("msgs");
  const source = analytics.dataset.source;
  const groups = useMemo(() => parents.map((parent) => parent.types), [parents]);
  const summaryQuery = useSummary(source, analytics.q);
  const roomsQuery = useRoomAggs(source, analytics.q, groups);
  const agentsQuery = useAgentAggs(source, analytics.q);
  const catsQuery = useCategories(source, analytics.q, groups);
  // 待跟进队列：**只取前几条**，不再把全部无响应事件拉到浏览器里排序。
  // 服务端按 `(occurred_on, id)` 升序，也就是等得最久的排在前面。
  const queueQuery = useEventsPage(
    source,
    useMemo(() => ({ ...analytics.q, status: "unreplied", overdueOnly: null }), [analytics.q]),
    1,
    QUEUE_PREVIEW,
  );
  const m = msgRollup(analytics);
  const missingDays = cov.missing;
  const unavailable = cov.known === 0;
  const count = (n: number) => (unavailable ? "—" : formatInt(n));
  const slaLabel = SLA_OPTIONS.find((o) => o.value === slaSec)?.label ?? `${slaSec} 秒`;
  const failed = [summaryQuery, roomsQuery, agentsQuery, catsQuery, queueQuery].find(
    (query) => query.isError,
  );
  const agg = summaryQuery.data;
  const cats = categoryRows(
    catsQuery.data ?? [],
    "level1",
    analytics.taxIndex,
    parents,
    agg?.events ?? 0,
  );
  const allRooms = roomRollup({
    aggs: roomsQuery.data ?? [],
    groupDaily: analytics.dataset.groupDaily,
    rooms: analytics.dataset.meta.rooms,
    days,
    dayset: analytics.dayset,
    labelOf: roomLabel,
    parents,
    query: analytics.query,
  }).sort((a, b) => {
    if (roomOrder === "unreplied")
      return (b.unreplied ?? -1) - (a.unreplied ?? -1) || (b.msgs ?? -1) - (a.msgs ?? -1);
    if (roomOrder === "overdue")
      return (b.overdueRate ?? -1) - (a.overdueRate ?? -1) || (b.msgs ?? -1) - (a.msgs ?? -1);
    return (b.msgs ?? -1) - (a.msgs ?? -1);
  });
  const rooms = allRooms.slice(0, 10);
  const heatMax = Math.max(1, ...rooms.flatMap((r) => r.series.map((n) => n ?? 0)));
  const agentRows = agentRollup({
    aggs: agentsQuery.data ?? [],
    groupDaily: analytics.dataset.groupDaily,
    rooms: analytics.dataset.meta.rooms,
    days,
    dayset: analytics.dayset,
    labelOf: analytics.agentLabel,
    query: analytics.query,
  })
    .sort((a, b) => b.involved - a.involved)
    .slice(0, 10);
  const maxInvolved = Math.max(1, ...agentRows.map((a) => a.involved));
  const queue = decorate(queueQuery.data?.rows ?? [], analytics.taxIndex);
  if (failed?.error)
    return <ErrorState error={failed.error} onRetry={() => void failed.refetch()} />;
  if (!agg) return <PageSkeleton />;

  const responseStates = [
    {
      label: "阈值内已回复",
      n: agg.merchant - agg.overdue,
      tone: "good",
      to: hrefWith({ status: "replied", overdueOnly: false }, "/detail"),
    },
    {
      label: "超时后已回复",
      n: agg.overdue - agg.unreplied,
      tone: "warn",
      to: hrefWith({ status: "replied", overdueOnly: true }, "/detail"),
    },
    {
      label: "仍无响应",
      n: agg.unreplied,
      tone: "risk",
      to: hrefWith({ status: "unreplied", overdueOnly: null }, "/detail"),
    },
  ];

  return (
    <main className="od-overview">
      <header className="od-header">
        <div>
          <h1>整体概览</h1>
          <p>群聊业务与客服响应</p>
        </div>
        <div className="od-window">
          <strong>
            {days[0] ?? "—"} <span>至</span> {lastDay}
          </strong>
          <span>最近 7 天 · UTC+8 · 工作时段 08:30–21:00</span>
        </div>
      </header>

      <section className="od-metrics od-overview-metrics" aria-label="核心指标" data-count="6">
        <Metric
          unavailable={unavailable}
          label="活跃群"
          value={count(agg.rooms)}
          unit="个"
          info={METRIC.rooms}
          to={hrefWith({}, "/rooms")}
          note={
            <Link to={hrefWith({}, "/agents")}>
              活跃客服 {count(agg.agents)} 人 <ArrowRightOutlined aria-hidden="true" />
            </Link>
          }
        />
        <Metric
          unavailable={!cov.cells}
          label="消息总量"
          value={cov.cells ? formatInt(m.msgs) : "—"}
          unit="条"
          info={MSG_INFO}
          to={hrefWith({}, "/rooms")}
          note={
            missingDays ? (
              <>{missingDays} 个群日无记录 · 日均暂缺</>
            ) : (
              <>日均 {days.length ? formatInt(Math.round(m.msgs / days.length)) : "—"} 条</>
            )
          }
        />
        <Metric
          unavailable={unavailable}
          label="事件量"
          value={count(agg.events)}
          unit="起"
          info={METRIC.events}
          to={hrefWith({}, "/detail")}
          note={
            <>
              <span>商家发起 {count(agg.merchant)}</span> · <span>平台发起 {count(agg.push)}</span>
            </>
          }
        />
        <Metric
          unavailable={unavailable}
          label="无响应事件"
          value={count(agg.unreplied)}
          unit="起"
          info={METRIC.unreplied}
          tone={agg.unreplied ? "risk" : undefined}
          to={hrefWith({ status: "unreplied", overdueOnly: null }, "/detail")}
          note={<>占商家发起 {formatPercent(agg.unrepliedRate) ?? "—"}</>}
        />
        <Metric
          unavailable={unavailable}
          label="首响超时率"
          value={formatPercent(agg.overdueRate) ?? "—"}
          info={METRIC.overdue}
          tone={agg.overdue ? "warn" : undefined}
          to={hrefWith({ overdueOnly: true, status: null }, "/detail")}
          note={
            <>
              {count(agg.overdue)} / {count(agg.merchant)} 起 · 含无响应
            </>
          }
        />
        <Metric
          unavailable={unavailable}
          label="首响 P50"
          value={formatDuration(agg.p50) ?? "—"}
          info={METRIC.p50}
          note={
            <>
              P90 {formatDuration(agg.p90) ?? "—"} · 已回复 {count(agg.replied)} 起
            </>
          }
        />
      </section>

      <div className="od-primary">
        <section className="od-trend">
          <SectionHead title="业务量趋势" note={`${days.length} 天 · 消息与事件分别统计`} />
          <OverviewTrend analytics={analytics} api={api} summary={agg} />
        </section>
        <section className="od-response">
          <SectionHead
            title="响应质量"
            note={
              <>
                商家发起 {count(agg.merchant)} 起 · 首响阈值 {slaLabel}
                <Info text={METRIC.overdue} />
              </>
            }
          />
          <div className="od-status-bar" aria-hidden="true">
            {responseStates.map((s) => (
              <i
                key={s.tone}
                data-tone={s.tone}
                style={{ width: `${agg.merchant ? (s.n / agg.merchant) * 100 : 0}%` }}
              />
            ))}
          </div>
          <div className="od-status-legend">
            {responseStates.map((s) => (
              <Link key={s.tone} to={s.to}>
                <i data-tone={s.tone} />
                <span>{s.label}</span>
                <b>{count(s.n)}</b>
                <small>{formatPercent(agg.merchant ? s.n / agg.merchant : null) ?? "—"}</small>
              </Link>
            ))}
          </div>
          <h3 className="od-subhead">
            首响时长分布 <span>已回复 {count(agg.replied)} 起</span>
          </h3>
          <ResponseDistribution buckets={agg.replyBuckets} replied={agg.replied} slaSec={slaSec} />
        </section>
      </div>

      <section className="od-rooms">
        <SectionHead
          title="群聊运行概况"
          note={
            <>
              群 × 日事件量 · 前 {rooms.length} / {allRooms.length} 个群
            </>
          }
          to={hrefWith({}, "/rooms")}
        />
        <div className="od-table-tools">
          <span className="od-heat-key">
            <i />
            事件量由少到多 <span className="od-missing">—</span> 抽取失败 / 无记录
          </span>
          <label>
            排序{" "}
            <select
              aria-label="群聊排序"
              value={roomOrder}
              onChange={(e) => setRoomOrder(e.target.value)}
            >
              <option value="msgs">消息总量</option>
              <option value="unreplied">无响应数</option>
              <option value="overdue">超时率</option>
            </select>
          </label>
        </div>
        {rooms.length ? (
          <div
            className="od-table-scroll"
            tabIndex={0}
            role="region"
            aria-label="群聊每日事件量与响应指标"
          >
            <table className="od-table">
              <thead>
                <tr>
                  <th scope="col">群聊</th>
                  <th scope="col">消息总量</th>
                  <th scope="col">事件量</th>
                  {days.map((day) => (
                    <th className="od-day" scope="col" key={day}>
                      {day.slice(5)}
                    </th>
                  ))}
                  <th scope="col">无响应</th>
                  <th scope="col">超时率</th>
                  <th scope="col">首响 P50</th>
                </tr>
              </thead>
              <tbody>
                {rooms.map((r) => (
                  <tr key={r.key}>
                    <th scope="row">
                      <Link to={hrefWith({ room: r.key }, "/rooms")} title={r.label}>
                        {r.label}
                      </Link>
                      {r.failedDays > 0 ? (
                        <span
                          className="od-room-gap"
                          title={`${r.failedDays} 天抽取失败，事件统计不完整`}
                        >
                          缺 {r.failedDays} 天
                        </span>
                      ) : null}
                    </th>
                    <td className="od-num">{r.msgs === null ? "—" : formatInt(r.msgs)}</td>
                    <td className="od-num">{r.events === null ? "—" : formatInt(r.events)}</td>
                    {r.series.map((n, i) => (
                      <td className="od-day" key={days[i]}>
                        {n === null ? (
                          <span
                            className="od-missing"
                            title={`${r.label} · ${days[i]}：抽取失败或无记录，NULL 不是 0`}
                          >
                            —
                          </span>
                        ) : (
                          <Link
                            className="od-heat-cell"
                            data-level={n === 0 ? 0 : Math.min(4, Math.ceil((n / heatMax) * 4))}
                            aria-label={`${r.label} · ${days[i]}：${n} 起，查看明细`}
                            to={hrefWith(
                              { room: r.key, from: days[i] ?? null, to: days[i] ?? null },
                              "/detail",
                            )}
                          >
                            {n}
                          </Link>
                        )}
                      </td>
                    ))}
                    <td className="od-num">
                      <Link
                        data-tone={r.unreplied ? "risk" : undefined}
                        to={hrefWith(
                          { room: r.key, status: "unreplied", overdueOnly: null },
                          "/detail",
                        )}
                      >
                        {r.unreplied ?? "—"}
                      </Link>
                    </td>
                    <td className="od-num">
                      <Link
                        to={hrefWith({ room: r.key, overdueOnly: true, status: null }, "/detail")}
                        title={`商家发起 ${r.merchant ?? 0} 起${(r.merchant ?? 0) < 3 ? "，样本不足 3 起" : ""}`}
                      >
                        {formatPercent(r.overdueRate) ?? "—"}
                      </Link>
                    </td>
                    <td className="od-num">{formatDuration(r.p50) ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="od-empty">当前窗口没有群聊记录</p>
        )}
      </section>

      <div className="od-secondary">
        <section>
          <SectionHead
            title="事件类型分布"
            note={`一级分类 · ${cats.length} 类`}
            to={hrefWith({}, "/events")}
          />
          {cats.length ? <CategoryPie rows={cats} api={api} /> : null}
          {!cats.length ? (
            <p className="od-empty">
              {unavailable ? "事件抽取失败，暂无分类统计" : "当前窗口没有事件"}
            </p>
          ) : null}
        </section>
        <section>
          <SectionHead
            title="活跃客服"
            note={
              <>
                活跃量 / 首响归属
                <Info
                  text={`${METRIC.owned} 活跃量按事件去重，从 agents[] 统计，多人协作分别计入。`}
                />
              </>
            }
            to={hrefWith({}, "/agents")}
          />
          <div className="od-agents">
            {agentRows.map((a) => (
              <Link
                className="od-rank-row"
                key={a.key}
                to={hrefWith({ agent: a.key }, "/agents")}
                title={`${a.label}：活跃量 ${a.involved} 起，首响归属 ${a.owned} 起`}
              >
                <span>{a.label}</span>
                <b>{a.involved}</b>
                <small>/ {a.owned}</small>
                <div className="od-rank-track">
                  <i style={{ width: `${(a.involved / maxInvolved) * 100}%` }}>
                    <b style={{ width: `${a.involved ? (a.owned / a.involved) * 100 : 0}%` }} />
                  </i>
                </div>
              </Link>
            ))}
          </div>
          <p className="od-footnote">
            {agentRows.length
              ? "多人协作分别计入活跃量；无响应不归属首响客服。"
              : "当前窗口没有客服活跃记录"}
          </p>
        </section>
        <section className="od-attention">
          <SectionHead
            title="待跟进事件"
            note={<>无响应 {count(agg.unreplied)} 起 · 按等待时长排序</>}
            to={hrefWith({ status: "unreplied", overdueOnly: null }, "/detail")}
          />
          <div className="od-queue">
            {queue.map((e) => (
              <Link className="od-queue-row" key={e.id} to={hrefWith({ drawer: e.id })}>
                <span className="od-queue-summary" title={e.summary}>
                  {e.summary}
                </span>
                <span className="od-queue-meta">{roomLabel(e.roomid)}</span>
                <b>{formatDurationCompact(waitedSecFrom(lastDay, e.first_msg_time)) ?? "—"}</b>
              </Link>
            ))}
            {!queue.length ? (
              <p className="od-empty">
                {unavailable
                  ? "事件抽取失败，暂无法判断无响应情况"
                  : agg.merchant
                    ? "当前窗口的商家事件均已回复"
                    : "当前窗口没有商家发起事件"}
              </p>
            ) : null}
          </div>
          <p className="od-footnote">等待时长截至 {lastDay} 24:00</p>
        </section>
      </div>
      <EventDrawer
        event={queue.find((e) => e.id === api.filters.drawer)}
        analytics={analytics}
        missingId={api.filters.drawer}
        onClose={() => api.patch({ drawer: null })}
      />
    </main>
  );
}
