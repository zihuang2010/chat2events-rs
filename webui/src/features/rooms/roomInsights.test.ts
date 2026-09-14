/**
 * 群抽屉模型的测试。
 *
 * ⚠️ **指标不再由这里现算** —— 它们来自 `/api/summary?room=X` 与 `/api/categories?room=X`。
 * 所以输入用 `mock/aggregate`（口径的前端对照实现）从事件算出来，再喂给
 * `buildRoomInsights`：这样「口径」和「拼接」各自被测到，而不是混成一坨。
 */
import { describe, expect, it } from "vitest";
import * as echarts from "echarts";
import { mockCategories, mockSummary } from "@/api/mock/aggregate";
import { buildMockDataset } from "@/api/mock/generator";
import type { LoadedDataset } from "@/api/source";
import { buildTaxonomyIndex } from "@/domain/metrics";
import type { EventRow, GroupDailyRow } from "@/domain/schemas";
import { parentGroups } from "@/features/filters/useAnalytics";
import { buildRoomCharts, buildRoomInsights, roomInsightsWindow } from "./roomInsights";

const raw = buildMockDataset();
const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version);
const dataset: LoadedDataset = {
  meta: raw.meta,
  groupDaily: raw.groupDaily,
  taxIndex,
  source: "mock",
  fallbackReason: null,
  loadedAt: 0,
};
const roomId = raw.meta.rooms[0]!.roomid;

/** 把「接口那三份返回」按抽屉自己的七天窗口算出来，再交给待测函数。 */
function build(
  data: LoadedDataset,
  events: readonly EventRow[] = [],
  groupDaily: readonly GroupDailyRow[] = data.groupDaily,
) {
  const { from, to } = roomInsightsWindow(data);
  const scope = { from, to, room: roomId, slaSec: 1800 };
  const parents = parentGroups(data);
  const groups = parents.map((parent) => parent.types);
  return buildRoomInsights({
    dataset: data,
    roomId,
    slaSec: 1800,
    summary: mockSummary(events, groupDaily, data.taxIndex, scope),
    level1: mockCategories(events, groupDaily, data.taxIndex, scope, groups),
    level2: mockCategories(events, groupDaily, data.taxIndex, scope),
    tax: data.taxIndex,
    parents,
  });
}

it("keeps_fact_metrics_available_while_room_labels_are_pending", () => {
  const cell = raw.groupDaily.find(
    (row) => row.roomid === roomId && row.extraction_status === "ok",
  )!;
  const pending = [{ ...cell, classification_status: "pending" as const }];
  const model = build(
    { ...dataset, meta: { ...dataset.meta, days: [cell.dt] }, groupDaily: pending },
    [],
    pending,
  );
  expect(model.pendingLabels).toBe(1);
  expect(model.failedLabels).toBe(0);
  expect(model.daily.at(-1)!.classificationStatus).toBe("pending");
  expect(model.daily.at(-1)!.metrics).not.toBeNull();
  expect(model.categories.level1).toEqual([]);
});

describe("群聊近七天指标", () => {
  it("固定连续七个自然日，按群隔离，缺记录留空，成功且无事件是零", () => {
    const groupDaily: GroupDailyRow[] = [
      {
        ...dataset.groupDaily[0]!,
        roomid: roomId,
        dt: "2026-08-29",
        msg_count: 12,
        extraction_status: "ok",
      },
      {
        ...dataset.groupDaily[0]!,
        roomid: roomId,
        dt: "2026-08-31",
        msg_count: 24,
        extraction_status: "failed",
      },
      {
        ...dataset.groupDaily[0]!,
        roomid: "other",
        dt: "2026-08-29",
        msg_count: 900,
        extraction_status: "ok",
      },
    ];
    const source: LoadedDataset = {
      ...dataset,
      meta: { ...dataset.meta, days: ["2026-08-29", "2026-08-31"] },
      groupDaily,
    };
    const model = build(source, [], groupDaily);
    expect(model.days).toEqual([
      "2026-08-25",
      "2026-08-26",
      "2026-08-27",
      "2026-08-28",
      "2026-08-29",
      "2026-08-30",
      "2026-08-31",
    ]);
    expect(model.msgs).toBe(36);
    // 抽取成功但没有事件 = 真的 0；缺记录和抽取失败都必须是 null，不能伪装成 0。
    expect(model.daily[4]?.metrics?.events).toBe(0);
    expect(model.daily[5]).toMatchObject({ status: "missing", msgs: null, metrics: null });
    expect(model.daily[6]).toMatchObject({ status: "failed", msgs: 24, metrics: null });
    expect(model).toMatchObject({ failed: 1, missing: 5 });
    expect(buildRoomCharts(model, "level1").events).toMatchObject({
      series: [
        { data: [null, null, null, null, 0, null, null] },
        { data: [null, null, null, null, 0, null, null] },
      ],
    });
  });

  it("全部失败时消息量仍可用，事件、首响与分类不伪装成零", () => {
    const groupDaily = dataset.groupDaily.map((row) => ({
      ...row,
      extraction_status: "failed" as const,
    }));
    const model = build({ ...dataset, groupDaily }, raw.events, groupDaily);
    expect(model.msgs).toBeGreaterThan(0);
    expect(model.metrics).toBeNull();
    expect(model.categories.level1).toHaveLength(0);
    expect(model.daily.every((day) => day.metrics === null)).toBe(true);
  });

  it("区间首响从事件重算，排除平台发起，主类只统计一次", () => {
    const first = "2026-08-30";
    const last = "2026-08-31";
    // 首响走工作时段口径：09:00 → 09:01 是 60 秒，09:00 → 10:40 是 6000 秒。
    const at = (day: string, time: string) => `${day} ${time}`;
    const base = raw.events[0]!;
    const events: EventRow[] = Array.from({ length: 10 }, (_, index) => {
      const day = index < 9 ? first : last;
      return {
        ...base,
        id: index + 1,
        roomid: roomId,
        occurred_on: day,
        first_msg_time: at(day, "09:00:00"),
        last_msg_time: at(day, "11:00:00"),
        first_agent_reply_time: index < 9 ? at(day, "09:01:00") : at(day, "10:40:00"),
        asker_role: "EXTERNAL" as const,
        first_responder: "a1",
        agents: ["a1"],
        event_types: ["urge_visit"],
        event_type: "urge_visit",
        taxonomy_version: raw.meta.taxonomy_version,
      };
    });
    const push: EventRow = { ...events[0]!, id: 99, asker_role: "INTERNAL" };
    const groupDaily: GroupDailyRow[] = [first, last].map((dt) => ({
      ...dataset.groupDaily[0]!,
      roomid: roomId,
      dt,
      extraction_status: "ok",
    }));
    const model = build({ ...dataset, groupDaily }, [...events, push], groupDaily);
    expect(model.metrics).toMatchObject({
      events: 11,
      merchant: 10,
      push: 1,
      p50: 60,
      p90: 6000,
      overdue: 1,
      overdueRate: 0.1,
    });
    // 副类不进指标：合计恒等于事件数，否则一个事件会被计进 N 行。
    expect(model.categories.level1.reduce((sum, row) => sum + row.count, 0)).toBe(11);
    expect(model.categories.level2.reduce((sum, row) => sum + row.count, 0)).toBe(11);
  });

  it("四张图的配置可由真实 ECharts 渲染", () => {
    const model = build(dataset, raw.events);
    for (const option of Object.values(buildRoomCharts(model, "level1"))) {
      const chart = echarts.init(null, undefined, {
        renderer: "svg",
        ssr: true,
        width: 480,
        height: 240,
      });
      try {
        chart.setOption(option);
        expect(chart.renderToSVGString()).toContain("<path");
      } finally {
        chart.dispose();
      }
    }
  });
});
