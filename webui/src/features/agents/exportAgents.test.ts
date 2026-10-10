import { expect, it } from "vitest";
import type { AgentRow } from "@/domain/metrics";
import { parseFilters } from "@/features/filters/useFilters";
import { buildMockDataset } from "@/test/mock/generator";
import { agentSheet, type AgentSheetView } from "./exportAgents";

const { meta } = buildMockDataset();
const view: AgentSheetView = {
  days: ["2026-10-01", "2026-10-02", "2026-10-03"],
  lastDay: "2026-10-03",
  roomLabel: (id) => `群${id}`,
  agentLabel: (id) => `客服${id}`,
  agentAliasIsAuthoritative: (id) => id === "a1",
};
// 2026-10-09 18:30 UTC+8
const now = new Date("2026-10-09T10:30:00Z");
const filters = parseFilters(new URLSearchParams());

function row(patch: Partial<AgentRow> & Pick<AgentRow, "key">): AgentRow {
  return {
    label: patch.key,
    roomIds: [],
    rooms: 0,
    involved: 0,
    owned: 0,
    merchantOwned: 0,
    replySamples: 0,
    p50: null,
    p90: null,
    overdue: 0,
    overdueRate: null,
    involvedSeries: [],
    ownedSeries: [],
    failedCells: 0,
    coverageUnknown: false,
    ...patch,
  };
}

it("writes summary, a blank spacer row, bold headers, then rows by involved count with gaps left empty", () => {
  const rows = [
    // 回落成 easyUserId 本身：不标企微账号
    row({ key: "a3", involved: 1 }),
    // 别名是企微账号，不是权威姓名
    row({ key: "a2", label: "zhangsan", involved: 4, rooms: 2, owned: 3 }),
    row({
      key: "a1",
      label: "李四",
      rooms: 3,
      involved: 9,
      owned: 6,
      merchantOwned: 5,
      replySamples: 4,
      p50: 90,
      p90: 3600,
      overdue: 1,
      overdueRate: 0.2,
    }),
  ];
  const sheet = agentSheet(rows, view, meta, filters, now);

  expect(sheet[1]).toEqual([]);
  expect(sheet[2]![0]).toEqual({ value: "客服名称", fontWeight: "bold" });
  expect(sheet[3]).toEqual([
    "李四",
    3,
    9,
    6,
    5,
    4,
    { value: 90 / 86_400, format: "[h]:mm:ss" },
    { value: 3600 / 86_400, format: "[h]:mm:ss" },
    { value: 0.2, format: "0.0%" },
  ]);
  // 没有首响样本：分位数与超时率是空格子，不是 0（承重不变量 4）
  expect(sheet[4]).toEqual(["zhangsan（企微账号）", 2, 4, 3, 0, 0, null, null, null]);
  expect(sheet[5]![0]).toBe("a3");
});

it("summary counts people and flags incomplete ones only when there are any", () => {
  expect(agentSheet([row({ key: "a1" })], view, meta, filters, now)[0]![0]).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 1 人｜导出于 2026-10-09 18:30",
  );

  const rows = [
    row({ key: "a1", failedCells: 2 }),
    row({ key: "a2", coverageUnknown: true }),
    row({ key: "a3" }),
  ];
  expect(agentSheet(rows, view, meta, filters, now)[0]![0]).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 3 人｜其中 2 人数据不完整，数字只含已知量｜导出于 2026-10-09 18:30",
  );
});
