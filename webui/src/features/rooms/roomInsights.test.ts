import { describe, expect, it } from "vitest";
import * as echarts from "echarts";
import { buildMockDataset } from "@/api/mock/generator";
import type { LoadedDataset } from "@/api/source";
import { buildTaxonomyIndex, decorate } from "@/domain/metrics";
import { buildRoomCharts, buildRoomInsights } from "./roomInsights";

const raw = buildMockDataset();
const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy);
const dataset: LoadedDataset = {
  ...raw,
  events: decorate(raw.events, taxIndex),
  taxIndex,
  source: "mock",
  fallbackReason: null,
  loadedAt: 0,
};
const roomId = raw.meta.rooms[0]!.roomid;

describe("群聊近七天指标", () => {
  it("固定连续七个自然日，按群隔离，缺记录留空，成功且无事件是零", () => {
    const source: LoadedDataset = {
      ...dataset,
      meta: { ...dataset.meta, days: ["2026-08-29", "2026-08-31"] },
      groupDaily: [
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
      ],
      events: [],
    };
    const model = buildRoomInsights(source, roomId, 1800);
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
    const source = {
      ...dataset,
      groupDaily: dataset.groupDaily.map((row) => ({
        ...row,
        extraction_status: "failed" as const,
      })),
    };
    const model = buildRoomInsights(source, roomId, 1800);
    expect(model.msgs).toBeGreaterThan(0);
    expect(model.metrics).toBeNull();
    expect(model.events).toHaveLength(0);
    expect(model.categories.level1).toHaveLength(0);
    expect(model.daily.every((day) => day.metrics === null)).toBe(true);
  });

  it("区间首响从事件重算，排除平台发起，主类只统计一次", () => {
    const first = "2026-08-30";
    const last = "2026-08-31";
    const events = Array.from({ length: 10 }, (_, index) => ({
      ...dataset.events[0]!,
      id: index + 1,
      roomid: roomId,
      occurred_on: index < 9 ? first : last,
      asker_role: "EXTERNAL" as const,
      firstReplySec: index < 9 ? 60 : 6000,
      event_types: [dataset.events[0]!.event_type, "secondary"],
    }));
    const source: LoadedDataset = {
      ...dataset,
      events: [...events, { ...events[0]!, id: 99, asker_role: "INTERNAL", firstReplySec: 0 }],
      groupDaily: [first, last].map((dt) => ({
        ...dataset.groupDaily[0]!,
        roomid: roomId,
        dt,
        extraction_status: "ok",
      })),
    };
    const model = buildRoomInsights(source, roomId, 1800);
    expect(model.metrics).toMatchObject({
      events: 11,
      merchant: 10,
      push: 1,
      p50: 60,
      p90: 6000,
      overdue: 1,
      overdueRate: 0.1,
    });
    expect(model.categories.level1.reduce((sum, row) => sum + row.count, 0)).toBe(11);
    expect(model.categories.level2.reduce((sum, row) => sum + row.count, 0)).toBe(11);
  });

  it("四张图的配置可由真实 ECharts 渲染", () => {
    const model = buildRoomInsights(dataset, roomId, 1800);
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
