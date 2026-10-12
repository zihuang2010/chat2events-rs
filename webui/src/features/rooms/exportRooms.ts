/**
 * 群维度指标导出成 xlsx。写出的是页面上 `roomRollup` 已经拼好的行，这里**不算任何指标**
 * —— 再算一遍就是第四份口径实现（CLAUDE.md「指标口径有三份实现」），分家了没有金标能发现。
 *
 * 版式（导出条件 · 空行 · 表头）与写文件在 `features/export/workbook.ts`，这里只描述三个 sheet。
 *
 * 缺失值（null）写空格子，绝不写 0 —— 承重不变量 4。
 *
 * 同一个文件里还有两张分类宽表「群×一级分类」「群×二级分类」，数据来自
 * `/api/room-categories`（群 × 二级分类的事件数），一级按词表把二级相加。它们同样不算指标，
 * 只是换个形状：范围与第一个 sheet 同一组筛选，所以每行各分类列 + 未归类 + 打标未完成
 * 等于第一个 sheet 的「事件量」。
 */

import { compareRoomEvents, roomCoverageLabel, type RoomRow } from "@/domain/metrics";
import type { Meta, RoomCategoryAgg } from "@/domain/schemas";
import { categoryPivot, type PivotView } from "@/features/export/categoryPivot";
import { duration, percent, summary, type WorkbookSheet } from "@/features/export/workbook";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { Filters } from "@/features/filters/useFilters";

/** 列宽，单位是字符；一个汉字约占两个。 */
const COLUMNS = [
  { header: "群名称", width: 30 },
  { header: "商家名称", width: 24 },
  { header: "商家分组", width: 14 },
  { header: "业务经理", width: 12 },
  { header: "消息总量", width: 10 },
  { header: "事件量", width: 10 },
  { header: "商家发起", width: 10 },
  { header: "首响 P50", width: 11 },
  { header: "首响 P90", width: 11 },
  { header: "无响应", width: 9 },
  { header: "无响应率", width: 10 },
  { header: "超时率", width: 9 },
  { header: "数据完整性", width: 30 },
];

/** 分类宽表固定的头部列；后面是 `categoryPivot` 给的分类列，尾部再接「数据完整性」。 */
const PIVOT_HEAD = [
  { header: "群名称", width: 30 },
  { header: "商家名称", width: 24 },
  { header: "商家分组", width: 14 },
  { header: "业务经理", width: 12 },
  { header: "事件量", width: 10 },
];

export type RoomSheetView = Pick<
  Analytics,
  | "days"
  | "lastDay"
  | "roomLabel"
  | "agentLabel"
  | "roomMerchant"
  | "roomMerchantGroup"
  | "roomManager"
>;

/** 分类宽表还要词表分组（列）与当前的分类筛选（范围外的列留空）。 */
export type RoomPivotView = RoomSheetView & PivotView;

export function roomSheet(
  rows: readonly RoomRow[],
  view: RoomSheetView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): WorkbookSheet {
  // 与页面默认排序相同：事件量倒序，缺失排最后。sort 是稳定的，同值保持页面上的先后。
  const sorted = [...rows].sort((a, b) => compareRoomEvents(b, a));
  return {
    name: "群维度指标",
    caption: summary([`共 ${rows.length} 个群`], view, meta, filters, now),
    columns: COLUMNS,
    rows: sorted.map((r) => [
      r.label,
      view.roomMerchant(r.key),
      view.roomMerchantGroup(r.key),
      view.roomManager(r.key),
      r.msgs,
      r.events,
      r.merchant,
      duration(r.p50),
      duration(r.p90),
      r.unreplied,
      percent(r.unrepliedRate),
      percent(r.overdueRate),
      roomCoverageLabel(r).text,
    ]),
  };
}

/**
 * 「群×一级分类」/「群×二级分类」宽表。一个群一行，群与顺序同第一个 sheet（事件量倒序），
 * 0 事件、抽取全失败的群也各占一行 —— 三个 sheet 逐行对齐。
 *
 * 分类列、范围外留空、合计核对都在 `features/export/categoryPivot.ts`（与客服导出共用）；
 * 抽取全失败的群（`events === null`）整行留空、不核对。
 */
export function roomPivotSheet(
  level: "level1" | "level2",
  rows: readonly RoomRow[],
  aggs: readonly RoomCategoryAgg[],
  view: RoomPivotView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): WorkbookSheet {
  const sorted = [...rows].sort((a, b) => compareRoomEvents(b, a));
  const pivot = categoryPivot(
    level,
    sorted.map((r) => ({ key: r.key, label: r.label, total: r.events })),
    new Map(aggs.map((agg) => [agg.roomid, agg])),
    view,
    meta,
    { subject: "群", total: "事件量" },
  );
  return {
    name: level === "level1" ? "群×一级分类" : "群×二级分类",
    caption: summary([`共 ${rows.length} 个群`], view, meta, filters, now),
    // 宽表有几十列：横向滚动时冻住首列「群名称」。
    stickyColumns: 1,
    columns: [...PIVOT_HEAD, ...pivot.columns, { header: "数据完整性", width: 30 }],
    rows: sorted.map((r) => [
      r.label,
      view.roomMerchant(r.key),
      view.roomMerchantGroup(r.key),
      view.roomManager(r.key),
      r.events,
      ...pivot.cells.get(r.key)!,
      roomCoverageLabel(r).text,
    ]),
  };
}
