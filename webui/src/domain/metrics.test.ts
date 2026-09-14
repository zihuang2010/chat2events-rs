/**
 * 指标层的测试。**这里钉住的是口径，不是实现**：每一条都对应一个「错了会给出
 * 偏小但看起来正常的数字」的坑。改实现可以，改这些期望值必须先改口径文档。
 */

import { describe, expect, it } from "vitest";
import {
  aggregate,
  agentRollup,
  buildTaxonomyIndex,
  categoryRows,
  coverage,
  coverageLabel,
  dailyCounts,
  decorate,
  isBacklog,
  isOverdue,
  isUnreplied,
  quantile,
  roomRollup,
  statusOf,
} from "./metrics";
// 聚合口径已经搬进 SQL；`mock/aggregate` 是它在前端的对照实现，
// 也是这些测试的输入来源 —— 于是「拼接」和「口径」各自被测到。
import { mockAgentAggs, mockCategories, mockRoomAggs } from "@/api/mock/aggregate";
import type { EventRow, GroupDailyRow, TaxonomyType } from "./schemas";

const TAX: TaxonomyType[] = [
  { type_id: "urge_visit", parent_name: "履约催促", name: "催上门", description: "" },
  { type_id: "fee_refund", parent_name: "费用结算", name: "退款", description: "" },
];
const tax = buildTaxonomyIndex(TAX, "v1");

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
    last_msg_role: "EXTERNAL",
    followup_wait_max_sec: null,
    event_type: "urge_visit",
    event_types: ["urge_visit"],
    taxonomy_version: "v1",
    ...over,
  };
}
const dec = (rows: EventRow[]) => decorate(rows, tax);
/** 一级分类，**下标即 `groups` 下标** —— 与 `useAnalytics.parentGroups` 同一套排序。 */
const PARENTS = [
  { name: "履约催促", types: ["urge_visit"] },
  { name: "费用结算", types: ["fee_refund"] },
];

it("counts_pending_facts_without_publishing_them_as_a_category", () => {
  const raw = [ev({ event_type: null, event_types: null, taxonomy_version: null })];
  const rows = dec(raw);
  expect(rows[0]!.level1).toBe("打标未完成");
  expect(aggregate(rows, 1800, "2026-08-25").events).toBe(1);
  // 打标未完成的事件**整个排除在分类之外**，不能凑成一个「未归类」分类：
  // 那会把「还没算」显示成一个真实的业务类别。后端 `e.event_type IS NOT NULL` 同义。
  const cells: GroupDailyRow[] = [
    {
      corpid: "corp",
      roomid: "R1",
      dt: "2026-08-25",
      msg_count: 1,
      sender_count: 1,
      event_count: 1,
      merchant_event_count: 1,
      unreplied_count: 0,
      first_reply_p50_sec: 300,
      first_reply_p90_sec: 300,
      extraction_status: "ok",
      classification_status: "ok",
    },
  ];
  const window = { from: "2026-08-25", to: "2026-08-25", slaSec: 1800 };
  expect(mockCategories(raw, cells, tax, window)).toEqual([]);
  expect(
    mockCategories(
      raw,
      cells,
      tax,
      window,
      PARENTS.map((p) => p.types),
    ),
  ).toEqual([]);
});

describe("派生字段", () => {
  it("首响秒数按两个时间戳算；未回复保持 null，不折成 0", () => {
    const [replied, unreplied] = dec([
      ev(),
      ev({ first_agent_reply_time: null, agents: [], first_responder: null }),
    ]);
    expect(replied?.firstReplySec).toBe(300);
    expect(unreplied?.firstReplySec).toBeNull();
  });

  it("首响走工作时段口径，时段外的等待不计", () => {
    // 23:00 发问、次日 09:00 回：墙钟 36000 秒，工作时段只有次日 08:30→09:00。
    const [overnight] = dec([
      ev({
        first_msg_time: "2026-08-25 23:00:00",
        first_agent_reply_time: "2026-08-26 09:00:00",
      }),
    ]);
    expect(overnight?.firstReplySec).toBe(1800);
    // 整段都在打烊之后：0，不是负数也不是墙钟差。
    const [closed] = dec([
      ev({
        first_msg_time: "2026-08-25 21:30:00",
        first_agent_reply_time: "2026-08-25 23:00:00",
      }),
    ]);
    expect(closed?.firstReplySec).toBe(0);
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

  it("未回复计入超时分子：T+2 下未回复必然已超过任何阈值", () => {
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
      classification_status: "ok",
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
      classification_status: "failed",
    },
  ];

  const roster = gd.map((row) => ({ roomid: row.roomid, alias: null }));

  it("reports_label_progress_separately_from_extraction_coverage", () => {
    for (const status of ["pending", "failed"] as const) {
      const cov = coverage(
        [{ ...gd[0]!, classification_status: status }],
        new Set(["2026-08-25"]),
        "R1",
        roster,
      );
      expect(cov.known).toBe(1);
      expect(cov.failed).toBe(0);
      expect(cov.complete).toBe(false);
      expect(coverageLabel(cov)).toContain(status === "pending" ? "待打标" : "打标失败");
      // ⚠️ **打标未完成不进 `days` / `rooms`**：那两个说的是「事件计数不可信」，
      // 而事件数是抽取的产物，跟标签算没算完无关。混进去的话，一个群日 `pending`
      // 就能让 `EventTrends` 把那天所有分类的折线断成 null，而那里的解释文案只认
      // `cov.failed`（此时是 0）—— 图空一片、一个字解释都没有。
      expect(cov.days).toEqual([]);
      expect(cov.rooms).toEqual([]);
    }
  });

  it("统计失败格数与涉及的群、日", () => {
    const cov = coverage(gd, new Set(["2026-08-25"]), null, roster);
    expect(cov.complete).toBe(false);
    expect(cov.cells).toBe(2);
    expect(cov.failed).toBe(1);
    expect(cov.rooms).toEqual(["R2"]);
  });

  it("按群过滤后仍能判断该群自己完不完整", () => {
    expect(coverage(gd, new Set(["2026-08-25"]), "R1", roster).complete).toBe(true);
    expect(coverage(gd, new Set(["2026-08-25"]), "R2", roster).complete).toBe(false);
  });

  it("失败的群在消息级仍有数：msg_count 不依赖抽取", () => {
    expect(gd[1]?.msg_count).toBe(31);
    expect(gd[1]?.event_count).toBeNull();
  });

  it("缺记录不能被判为完整，也不能被计为抽取失败", () => {
    const cov = coverage([gd[0]!], new Set(["2026-08-25", "2026-08-26"]), "R1", roster);
    expect(cov.complete).toBe(false);
    expect(cov.failed).toBe(0);
    expect(cov.days).toContain("2026-08-26");
    expect(coverage([], new Set(["2026-08-25"]), null, roster).complete).toBe(false);
  });

  it("旧成功记录不能覆盖后续处理结果未知的状态", () => {
    const cov = coverage(
      [{ ...gd[0]!, freshness: "unknown" }],
      new Set(["2026-08-25"]),
      "R1",
      roster,
    );
    expect(cov).toMatchObject({ complete: false, known: 0, failed: 0, missing: 0, unknown: 1 });
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
      classification_status: "ok",
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
      classification_status: "failed",
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
      classification_status: "failed",
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
      classification_status: "failed",
    },
  ];
  const window = { from: days[0]!, to: days[days.length - 1]!, slaSec: 1800 };
  const roomEvents = [ev({ roomid: "R1" })];
  const rows = roomRollup({
    aggs: mockRoomAggs(roomEvents, gd, tax, window),
    groupDaily: gd,
    rooms,
    days,
    dayset: new Set(days),
    labelOf: (id) => rooms.find((r) => r.roomid === id)?.alias ?? id,
    parents: PARENTS,
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

  it("主要事件类型按数量降序，不补齐缺少的类别", () => {
    // 每类的条数不同，用来钉住降序；截前四由后端 `ROW_NUMBER` 做，
    // 这里的对照实现（`mockRoomAggs`）用同一条规则。
    const many = [
      ...Array.from({ length: 5 }, () =>
        ev({ roomid: "R1", event_type: "fee_refund", event_types: ["fee_refund"] }),
      ),
      ...Array.from({ length: 2 }, () => ev({ roomid: "R1" })),
    ];
    const ranked = roomRollup({
      aggs: mockRoomAggs(
        many,
        gd,
        tax,
        window,
        PARENTS.map((p) => p.types),
      ),
      groupDaily: gd,
      rooms,
      days,
      dayset: new Set(days),
      labelOf: (id) => id,
      parents: PARENTS,
      query: "",
    });
    expect(ranked.find((r) => r.key === "R1")?.topLevel1).toEqual([
      { name: "费用结算", count: 5 },
      { name: "履约催促", count: 2 },
    ]);
    expect(ranked.find((r) => r.key === "R2")?.topLevel1).toEqual([]);
  });
});

describe("客服维度：参与量与首响归属量是两个口径", () => {
  const days = ["2026-08-25"];
  const window = { from: days[0]!, to: days[0]!, slaSec: 1800 };
  /** 只有「本轮已知成功」的群日才进聚合 —— 与后端 `OK_DAYS` 同一条规矩。 */
  const okCell = (roomid: string, dt: string): GroupDailyRow => ({
    corpid: "corp",
    roomid,
    dt,
    msg_count: 2,
    sender_count: 2,
    event_count: 1,
    merchant_event_count: 1,
    unreplied_count: 0,
    first_reply_p50_sec: 300,
    first_reply_p90_sec: 300,
    extraction_status: "ok",
    classification_status: "ok",
  });
  const cells = [okCell("R1", days[0]!)];
  const raw = [
    ev({ agents: ["a1", "a2"], first_responder: "a1" }), // 两人协作，a1 首响
    ev({ agents: ["a2"], first_responder: "a2" }),
    ev({ first_agent_reply_time: null, agents: [], first_responder: null }), // 没人接
  ];
  const events = dec(raw);
  const rows = agentRollup({
    aggs: mockAgentAggs(raw, cells, tax, window),
    groupDaily: cells,
    days,
    dayset: new Set(days),
    labelOf: (a) => a,
    query: "",
  });

  /**
   * **别人的群不完整，不该把这个人的折线抹掉。**
   *
   * 此前的判据是「那天任一群不 ok」，而 `rotate_daily` 的 1000 群 / 每轮 400 意味着
   * **缺格是常态** —— 于是每个客服的两条折线全空、「完整性未知」恒真，
   * 那个保守选择退化成了「永远留空」。改成只看这个人参与过的群。
   */
  it.each(["ok", "failed", "missing"] as const)(
    "another_rooms_gap_does_not_blank_this_agents_series_%s",
    (status) => {
      const success = okCell("R1", days[0]!);
      const other: GroupDailyRow =
        status === "failed"
          ? {
              ...success,
              roomid: "R2",
              extraction_status: "failed",
              classification_status: "failed",
              event_count: null,
              merchant_event_count: null,
              unreplied_count: null,
              first_reply_p50_sec: null,
              first_reply_p90_sec: null,
            }
          : { ...success, roomid: "R2" };
      const gd = status === "missing" ? [success] : [success, other];
      const result = agentRollup({
        aggs: mockAgentAggs([ev()], gd, tax, window),
        groupDaily: gd,
        days,
        dayset: new Set(days),
        labelOf: (agent) => agent,
        query: "",
      });
      // 这个人只在 R1 —— R2 好不好跟他的参与量没关系。
      expect(result[0]).toMatchObject({
        roomIds: ["R1"],
        failedCells: 0,
        involved: 1,
        coverageUnknown: false,
        involvedSeries: [1],
        ownedSeries: [1],
      });
    },
  );

  /** 反过来：**自己的群**那天不完整，仍然必须留空 —— 那才是真的算不出来。 */
  it.each(["failed", "missing"] as const)(
    "a_gap_in_this_agents_own_room_still_blanks_that_day_%s",
    (status) => {
      const twoDays = ["2026-08-25", "2026-08-26"];
      const wide = { from: twoDays[0]!, to: twoDays[1]!, slaSec: 1800 };
      const good = okCell("R1", twoDays[0]!);
      const bad: GroupDailyRow = {
        ...good,
        dt: twoDays[1]!,
        extraction_status: "failed",
        classification_status: "failed",
        event_count: null,
        merchant_event_count: null,
        unreplied_count: null,
        first_reply_p50_sec: null,
        first_reply_p90_sec: null,
      };
      const gd = status === "missing" ? [good] : [good, bad];
      const result = agentRollup({
        aggs: mockAgentAggs([ev()], gd, tax, wide),
        groupDaily: gd,
        days: twoDays,
        dayset: new Set(twoDays),
        labelOf: (agent) => agent,
        query: "",
      });
      expect(result[0]).toMatchObject({
        roomIds: ["R1"],
        coverageUnknown: true,
        // 08-25 有事实照常出数，08-26 是自己群的缺口 —— 留空不是 0。
        involvedSeries: [1, null],
        ownedSeries: [1, null],
      });
    },
  );

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
    const mine = [
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
    ];
    const result = agentRollup({
      aggs: mockAgentAggs(mine, cells, tax, window),
      groupDaily: cells,
      days,
      dayset: new Set(days),
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
  const days = ["2026-08-25"];
  const window = { from: days[0]!, to: days[0]!, slaSec: 1800 };
  const cells: GroupDailyRow[] = [
    {
      corpid: "corp",
      roomid: "R1",
      dt: days[0]!,
      msg_count: 2,
      sender_count: 2,
      event_count: 2,
      merchant_event_count: 2,
      unreplied_count: 0,
      first_reply_p50_sec: 300,
      first_reply_p90_sec: 300,
      extraction_status: "ok",
      classification_status: "ok",
    },
  ];
  const raw = [
    ev({ event_type: "urge_visit", event_types: ["urge_visit", "fee_refund"] }), // 带副类
    ev({ event_type: "fee_refund", event_types: ["fee_refund"] }),
  ];

  it("副类不进指标，否则合计会大于事件数", () => {
    const l2 = categoryRows(mockCategories(raw, cells, tax, window), "level2", tax, PARENTS, 2);
    expect(l2.reduce((s, c) => s + c.count, 0)).toBe(2);
    expect(l2.find((c) => c.key === "fee_refund")?.count).toBe(1);
  });

  it("一级的 key 是分组下标，由前端换回父类名 —— 后端不认识词表", () => {
    const groups = PARENTS.map((parent) => parent.types);
    const l1 = categoryRows(
      mockCategories(raw, cells, tax, window, groups),
      "level1",
      tax,
      PARENTS,
      2,
    );
    expect(l1.map((c) => c.key).sort()).toEqual(["履约催促", "费用结算"]);
  });
});
