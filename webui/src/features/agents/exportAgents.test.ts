import { expect, it } from "vitest";
import type { AgentRow } from "@/domain/metrics";
import type { AgentCategoryAgg } from "@/domain/schemas";
import type { WorkbookSheet } from "@/features/export/workbook";
import { parentGroups } from "@/features/filters/useAnalytics";
import { parseFilters } from "@/features/filters/useFilters";
import { buildMockDataset } from "@/test/mock/generator";
import { column, headers, record } from "@/test/workbook";
import { agentPivotSheet, agentSheet, type AgentSheetView } from "./exportAgents";

const raw = buildMockDataset();
const { meta } = raw;
const parents = parentGroups({ ...raw, taxIndex: new Map(), source: "api", loadedAt: 0 });
const baseView: AgentSheetView = {
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
  const sheet = agentSheet(rows, baseView, meta, filters, now);

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
  expect(agentSheet([row({ key: "a1" })], baseView, meta, filters, now).caption).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 1 人｜导出于 2026-10-09 18:30",
  );

  const rows = [
    row({ key: "a1", failedCells: 2 }),
    row({ key: "a2", coverageUnknown: true }),
    row({ key: "a3" }),
  ];
  expect(agentSheet(rows, baseView, meta, filters, now).caption).toBe(
    "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 3 人｜其中 2 人数据不完整，数字只含已知量｜导出于 2026-10-09 18:30",
  );
});

/** 宽表第 i 行里「参与事件数」之后的格子：各分类列 + 未归类 + 打标未完成。 */
const tallies = (sheet: WorkbookSheet, i: number) =>
  sheet.rows[i]!.slice(headers(sheet).indexOf("参与事件数") + 1);

it("pivot sheets keep the agent sheet order and add up to each person's involved count", () => {
  const rows = [
    row({ key: "a3", involved: 0 }),
    row({ key: "a2", label: "zhangsan", involved: 3, failedCells: 1 }),
    row({ key: "a1", label: "李四", involved: 5 }),
  ];
  // a1、a2 共同参与的那件催接单在两人那里各计一次
  const aggs: AgentCategoryAgg[] = [
    { agent: "a1", counts: { urge_accept: 2, tech_reassign: 1, __untyped__: 1 }, unlabeled: 1 },
    { agent: "a2", counts: { urge_accept: 1, retired_code: 2 }, unlabeled: 0 },
  ];
  const view = { ...baseView, parents, q: {} };
  const l1 = agentPivotSheet("level1", rows, aggs, view, meta, filters, now);
  const l2 = agentPivotSheet("level2", rows, aggs, view, meta, filters, now);

  expect(l1.name).toBe("客服×一级分类");
  expect(l2.name).toBe("客服×二级分类");
  for (const sheet of [l1, l2]) {
    expect(sheet.stickyColumns).toBe(1);
    expect(sheet.caption).toBe(
      "日期 2026-10-01 ~ 2026-10-03（3 天，UTC+8）｜首响阈值 30 分钟｜共 3 人｜其中 1 人数据不完整，数字只含已知量｜多人参与的事件各计一次，按列相加大于事件量｜导出于 2026-10-09 18:30",
    );
    expect(headers(sheet).slice(0, 2)).toEqual(["客服名称", "参与事件数"]);
    expect(headers(sheet).slice(-2)).toEqual(["未归类", "打标未完成"]);
    // 与第一个 sheet 同人同序同名
    expect(column(sheet, "客服名称")).toEqual(["李四", "zhangsan（企微账号）", "a3"]);
    for (const [i, involved] of [5, 3, 0].entries()) {
      expect((tallies(sheet, i) as number[]).reduce((a, b) => a + b, 0)).toBe(involved);
    }
  }
  expect(record(l1, 0)).toMatchObject({ 履约催促: 2, 师傅调度: 1, 未归类: 1, 打标未完成: 1 });
  expect(record(l1, 1)).toMatchObject({ 履约催促: 1, 师傅调度: 0, 未归类: 2, 打标未完成: 0 });
  expect(record(l2, 0)).toMatchObject({ 催接单: 2, 改派师傅: 1, 未归类: 1, 打标未完成: 1 });
});

it("pivot sheets refuse to export when the category data no longer matches the agent sheet", () => {
  const view = { ...baseView, parents, q: {} };
  const build = (rows: AgentRow[], aggs: AgentCategoryAgg[]) => () =>
    agentPivotSheet("level1", rows, aggs, view, meta, filters, now);

  expect(
    build(
      [row({ key: "a1", label: "李四", involved: 3 })],
      [{ agent: "a1", counts: { urge_accept: 2 }, unlabeled: 0 }],
    ),
  ).toThrow("数据刚有更新（客服「李四」分类合计 2，参与事件数 3），请刷新页面后重新导出");
  expect(
    build(
      [row({ key: "a1", involved: 0 })],
      [{ agent: "a9", counts: { urge_accept: 1 }, unlabeled: 0 }],
    ),
  ).toThrow("数据刚有更新（分类数据里多出 1 个客服），请刷新页面后重新导出");
});
