import { expect, it } from "vitest";
import { buildMockDataset } from "@/test/mock/generator";
import { eventSchema, groupDailySchema, metaSchema } from "./schemas";

const raw = buildMockDataset();

it("accepts_pending_labels_and_rejects_partially_populated_label_columns", () => {
  const pending = {
    ...raw.events[0]!,
    event_type: null,
    taxonomy_version: null,
  };
  expect(eventSchema.safeParse(pending).success).toBe(true);
  for (const patch of [{ event_type: "urge_visit" }, { taxonomy_version: "v1" }]) {
    expect(eventSchema.safeParse({ ...pending, ...patch }).success).toBe(false);
  }
});

it("群元数据保留商家与经理的大整数 ID，并带出商家分组与经理姓名", () => {
  const rooms = [
    {
      roomid: "R",
      alias: "商家服务群",
      merchant_id: "9223372036854775807",
      alias_is_authoritative: true,
      merchant_name: "极限商家",
      merchant_name_is_authoritative: true,
      merchant_group_config_name: "华东组",
      // 经理编号同样是 BIGINT，超出 JS 安全整数，必须以字符串保留。
      business_manager_id: "9007199254740993",
      business_manager_name: "李经理",
    },
    { roomid: "missing", alias: null, merchant_id: null, alias_is_authoritative: false },
    // 有编号查不到姓名 / 商家没配分组：三个新字段都允许 null。
    {
      roomid: "unnamed",
      alias: "无名经理的群",
      merchant_id: "7",
      merchant_name: null,
      merchant_group_config_name: null,
      business_manager_id: "42",
      business_manager_name: null,
    },
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
    // 有分类却没词表版本 —— 标签两列必须同生同死，否则页面会显示一个无从解释的分类
    { taxonomy_version: null },
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
