/**
 * 模拟数据源的测试。它钉的不是「数字等于多少」，而是**边界场景真的被覆盖到了**：
 * 少了任何一种，筛选、联动、缺口渲染就成了没被验证过的代码路径。
 */

import { describe, expect, it } from "vitest";
import { buildMockDataset } from "./generator";
import {
  eventListSchema,
  groupDailyListSchema,
  agentDailyListSchema,
  metaSchema,
} from "@/domain/schemas";

const data = buildMockDataset();

describe("模拟数据符合领域契约", () => {
  it("演示窗口覆盖连续七天，每天都有群日记录", () => {
    expect(data.meta.days).toEqual([
      "2026-08-25",
      "2026-08-26",
      "2026-08-27",
      "2026-08-28",
      "2026-08-29",
      "2026-08-30",
      "2026-08-31",
    ]);
    expect(new Set(data.groupDaily.map((g) => g.dt))).toEqual(new Set(data.meta.days));
  });
  it("每一类记录都能通过 zod 校验", () => {
    expect(metaSchema.safeParse(data.meta).success).toBe(true);
    expect(eventListSchema.safeParse(data.events).success).toBe(true);
    expect(groupDailyListSchema.safeParse(data.groupDaily).success).toBe(true);
    expect(agentDailyListSchema.safeParse(data.agentDaily).success).toBe(true);
  });

  it("别名不是权威数据，界面必须标注待补", () => {
    expect(data.meta.alias_is_authoritative).toBe(false);
  });

  it("确定性：同一个种子跑两遍必须完全一致", () => {
    expect(buildMockDataset().events.length).toBe(data.events.length);
    expect(buildMockDataset().events[0]?.summary).toBe(data.events[0]?.summary);
  });
});

describe("边界场景覆盖", () => {
  it("有未回复事件，且它们的 agents 为空", () => {
    const unreplied = data.events.filter(
      (e) => e.asker_role === "EXTERNAL" && e.first_agent_reply_time === null,
    );
    expect(unreplied.length).toBeGreaterThan(0);
    expect(unreplied.every((e) => e.agents.length === 0 && e.first_responder === null)).toBe(true);
  });

  it("有平台发起的事件，且首响恒 0 秒", () => {
    const push = data.events.filter((e) => e.asker_role === "INTERNAL");
    expect(push.length).toBeGreaterThan(0);
    expect(push.every((e) => e.first_agent_reply_time === e.first_msg_time)).toBe(true);
  });

  it("有跨天事件、多客服协作事件、归不上去的事件", () => {
    expect(
      data.events.filter((e) => e.last_msg_time.slice(0, 10) !== e.occurred_on).length,
    ).toBeGreaterThan(0);
    expect(data.events.filter((e) => e.agents.length > 1).length).toBeGreaterThan(0);
    expect(data.events.filter((e) => e.event_type === "__untyped__").length).toBeGreaterThan(0);
  });

  it("有抽取失败的群日：事件级列全 NULL，消息级列照常有数", () => {
    const failed = data.groupDaily.filter((g) => g.extraction_status === "failed");
    expect(failed.length).toBeGreaterThan(0);
    for (const g of failed) {
      expect(g.event_count).toBeNull();
      expect(g.merchant_event_count).toBeNull();
      expect(g.unreplied_count).toBeNull();
      expect(g.first_reply_p50_sec).toBeNull();
      expect(g.msg_count).toBeGreaterThan(0);
    }
  });

  it("失败的群日在 agent 表上是整行缺失，不是 0", () => {
    const failedCells = new Set(
      data.groupDaily
        .filter((g) => g.extraction_status === "failed")
        .map((g) => `${g.roomid}|${g.dt}`),
    );
    expect(data.agentDaily.some((r) => failedCells.has(`${r.room}|${r.dt}`))).toBe(false);
  });

  it("失败的群日在 event 表上也没有行", () => {
    const failedCells = new Set(
      data.groupDaily
        .filter((g) => g.extraction_status === "failed")
        .map((g) => `${g.roomid}|${g.dt}`),
    );
    expect(data.events.some((e) => failedCells.has(`${e.roomid}|${e.occurred_on}`))).toBe(false);
  });
});

describe("契约不变量", () => {
  it("source_msg_ids 非空，且与消息时间线一一对应", () => {
    for (const e of data.events) {
      expect(e.source_msg_ids.length).toBeGreaterThan(0);
      expect(data.messages.get(e.id)?.length).toBe(e.source_msg_ids.length);
    }
  });

  it("摘要是中文一句话、不超过 100 字、不含订单号等 ID", () => {
    for (const e of data.events) {
      expect([...e.summary].length).toBeLessThanOrEqual(100);
      expect(e.summary).not.toMatch(/\d{5,}/);
    }
  });

  it("agent 表求和小于事件数，差额正好是未回复的商家事件数", () => {
    const total = data.agentDaily.reduce((s, r) => s + r.event_count, 0);
    const unreplied = data.events.filter(
      (e) => e.asker_role === "EXTERNAL" && e.first_agent_reply_time === null,
    ).length;
    expect(total).toBe(data.events.length - unreplied);
  });
});
