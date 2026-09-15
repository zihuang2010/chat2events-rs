import { ArrowRightOutlined } from "@ant-design/icons";
import { Drawer, Segmented, Tooltip } from "antd";
import { useCategories, useSummary } from "@/api/queries";
import type { CategoryAgg, SummaryRow } from "@/domain/schemas";
import { ErrorState, PageSkeleton } from "@/components/states";
import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";
import { EChart } from "@/components/charts/EChart";
import { METRIC } from "@/domain/definitions";
import { formatDuration, formatInt, formatPercent } from "@/lib/format";
import { buildRoomCharts, buildRoomInsights, roomInsightsWindow } from "./roomInsights";
import { WORKBENCH_THEME, cssVars } from "@/app/theme/workbench";

export function RoomInsightsDrawer({
  roomId,
  analytics,
  api,
  onClose,
}: {
  roomId: string | null;
  analytics: Analytics;
  api: FiltersApi;
  onClose: () => void;
}) {
  return (
    <Drawer
      open={roomId !== null}
      onClose={onClose}
      destroyOnHidden
      rootStyle={cssVars(WORKBENCH_THEME)}
      size="min(1120px, 100vw)"
      rootClassName="ri-drawer"
      title={roomId ? `${analytics.roomLabel(roomId)} · 近 7 天指标` : "群聊指标"}
    >
      {roomId ? (
        <RoomInsightsContent key={roomId} roomId={roomId} analytics={analytics} api={api} />
      ) : null}
    </Drawer>
  );
}

/**
 * 抽屉自己取数：**固定七天窗口 ＋ 只看这一个群**，与表格上的筛选条件无关
 * （所以不能复用页面那份 `analytics.q`，只借它的 `slaSec`）。
 */
function RoomInsightsContent({
  roomId,
  analytics,
  api,
}: {
  roomId: string;
  analytics: Analytics;
  api: FiltersApi;
}) {
  const source = analytics.dataset.source;
  const { from, to } = roomInsightsWindow(analytics.dataset);
  const scope = useMemo(
    () => ({ from, to, room: roomId, slaSec: analytics.slaSec }),
    [from, to, roomId, analytics.slaSec],
  );
  const groups = useMemo(
    () => analytics.parents.map((parent) => parent.types),
    [analytics.parents],
  );
  const summary = useSummary(source, scope);
  const level1 = useCategories(source, scope, groups);
  const level2 = useCategories(source, scope);
  const failed = [summary, level1, level2].find((query) => query.isError);
  if (failed?.error)
    return <ErrorState error={failed.error} onRetry={() => void failed.refetch()} />;
  if (!summary.data || !level1.data || !level2.data) return <PageSkeleton />;
  return (
    <RoomInsightsReady
      roomId={roomId}
      analytics={analytics}
      api={api}
      summary={summary.data}
      level1={level1.data}
      level2={level2.data}
    />
  );
}

function RoomInsightsReady({
  roomId,
  analytics,
  api,
  summary,
  level1,
  level2,
}: {
  roomId: string;
  analytics: Analytics;
  api: FiltersApi;
  summary: SummaryRow;
  level1: CategoryAgg[];
  level2: CategoryAgg[];
}) {
  const [level, setLevel] = useState<"level1" | "level2">("level1");
  const model = useMemo(
    () =>
      buildRoomInsights({
        dataset: analytics.dataset,
        roomId,
        slaSec: analytics.slaSec,
        summary,
        level1,
        level2,
        tax: analytics.taxIndex,
        parents: analytics.parents,
      }),
    [analytics, roomId, summary, level1, level2],
  );
  const charts = useMemo(() => buildRoomCharts(model, level), [model, level]);
  const count = (value: number | null | undefined) => (value == null ? "—" : formatInt(value));
  const categories = model.categories[level];
  const details = api.hrefWith(
    {
      room: roomId,
      from: model.from,
      to: model.to,
      agent: null,
      focusAgent: null,
      level1: null,
      level2: null,
      status: null,
      overdueOnly: null,
      query: "",
      drawer: null,
    },
    "/detail",
  );
  return (
    <div className="ri-content">
      <div className="od-drawer-context">
        <div>
          <strong>
            {model.from} 至 {model.to}
          </strong>
          <span>最近 7 天 · UTC+8 · 全部事件 · 首响阈值 {formatDuration(model.slaSec)}</span>
        </div>
        <Link className="od-link" to={details}>
          事件明细 <ArrowRightOutlined aria-hidden="true" />
        </Link>
      </div>
      <p className="od-drawer-id">
        {roomId}
        {roomId && !analytics.roomAliasIsAuthoritative(roomId) ? " · 群名待补" : ""}
      </p>
      {model.failed || model.missing || model.unknown ? (
        <p className="ri-coverage" role="status">
          {model.unknown ? `${model.unknown} 天最新处理结果未知 · ` : ""}
          {model.failed} 天抽取失败 · {model.missing}{" "}
          天无记录。事件统计仅含完整日期，缺失日期留空；失败日的消息总量仍保留。
        </p>
      ) : model.pendingLabels || model.failedLabels ? (
        <p className="ri-coverage" role="status">
          {model.pendingLabels} 天待打标 · {model.failedLabels}{" "}
          天打标失败。事实指标可用，分类统计未完成。
        </p>
      ) : (
        <p className="ri-complete">7 天数据完整</p>
      )}
      <div className="ri-charts">
        <section aria-labelledby="ri-messages-title">
          <div className="ri-section-head">
            <h3 id="ri-messages-title">消息总量</h3>
            <Tooltip title={METRIC.msgCount}>
              <span className="ri-total">
                {count(model.msgs)} <small>条</small>
              </span>
            </Tooltip>
          </div>
          <EChart option={charts.messages} height={240} ariaLabel="近7天每日消息总量与发言人数" />
          <p className="od-footnote">
            发言人数按日去重，不跨日相加；消息总量不受事件抽取成败影响。
          </p>
        </section>
        <section aria-labelledby="ri-events-title">
          <div className="ri-section-head">
            <h3 id="ri-events-title">事件量</h3>
            <span className="ri-total">
              {count(model.metrics?.events)} <small>起</small>
            </span>
          </div>
          <EChart option={charts.events} height={240} ariaLabel="近7天每日事件量与无响应事件数" />
          <p className="od-footnote">
            商家发起 {count(model.metrics?.merchant)} 起 · 平台发起 {count(model.metrics?.push)} 起
            · 无响应 {count(model.metrics?.unreplied)} 起
          </p>
        </section>
        <section aria-labelledby="ri-types-title">
          <div className="ri-section-head">
            <h3 id="ri-types-title">事件类型分布</h3>
            <Segmented<"level1" | "level2">
              aria-label="分类层级"
              value={level}
              options={[
                { label: "一级分类", value: "level1" },
                { label: "二级类型", value: "level2" },
              ]}
              onChange={(value) => setLevel(value)}
            />
          </div>
          {categories.length ? (
            <div
              className="ri-category-scroll"
              role="region"
              aria-label="事件类型分布图"
              tabIndex={0}
            >
              <EChart
                option={charts.classification}
                height={Math.max(240, categories.length * 28 + 36)}
                ariaLabel={`近7天${level === "level1" ? "一级分类" : "二级类型"}事件数量分布`}
              />
            </div>
          ) : (
            <div className="ri-chart-empty">
              {model.metrics ? "暂无事件类型数据" : "事件抽取数据不可用"}
            </div>
          )}
          <p className="od-footnote">一个事件只有一个分类，合计等于事件数。</p>
        </section>
        <section aria-labelledby="ri-response-title">
          <div className="ri-section-head">
            <h3 id="ri-response-title">首响指标</h3>
            <Tooltip title={METRIC.p50}>
              <span className="ri-total">
                {formatDuration(model.metrics?.p50 ?? null) ?? "—"} <small>P50</small>
              </span>
            </Tooltip>
          </div>
          <EChart
            option={charts.response}
            height={240}
            ariaLabel="近7天每日首响P50与P90，单位分钟"
          />
          <p className="od-footnote">
            区间 P90 {formatDuration(model.metrics?.p90 ?? null) ?? "—"} · 超时率{" "}
            {formatPercent(model.metrics?.overdueRate ?? null) ?? "—"} · 已回复商家事件{" "}
            {count(model.metrics?.replied)} 起
          </p>
        </section>
      </div>
      <details className="ri-daily-details">
        <summary>每日指标数据</summary>
        <div className="od-table-scroll">
          <table className="od-table">
            <thead>
              <tr>
                <th>日期</th>
                <th>消息总量</th>
                <th>发言人数</th>
                <th>事件量</th>
                <th>无响应</th>
                <th>首响 P50</th>
                <th>首响 P90</th>
                <th>超时率</th>
                <th>数据状态</th>
              </tr>
            </thead>
            <tbody>
              {model.daily.map((day) => (
                <tr key={day.day}>
                  <th scope="row">{day.day}</th>
                  <td>{count(day.msgs)}</td>
                  <td>{count(day.senders)}</td>
                  <td>{count(day.metrics?.events)}</td>
                  <td>{count(day.metrics?.unreplied)}</td>
                  <td>{formatDuration(day.metrics?.p50 ?? null) ?? "—"}</td>
                  <td>{formatDuration(day.metrics?.p90 ?? null) ?? "—"}</td>
                  <td>{formatPercent(day.metrics?.overdueRate ?? null) ?? "—"}</td>
                  <td>
                    {day.status === "ok"
                      ? day.classificationStatus === "pending"
                        ? "待打标"
                        : day.classificationStatus === "failed"
                          ? "打标失败"
                          : "完整"
                      : day.status === "failed"
                        ? "抽取失败"
                        : day.status === "unknown"
                          ? "最新结果未知"
                          : "无记录"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </div>
  );
}
