import { expect, it } from "vitest";
import type { AgentRow } from "@/domain/metrics";
import { parseFilters } from "@/features/filters/useFilters";
import { buildMockDataset } from "@/test/mock/generator";
import { column, headers, record } from "@/test/workbook";
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

it("describes the agent sheet with rows by involved count and gaps left empty", () => {
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

  expect(sheet.name).toBe("客服效能");
  expect(sheet.stickyColumns).toBeUndefined();
  expect(headers(sheet)).toEqual([
    "客服名称",
    "活跃群",
    "参与事件数",
    "首响归属事件数",
    "首响归属·商家发起",
    "首响统计样本数",
    "本人首响 P50",
    "本人首响 P90",
    "本人首响超时率",
  ]);
  expect(record(sheet, 0)).toEqual({
    客服名称: "李四",
    活跃群: 3,
    参与事件数: 9,
    首响归属事件数: 6,
    "首响归属·商家发起": 5,
    首响统计样本数: 4,
    "本人首响 P50": { value: 90 / 86_400, format: "[h]:mm:ss" },
    "本人首响 P90": { value: 3600 / 86_400, format: "[h]:mm:ss" },
    本人首响超时率: { value: 0.2, format: "0.0%" },
  });
  // 没有首响样本：分位数与超时率是空格子，不是 0（承重不变量 4）
  expect(record(sheet, 1)).toEqual({
    客服名称: "zhangsan（企微账号）",
    活跃群: 2,
    参与事件数: 4,
    首响归属事件数: 3,
    "首响归属·商家发起": 0,
    首响统计样本数: 0,
    "本人首响 P50": null,
    "本人首响 P90": null,
    本人首响超时率: null,
  });
  expect(column(sheet, "客服名称")).toEqual(["李四", "zhangsan（企微账号）", "a3"]);
});

it("summary counts people and flags incomplete ones only when there are any", () => {
  expect(agentSheet([row({ key: "a1" })], view, meta, filters, now).caption).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 1 人｜导出于 2026-10-09 18:30",
  );

  const rows = [
    row({ key: "a1", failedCells: 2 }),
    row({ key: "a2", coverageUnknown: true }),
    row({ key: "a3" }),
  ];
  expect(agentSheet(rows, view, meta, filters, now).caption).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 3 人｜其中 2 人数据不完整，数字只含已知量｜导出于 2026-10-09 18:30",
  );
});
