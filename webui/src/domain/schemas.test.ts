import { expect, it } from "vitest";
import { buildMockDataset } from "@/api/mock/generator";
import { eventSchema, groupDailySchema, metaSchema } from "./schemas";

const raw = buildMockDataset();

it("accepts_pending_labels_and_rejects_partially_populated_label_columns", () => {
  const pending = {
    ...raw.events[0]!,
    event_type: null,
    event_types: null,
    taxonomy_version: null,
  };
  expect(eventSchema.safeParse(pending).success).toBe(true);
  for (const patch of [
    { event_type: "urge_visit" },
    { event_types: ["urge_visit"] },
    { taxonomy_version: "v1" },
  ]) {
    expect(eventSchema.safeParse({ ...pending, ...patch }).success).toBe(false);
  }
});

it("群元数据保留商家大整数 ID 与独立名称来源", () => {
  const rooms = [
    {
      roomid: "R",
      alias: "商家服务群",
      merchant_id: "18446744073709551615",
      alias_is_authoritative: true,
    },
    { roomid: "missing", alias: null, merchant_id: null, alias_is_authoritative: false },
  ];
  const meta = metaSchema.parse({ ...raw.meta, rooms });
  expect(meta.rooms).toEqual(rooms);
  expect(meta.alias_is_authoritative).toBe(false);
});

it("元数据拒绝无效、重复或倒序日期", () => {
  for (const days of [["2026-02-30"], ["2026-08-25", "2026-08-25"], ["2026-08-26", "2026-08-25"]]) {
    expect(metaSchema.safeParse({ ...raw.meta, days }).success).toBe(false);
  }
});

it("拒绝会扭曲首响、归属日或主分类的事件", () => {
  const event = raw.events.find((row) => row.first_responder !== null)!;
  for (const patch of [
    { first_msg_time: "2026-02-30 09:00:00" },
    { first_agent_reply_time: "2026-01-01 00:00:00" },
    { occurred_on: "2026-01-01" },
    { event_types: ["different"] },
    { first_responder: null },
    { agents: [] },
  ]) {
    expect(eventSchema.safeParse({ ...event, ...patch }).success).toBe(false);
  }
});

it("失败日不允许以 0 冒充 NULL，成功空日允许零事件", () => {
  const failed = raw.groupDaily.find((row) => row.extraction_status === "failed")!;
  expect(groupDailySchema.safeParse(failed).success).toBe(true);
  expect(groupDailySchema.safeParse({ ...failed, event_count: 0 }).success).toBe(false);
  expect(
    groupDailySchema.safeParse({
      ...failed,
      extraction_status: "ok",
      classification_status: "ok",
      event_count: 0,
      merchant_event_count: 0,
      unreplied_count: 0,
    }).success,
  ).toBe(true);
});
