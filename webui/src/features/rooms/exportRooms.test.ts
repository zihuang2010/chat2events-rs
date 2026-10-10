import { expect, it } from "vitest";
import type { RoomRow } from "@/domain/metrics";
import { parseFilters } from "@/features/filters/useFilters";
import { buildMockDataset } from "@/test/mock/generator";
import { roomSheet, roomTypeSheet, type RoomSheetView } from "./exportRooms";

const { meta } = buildMockDataset();
const view: RoomSheetView = {
  days: ["2026-10-01", "2026-10-02", "2026-10-03"],
  lastDay: "2026-10-03",
  roomLabel: (id) => `群${id}`,
  agentLabel: (id) => `客服${id}`,
  roomMerchant: (id) => (id === "r1" ? "一号店" : null),
  roomMerchantGroup: (id) => (id === "r1" ? "华东" : null),
  roomManager: (id) => (id === "r1" ? "张三" : null),
};
// 2026-10-09 18:30 UTC+8
const now = new Date("2026-10-09T10:30:00Z");

function row(patch: Partial<RoomRow> & Pick<RoomRow, "key">): RoomRow {
  return {
    label: `群${patch.key}`,
    events: 0,
    merchant: 0,
    unreplied: 0,
    unrepliedRate: null,
    p50: null,
    p90: null,
    overdue: 0,
    overdueRate: null,
    backlog: 0,
    msgs: null,
    senders: null,
    failedDays: 0,
    pendingLabels: 0,
    failedLabels: 0,
    missingDays: 0,
    unknownDays: 0,
    totalDays: 3,
    series: [],
    topLevel1: [],
    ...patch,
  };
}

it("writes summary, a blank spacer row, bold headers, then rows by event count with gaps left empty", () => {
  const rows = [
    row({ key: "r0", events: null, merchant: null, unreplied: null, failedDays: 3 }),
    row({ key: "r2", events: 2 }),
    row({
      key: "r1",
      events: 5,
      merchant: 4,
      unreplied: 1,
      unrepliedRate: 0.25,
      p50: 90,
      p90: 3600,
      overdueRate: 0.5,
      msgs: 120,
    }),
  ];
  const sheet = roomSheet(rows, view, meta, parseFilters(new URLSearchParams()), now);

  expect(sheet[1]).toEqual([]);
  expect(sheet[2]![0]).toEqual({ value: "群名称", fontWeight: "bold" });
  expect(sheet.slice(3).map((r) => r[0])).toEqual(["群r1", "群r2", "群r0"]);
  expect(sheet[3]).toEqual([
    "群r1",
    "一号店",
    "华东",
    "张三",
    120,
    5,
    4,
    { value: 90 / 86_400, format: "[h]:mm:ss" },
    { value: 3600 / 86_400, format: "[h]:mm:ss" },
    1,
    { value: 0.25, format: "0.0%" },
    { value: 0.5, format: "0.0%" },
    "3 日完整",
  ]);
  // 抽取整段失败：事件级是空格子，不是 0（承重不变量 4）
  expect(sheet[5]).toEqual([
    "群r0",
    null,
    null,
    null,
    null,
    null,
    null,
    null,
    null,
    null,
    null,
    null,
    "3 / 3 日失败",
  ]);
});

it("summary always states dates and threshold, and only the filters in effect", () => {
  const plain = roomSheet([], view, meta, parseFilters(new URLSearchParams()), now)[0]![0];
  expect(plain).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 0 个群｜导出于 2026-10-09 18:30",
  );

  const filtered = roomSheet(
    [],
    view,
    meta,
    parseFilters(
      new URLSearchParams("sla=3600&group=华东&q= 退款 &l2=urge_accept&status=unreplied&overdue=0"),
    ),
    now,
  )[0]![0];
  expect(filtered).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 1 小时｜商家分组：华东｜事件摘要：退款｜二级分类：履约催促 / 催接单｜状态：无响应｜超时：仅未超时｜共 0 个群｜导出于 2026-10-09 18:30",
  );
});

it("type sheet lists rooms by event count, types by count, and names null / untyped / unknown codes like the event table", () => {
  const rows = [
    row({ key: "r2", events: 2 }),
    row({
      key: "r1",
      events: 6,
      failedDays: 0,
      totalDays: 3,
    }),
  ];
  const cells = [
    { roomid: "r1", key: "urge_accept", count: 1 },
    { roomid: "r1", key: null, count: 2 },
    { roomid: "r1", key: "__untyped__", count: 1 },
    { roomid: "r1", key: "retired_code", count: 2 },
    { roomid: "r2", key: "urge_accept", count: 2 },
  ];
  const sheet = roomTypeSheet(rows, cells, view, meta, parseFilters(new URLSearchParams()), now);

  expect(sheet[1]).toEqual([]);
  expect(sheet[2]!.map((c) => (c as { value: string }).value)).toEqual([
    "群名称",
    "商家名称",
    "商家分组",
    "业务经理",
    "一级分类",
    "二级类型",
    "事件量",
    "数据完整性",
  ]);
  // 群按事件量倒序（r1 在前）；群内按事件量倒序，同量按二级名排，保证导出稳定
  expect(sheet.slice(3).map((r) => [r[0], r[4], r[5], r[6]])).toEqual([
    ["群r1", "打标未完成", "打标未完成", 2],
    ["群r1", "未归类", "retired_code", 2],
    ["群r1", "履约催促", "催接单", 1],
    ["群r1", "未归类", "归不上去", 1],
    ["群r2", "履约催促", "催接单", 2],
  ]);
  // 商家三列与第一个 sheet 同源，数据完整性同一份文案
  expect(sheet[3]!.slice(0, 4)).toEqual(["群r1", "一号店", "华东", "张三"]);
  expect(sheet[3]![7]).toBe("3 日完整");
  expect(sheet[0]![0]).toContain("共 2 个群");
});

it("type sheet keeps an all-failed room as a blank row instead of dropping it, and skips rooms with no events", () => {
  const rows = [
    row({ key: "r0", events: null, failedDays: 3 }),
    row({ key: "r3", events: 0 }),
    row({ key: "r1", events: 1 }),
  ];
  const cells = [{ roomid: "r1", key: "urge_accept", count: 1 }];
  const sheet = roomTypeSheet(rows, cells, view, meta, parseFilters(new URLSearchParams()), now);

  // r3 是真 0 个事件：没有任何类型可写，不出行；r0 抽取全失败：类型与事件量留空（不是 0）
  expect(sheet.slice(3)).toEqual([
    ["群r1", "一号店", "华东", "张三", "履约催促", "催接单", 1, "3 日完整"],
    ["群r0", null, null, null, null, null, null, "3 / 3 日失败"],
  ]);
});
