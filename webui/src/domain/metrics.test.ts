/**
 * 指标层的测试。**这里钉住的是口径，不是实现**：每一条都对应一个「错了会给出
 * 偏小但看起来正常的数字」的坑。改实现可以，改这些期望值必须先改口径文档。
 */

import { describe, expect, it } from "vitest";
import {
  aggregate,
  agentRollup,
  buildTaxonomyIndex,
  categoryRollup,
  coverage,
  dailyCounts,
  decorate,
  isBacklog,
  isOverdue,
  isUnreplied,
  quantile,
  roomRollup,
  statusOf,
} from "./metrics";
import type { EventRow, GroupDailyRow, TaxonomyType } from "./schemas";

const TAX: TaxonomyType[] = [
  { type_id: "urge_visit", parent_name: "履约催促", name: "催上门", description: "" },
  { type_id: "fee_refund", parent_name: "费用结算", name: "退款", description: "" },
];
const tax = buildTaxonomyIndex(TAX);

let seq = 0;
function ev(over: Partial<EventRow> = {}): EventRow {
  const id = ++seq;
  return {
    id,
    corpid: "corp",
    roomid: "R1",
    source_msg_ids: [`m${id}`],
    first_msg_time: "2026-08-25 10:00:00",
    last_msg_time: "2026-08-25 10:30:00",
    first_agent_reply_time: "2026-08-25 10:05:00",
    occurred_on: "2026-08-25",
    asker: "merchant1",
    asker_role: "EXTERNAL",
    agents: ["a1"],
    first_responder: "a1",
    summary: "客户已在家等候，要求尽快安排师傅上门。",
    event_type: "urge_visit",
    event_types: ["urge_visit"],
    taxonomy_version: "v1",
    ...over,
  };
}
const dec = (rows: EventRow[]) => decorate(rows, tax);

describe("派生字段", () => {
  it("首响秒数按两个时间戳算；未回复保持 null，不折成 0", () => {
    const [replied, unreplied] = dec([
      ev(),
      ev({ first_agent_reply_time: null, agents: [], first_responder: null }),
    ]);
    expect(replied?.firstReplySec).toBe(300);
    expect(unreplied?.firstReplySec).toBeNull();
  });

  it("跨天只按开始日归属，不在两天各算一次", () => {
    const [e] = dec([ev({ last_msg_time: "2026-08-27 09:00:00" })]);
    expect(e?.crossDay).toBe(true);
    expect(e?.occurred_on).toBe("2026-08-25");
    expect(dailyCounts([e!], ["2026-08-25", "2026-08-26", "2026-08-27"])).toEqual([1, 0, 0]);
  });

  it("归不上去的类型落到未归类，不是崩掉", () => {
    const [e] = dec([ev({ event_type: "__untyped__", event_types: ["__untyped__"] })]);
    expect(e?.level1).toBe("未归类");
    expect(e?.level2).toBe("归不上去");
  });
});

describe("分位数", () => {
  it("空集合返回 null，不返回 0", () => {
    expect(quantile([], 0.5)).toBeNull();
  });
  it("按升序取位", () => {
    expect(quantile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.5)).toBe(6);
    expect(quantile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.9)).toBe(10);
  });
});

describe("首响口径：分母是商家发起事件数", () => {
  const rows = dec([
    ev({ occurred_on: "2026-08-25" }), // 商家发起，5 分钟回复
    ev({ first_agent_reply_time: null, agents: [], first_responder: null }), // 未回复
    ev({
      asker_role: "INTERNAL", // 平台推送：首响恒 0 秒
      asker: "a2",
      agents: ["a2"],
      first_responder: "a2",
      first_agent_reply_time: "2026-08-25 10:00:00",
    }),
  ]);
  const agg = aggregate(rows, 1800, "2026-08-25");

  it("平台发起不进商家分母", () => {
    expect(agg.events).toBe(3);
    expect(agg.merchant).toBe(2);
    expect(agg.push).toBe(1);
  });

  it("未回复率的分母是商家发起数，不是事件总数", () => {
    expect(agg.unreplied).toBe(1);
    expect(agg.unrepliedRate).toBeCloseTo(1 / 2);
    expect(agg.unrepliedRate).not.toBeCloseTo(1 / 3);
  });

  it("平台推送的 0 秒不进分位数，否则会把 P50 拉到 0", () => {
    expect(agg.p50).toBe(300);
    expect(agg.p90).toBe(300);
  });

  it("未回复计入超时分子：T+1 下未回复必然已超过任何阈值", () => {
    expect(agg.overdue).toBe(1);
    expect(agg.overdueRate).toBeCloseTo(1 / 2);
  });
});

describe("状态判定", () => {
  const [replied, slow, unreplied, push] = dec([
    ev(),
    ev({ first_agent_reply_time: "2026-08-25 11:00:00" }),
    ev({ first_agent_reply_time: null, agents: [], first_responder: null }),
    ev({ asker_role: "INTERNAL", first_agent_reply_time: "2026-08-25 10:00:00" }),
  ]);

  it("四种状态互斥", () => {
    expect(statusOf(replied!, 1800)).toBe("replied");
    expect(statusOf(slow!, 1800)).toBe("overdue");
    expect(statusOf(unreplied!, 1800)).toBe("unreplied");
    expect(statusOf(push!, 1800)).toBe("push");
  });

  it("平台发起永远不算超时，也不算未回复", () => {
    expect(isOverdue(push!, 1)).toBe(false);
    expect(isUnreplied(push!)).toBe(false);
  });

  it("积压是前端派生：未回复且归属日早于窗口最后一天", () => {
    expect(isBacklog(unreplied!, "2026-08-29")).toBe(true);
    expect(isBacklog(unreplied!, "2026-08-25")).toBe(false);
  });
});

describe("覆盖度：失败的群日必须能被看见", () => {
  const gd: GroupDailyRow[] = [
    {
      corpid: "corp",
      roomid: "R1",
      dt: "2026-08-25",
      msg_count: 40,
      sender_count: 4,
      event_count: 6,
      merchant_event_count: 5,
      unreplied_count: 1,
      first_reply_p50_sec: 300,
      first_reply_p90_sec: 900,
      extraction_status: "ok",
    },
    {
      corpid: "corp",
      roomid: "R2",
      dt: "2026-08-25",
      msg_count: 31,
      sender_count: 3,
      event_count: null,
      merchant_event_count: null,
      unreplied_count: null,
      first_reply_p50_sec: null,
      first_reply_p90_sec: null,
      extraction_status: "failed",
    },
  ];

  it("统计失败格数与涉及的群、日", () => {
    const cov = coverage(gd, new Set(["2026-08-25"]), null);
    expect(cov.complete).toBe(false);
    expect(cov.cells).toBe(2);
    expect(cov.failed).toBe(1);
    expect(cov.rooms).toEqual(["R2"]);
  });

  it("按群过滤后仍能判断该群自己完不完整", () => {
    expect(coverage(gd, new Set(["2026-08-25"]), "R1").complete).toBe(true);
    expect(coverage(gd, new Set(["2026-08-25"]), "R2").complete).toBe(false);
  });

  it("失败的群在消息级仍有数：msg_count 不依赖抽取", () => {
    expect(gd[1]?.msg_count).toBe(31);
    expect(gd[1]?.event_count).toBeNull();
  });
});

describe("群维度汇总", () => {
  const rooms = [
    { roomid: "R1", alias: "甲群" },
    { roomid: "R2", alias: "乙群" },
  ];
  const days = ["2026-08-25", "2026-08-26"];
  const gd: GroupDailyRow[] = [
    {
      corpid: "c",
      roomid: "R1",
      dt: "2026-08-25",
      msg_count: 40,
      sender_count: 4,
      event_count: 1,
      merchant_event_count: 1,
      unreplied_count: 0,
      first_reply_p50_sec: 300,
      first_reply_p90_sec: 300,
      extraction_status: "ok",
    },
    {
      corpid: "c",
      roomid: "R1",
      dt: "2026-08-26",
      msg_count: 22,
      sender_count: 3,
      event_count: null,
      merchant_event_count: null,
      unreplied_count: null,
      first_reply_p50_sec: null,
      first_reply_p90_sec: null,
      extraction_status: "failed",
    },
    {
      corpid: "c",
      roomid: "R2",
      dt: "2026-08-25",
      msg_count: 9,
      sender_count: 2,
      event_count: null,
      merchant_event_count: null,
      unreplied_count: null,
      first_reply_p50_sec: null,
      first_reply_p90_sec: null,
      extraction_status: "failed",
    },
    {
      corpid: "c",
      roomid: "R2",
      dt: "2026-08-26",
      msg_count: 11,
      sender_count: 2,
      event_count: null,
      merchant_event_count: null,
      unreplied_count: null,
      first_reply_p50_sec: null,
      first_reply_p90_sec: null,
      extraction_status: "failed",
    },
  ];
  const rows = roomRollup({
    events: dec([ev({ roomid: "R1" })]),
    groupDaily: gd,
    rooms,
    days,
    dayset: new Set(days),
    slaSec: 1800,
    lastDay: "2026-08-26",
    labelOf: (id) => rooms.find((r) => r.roomid === id)?.alias ?? id,
    query: "",
  });

  it("整段都失败的群，事件数是 null 不是 0", () => {
    expect(rows.find((r) => r.key === "R2")?.events).toBeNull();
  });

  it("部分失败的群照常出数，并记下失败天数", () => {
    const r1 = rows.find((r) => r.key === "R1");
    expect(r1?.events).toBe(1);
    expect(r1?.failedDays).toBe(1);
    expect(r1?.totalDays).toBe(2);
  });

  it("每日序列里失败那天是 null，渲染时必须断成缺口而不是掉到 0", () => {
    expect(rows.find((r) => r.key === "R1")?.series).toEqual([1, null]);
  });

  it("消息级指标照常累加：它不依赖抽取", () => {
    expect(rows.find((r) => r.key === "R2")?.msgs).toBe(20);
  });

  it("主要事件类型按数量降序保留四类，不补齐缺少的类别", () => {
    const ranked = roomRollup({
      events: [1, 5, 3, 4, 2].flatMap((count, index) =>
        dec(Array.from({ length: count }, () => ev({ roomid: "R1" }))).map((event) => ({
          ...event,
          level1: `类别${index + 1}`,
        })),
      ),
      groupDaily: gd,
      rooms,
      days,
      dayset: new Set(days),
      slaSec: 1800,
      lastDay: "2026-08-26",
      labelOf: (id) => id,
      query: "",
    });
    expect(ranked.find((r) => r.key === "R1")?.topLevel1).toEqual([
      { name: "类别2", count: 5 },
      { name: "类别4", count: 4 },
      { name: "类别3", count: 3 },
      { name: "类别5", count: 2 },
    ]);
    expect(ranked.find((r) => r.key === "R2")?.topLevel1).toEqual([]);
    expect(rows.find((r) => r.key === "R1")?.topLevel1).toHaveLength(1);
  });
});

describe("客服维度：参与量与首响归属量是两个口径", () => {
  const days = ["2026-08-25"];
  const events = dec([
    ev({ agents: ["a1", "a2"], first_responder: "a1" }), // 两人协作，a1 首响
    ev({ agents: ["a2"], first_responder: "a2" }),
    ev({ first_agent_reply_time: null, agents: [], first_responder: null }), // 没人接
  ]);
  const rows = agentRollup({
    events,
    groupDaily: [],
    agents: [
      { agent: "a1", alias: "甲" },
      { agent: "a2", alias: "乙" },
    ],
    days,
    dayset: new Set(days),
    slaSec: 1800,
    labelOf: (a) => a,
    query: "",
  });

  it("多人协作各自计入参与量，所以各人相加会大于事件数", () => {
    const total = rows.reduce((s, r) => s + r.involved, 0);
    expect(total).toBe(3);
    expect(total).toBeGreaterThan(events.filter((e) => e.agents.length > 0).length);
  });

  it("首响归属只记一个人，没人接的单不落在任何人头上", () => {
    expect(rows.reduce((s, r) => s + r.owned, 0)).toBe(2);
    expect(rows.reduce((s, r) => s + r.owned, 0)).toBeLessThan(events.length);
  });

  it("个人样本与超时分母排除平台发起、协作和未归属事件", () => {
    const result = agentRollup({
      events: dec([
        ev(),
        ev({ first_agent_reply_time: "2026-08-25 11:00:00" }),
        ev({ asker_role: "INTERNAL", first_agent_reply_time: "2026-08-25 10:00:00" }),
        ev({ agents: ["a1", "a2"], first_responder: "a2" }),
        ev({ first_agent_reply_time: null, agents: [], first_responder: null }),
        ev({
          agents: ["a3"],
          first_responder: "a3",
          asker_role: "INTERNAL",
          first_agent_reply_time: "2026-08-25 10:00:00",
        }),
      ]),
      groupDaily: [],
      agents: ["a1", "a2", "a3"].map((agent) => ({ agent, alias: agent })),
      days,
      dayset: new Set(days),
      slaSec: 1800,
      labelOf: (agent) => agent,
      query: "",
    });
    expect(result.find((row) => row.key === "a1")).toMatchObject({
      involved: 4,
      owned: 3,
      merchantOwned: 2,
      replySamples: 2,
      overdue: 1,
      overdueRate: 0.5,
      p50: 3600,
    });
    expect(result.find((row) => row.key === "a3")).toMatchObject({
      owned: 1,
      merchantOwned: 0,
      replySamples: 0,
      overdueRate: null,
      p50: null,
      p90: null,
    });
  });
});

describe("分类汇总只按主类", () => {
  const events = dec([
    ev({ event_type: "urge_visit", event_types: ["urge_visit", "fee_refund"] }), // 带副类
    ev({ event_type: "fee_refund", event_types: ["fee_refund"] }),
  ]);

  it("副类不进指标，否则合计会大于事件数", () => {
    const l2 = categoryRollup(events, "level2", tax);
    expect(l2.reduce((s, c) => s + c.count, 0)).toBe(2);
    expect(l2.find((c) => c.key === "fee_refund")?.count).toBe(1);
  });

  it("一级由二级 JOIN 词表得到", () => {
    const l1 = categoryRollup(events, "level1", tax);
    expect(l1.map((c) => c.key).sort()).toEqual(["履约催促", "费用结算"]);
  });
});
