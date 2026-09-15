/**
 * 明细排序的口径测试。**钉的是「和 SQL 排得一样」**：
 * 分页是服务端做的，两边排序一旦分家，翻页会漏行或重复行，而页面上完全看不出来。
 *
 * 对应后端 `web::query::Paging::order_by`：`(expr) IS NULL, expr [ASC|DESC], e.id`。
 */
import { describe, expect, it } from "vitest";
import { mockEventsPage, mockSummary } from "./aggregate";
import { buildTaxonomyIndex } from "@/domain/metrics";
import type { EventRow, GroupDailyRow } from "@/domain/schemas";

const tax = buildTaxonomyIndex([], "v1");
const DAY = "2026-08-25";
const cells: GroupDailyRow[] = [
  {
    corpid: "c",
    roomid: "R1",
    dt: DAY,
    msg_count: 9,
    sender_count: 2,
    event_count: 3,
    merchant_event_count: 3,
    unreplied_count: 1,
    first_reply_p50_sec: 60,
    first_reply_p90_sec: 60,
    extraction_status: "ok",
    classification_status: "ok",
  },
];

/** 三条：09:10 回（600 秒）、09:01 回（60 秒）、没回（NULL）。 */
const events: EventRow[] = [
  ["09:10:00", 1],
  ["09:01:00", 2],
  [null, 3],
].map(([reply, id]) => ({
  id: id as number,
  corpid: "c",
  roomid: "R1",
  source_msg_ids: [`m${String(id)}`],
  first_msg_time: `${DAY} 09:00:00`,
  last_msg_time: `${DAY} 11:00:00`,
  first_agent_reply_time: reply === null ? null : `${DAY} ${reply as string}`,
  occurred_on: DAY,
  asker: "merchant1",
  asker_role: "EXTERNAL",
  agents: reply === null ? [] : ["a1"],
  first_responder: reply === null ? null : "a1",
  summary: `事件 ${String(id)}`,
  last_msg_role: "EXTERNAL",
  followup_wait_max_sec: null,
  event_type: null,
  taxonomy_version: null,
}));

const ids = (sorting: { sort?: string; dir?: "asc" | "desc" }) =>
  mockEventsPage(events, cells, tax, { from: DAY, to: DAY }, 1, 10, sorting).rows.map((e) => e.id);

describe("明细排序", () => {
  it("不给排序键时按归属日，id 收尾", () => {
    expect(ids({})).toEqual([1, 2, 3]);
  });

  it("按首响耗时升序：60 秒在 600 秒前", () => {
    expect(ids({ sort: "wait", dir: "asc" })).toEqual([2, 1, 3]);
  });

  /** 这条最容易错：NULL 是「没回复」不是「0 秒」，冒到升序头部会把
   *  「响应最快」那一屏变成一堆根本没人接的单。 */
  it("NULL 排在最后，降序也一样 —— 它是「没回复」，不是「很快」", () => {
    expect(ids({ sort: "wait", dir: "asc" }).at(-1)).toBe(3);
    expect(ids({ sort: "wait", dir: "desc" }).at(-1)).toBe(3);
    expect(ids({ sort: "reply", dir: "asc" }).at(-1)).toBe(3);
    expect(ids({ sort: "reply", dir: "desc" }).at(-1)).toBe(3);
  });

  it("值相同时按 id 定序 —— 排序键不唯一会让同一行在两页里都出现", () => {
    // 三条的 first_msg_time 完全相同，只能靠 id 定序。
    expect(ids({ sort: "time", dir: "asc" })).toEqual([1, 2, 3]);
    expect(ids({ sort: "time", dir: "desc" })).toEqual([1, 2, 3]);
  });

  it("分页在排序之后切，不是先切再排", () => {
    const page2 = mockEventsPage(events, cells, tax, { from: DAY, to: DAY }, 2, 1, {
      sort: "wait",
      dir: "asc",
    });
    expect(page2.rows.map((e) => e.id)).toEqual([1]);
  });

  it("总数与页数跟着明细自己那个集合走", () => {
    const page = mockEventsPage(events, cells, tax, { from: DAY, to: DAY }, 1, 2, {});
    expect(page.total).toBe(3);
    expect(page.pages).toBe(2);
    expect(page.truncated).toBe(false);
    // 越过页数返回空数组，不报错 —— 手改 URL 跳到不存在的页不是故障。
    expect(mockEventsPage(events, cells, tax, { from: DAY, to: DAY }, 9, 2, {}).rows).toEqual([]);
  });
});

/**
 * **这次缺口的形状**：明细表翻的是窗口内全部事件，概览只算已知成功群日上的事件。
 *
 * 模拟数据源此前在明细那一路也按已知成功群日过滤，于是 mock 下总数与行必然同集合、
 * 翻页永远夹不出问题 —— 真接口上却把人夹在更早的页码上，尾部的行永远翻不到。
 * 这条断言不需要数据库，正是为了让那件事在离线测试里就露出来。
 */
describe("抽取失败的群日", () => {
  /** R2 那天抽取失败：事件还在库里（核实用），但不能计进任何指标。 */
  const failedCell: GroupDailyRow = {
    corpid: "c",
    roomid: "R2",
    dt: DAY,
    msg_count: 4,
    sender_count: 2,
    event_count: null,
    merchant_event_count: null,
    unreplied_count: null,
    first_reply_p50_sec: null,
    first_reply_p90_sec: null,
    extraction_status: "failed",
    classification_status: "ok",
  };
  const strandedEvent: EventRow = {
    ...events[0]!,
    id: 99,
    roomid: "R2",
    summary: "抽取失败那天抽出来的事件",
  };
  const all = [...events, strandedEvent];
  const both = [...cells, failedCell];
  const window = { from: DAY, to: DAY };

  it("事件在明细里看得见", () => {
    const page = mockEventsPage(all, both, tax, window, 1, 10, {});
    expect(page.rows.map((e) => e.id)).toContain(99);
    expect(page.total).toBe(4);
  });

  it("同一个事件不进概览的事件总数", () => {
    expect(mockSummary(all, both, tax, window).events).toBe(3);
  });
});
