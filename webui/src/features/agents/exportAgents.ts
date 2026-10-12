/**
 * 客服效能导出成 xlsx。版式与群导出相同（见 `features/export/workbook.ts`）；写出的是页面上
 * `agentRollup` 已经拼好的行，这里**不算任何指标**。
 *
 * 表头比页面写得全：页面上「本人首响」是合并表头、「商家发起」是首响归属下面的小字，
 * 拍平成单行后这层上下文就没了。尤其超时率 —— 群表那一列分母含无响应，这里不含，
 * 两个文件里同名就会被拿去直接比。
 *
 * 没有「数据完整性」列（按参考版式），缺口改由条件行那一句交代：抽取失败的群日不计入，
 * 数字偏低不等于工作量低（承重不变量 4）。
 *
 * 同一个文件里还有两张分类宽表「客服×一级分类」「客服×二级分类」，数据来自
 * `/api/agent-categories`（客服 × 二级分类的参与事件数）。口径是**参与**，与第一个 sheet
 * 的「参与事件数」同一个范围，所以每行各分类列 + 未归类 + 打标未完成等于它；多人参与的
 * 事件在每人那里各计一次，一列往下加大于事件量。分类部分见 `features/export/categoryPivot.ts`。
 */

import type { AgentRow } from "@/domain/metrics";
import type { AgentCategoryAgg, Meta } from "@/domain/schemas";
import { categoryPivot, type PivotView } from "@/features/export/categoryPivot";
import {
  duration,
  percent,
  summary,
  type SheetView,
  type WorkbookSheet,
} from "@/features/export/workbook";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { Filters } from "@/features/filters/useFilters";

/** 列宽，单位是字符；一个汉字约占两个。 */
const COLUMNS = [
  { header: "客服名称", width: 24 },
  { header: "活跃群", width: 8 },
  { header: "参与事件数", width: 12 },
  { header: "首响归属事件数", width: 16 },
  { header: "首响归属·商家发起", width: 20 },
  { header: "首响统计样本数", width: 16 },
  { header: "本人首响 P50", width: 14 },
  { header: "本人首响 P90", width: 14 },
  { header: "本人首响超时率", width: 16 },
];

export type AgentSheetView = SheetView & Pick<Analytics, "agentAliasIsAuthoritative">;

/** 分类宽表还要词表分组（列）与当前的分类筛选（范围外的列留空）。 */
export type AgentPivotView = AgentSheetView & PivotView;

/** 分类宽表固定的头部列；后面是 `categoryPivot` 给的分类列。 */
const PIVOT_HEAD = [
  { header: "客服名称", width: 24 },
  { header: "参与事件数", width: 12 },
];

// 与页面默认排序相同：参与事件数倒序。sort 是稳定的，同值保持页面上的先后。
const byInvolved = (rows: readonly AgentRow[]) => [...rows].sort((a, b) => b.involved - a.involved);

// 与页面「企微账号」标签同一个判据：回落成 easyUserId 本身时不标，权威姓名不标。
const agentName = (r: AgentRow, view: AgentSheetView) =>
  view.agentAliasIsAuthoritative(r.key) || r.label === r.key ? r.label : `${r.label}（企微账号）`;

/** 条件行里的行数说明，三张客服表共用。 */
function headcount(rows: readonly AgentRow[]): (string | false)[] {
  const incomplete = rows.filter((r) => r.failedCells > 0 || r.coverageUnknown).length;
  return [
    `共 ${rows.length} 人`,
    incomplete > 0 && `其中 ${incomplete} 人数据不完整，数字只含已知量`,
  ];
}

export function agentSheet(
  rows: readonly AgentRow[],
  view: AgentSheetView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): WorkbookSheet {
  return {
    name: "客服效能",
    caption: summary(headcount(rows), view, meta, filters, now),
    columns: COLUMNS,
    rows: byInvolved(rows).map((r) => [
      agentName(r, view),
      r.rooms,
      r.involved,
      r.owned,
      r.merchantOwned,
      r.replySamples,
      duration(r.p50),
      duration(r.p90),
      percent(r.overdueRate),
    ]),
  };
}

/**
 * 「客服×一级分类」/「客服×二级分类」宽表。一人一行，人与顺序同第一个 sheet（参与事件数倒序），
 * 三个 sheet 逐行对齐。没有整行留空的情况：参与事件数只数已知成功群日上的事件，不会是 null。
 */
export function agentPivotSheet(
  level: "level1" | "level2",
  rows: readonly AgentRow[],
  aggs: readonly AgentCategoryAgg[],
  view: AgentPivotView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): WorkbookSheet {
  const sorted = byInvolved(rows);
  const pivot = categoryPivot(
    level,
    sorted.map((r) => ({ key: r.key, label: agentName(r, view), total: r.involved })),
    new Map(aggs.map((agg) => [agg.agent, agg])),
    view,
    meta,
    { subject: "客服", total: "参与事件数" },
  );
  return {
    name: level === "level1" ? "客服×一级分类" : "客服×二级分类",
    caption: summary(
      [...headcount(rows), "多人参与的事件各计一次，按列相加大于事件量"],
      view,
      meta,
      filters,
      now,
    ),
    // 宽表有几十列：横向滚动时冻住首列「客服名称」。
    stickyColumns: 1,
    columns: [...PIVOT_HEAD, ...pivot.columns],
    rows: sorted.map((r) => [agentName(r, view), r.involved, ...pivot.cells.get(r.key)!]),
  };
}
