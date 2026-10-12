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

/** 分类宽表固定的头尾列；中间是词表全量的分类列，个数随词表而定，每列同宽。 */
const PIVOT_HEAD = [
  { header: "群名称", width: 30 },
  { header: "商家名称", width: 24 },
  { header: "商家分组", width: 14 },
  { header: "业务经理", width: 12 },
  { header: "事件量", width: 10 },
];
const PIVOT_TAIL = [
  { header: "未归类", width: 9 },
  { header: "打标未完成", width: 11 },
  { header: "数据完整性", width: 30 },
];
const CATEGORY_WIDTH = 14;

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
export type RoomPivotView = RoomSheetView &
  Pick<Analytics, "parents"> & { q: Pick<Analytics["q"], "types" | "typesExclude"> };

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
 * 分类列取词表全量、顺序固定，换个日期范围再导列也不变，能直接拼表：一级按中文名
 * （`parentGroups` 的顺序），二级按所属一级聚在一起、组内按中文名。`unlabeled` 进
 * 「打标未完成」，`counts` 里的 `__untyped__` 与词表外的历史编码进「未归类」—— 两者绝不合并。
 *
 * 格子是事件量。留空有两种原因，都不能写 0（承重不变量 4）：
 * - 抽取全失败的群（`events === null`）：整行没算出来；
 * - 带了分类筛选时范围外的列：这次压根没计数（后端只要带分类筛选，就连打标未完成一起
 *   排掉，见 `web/params.rs`），写 0 会被读成「这个群没有这类事件」。
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
  const categories =
    level === "level1"
      ? view.parents.map((p) => ({ header: p.name, types: p.types }))
      : view.parents.flatMap((p) =>
          meta.taxonomy
            .filter((t) => t.parent_name === p.name)
            .map((t) => ({ header: t.name, types: [t.type_id] }))
            .sort((a, b) => a.header.localeCompare(b.header, "zh")),
        );
  const column = new Map(categories.flatMap((c, i) => c.types.map((id) => [id, i] as const)));
  const unclassified = categories.length;
  const unlabeled = categories.length + 1;

  // 与后端 `types` / `typesExclude` 那两段 WHERE 逐条同义（`web/params.rs`）：两者都给就取
  // 交集；任一分类筛选都连带排掉打标未完成。这个 key 的事件有没有被计数。
  const { types, typesExclude } = view.q;
  const counted = (key: string | null) =>
    (!types && !typesExclude) ||
    (key !== null &&
      (!types || types.includes(key)) &&
      (!typesExclude || !typesExclude.includes(key)));
  const inScope = [
    ...categories.map((c) => c.types.some(counted)),
    // 未归类 = 词表外的编码，列不出名单：只有 `types` 点名了词表外的编码才算在范围内。
    !types || types.some((key) => !column.has(key) && counted(key)),
    counted(null),
  ];

  const counts = new Map<string, number[]>();
  for (const agg of aggs) {
    const bucket = inScope.map(() => 0);
    for (const [type, n] of Object.entries(agg.counts))
      bucket[column.get(type) ?? unclassified]! += n;
    bucket[unlabeled] = agg.unlabeled;
    counts.set(agg.roomid, bucket);
  }
  // ⚠️ 两份数据来自两次请求：群指标是页面早先取的（最多缓存 5 分钟），分类计数是点导出时
  // 现取的。中间跑批落库或商家摘要刷新，就会写出「各分类相加 ≠ 事件量」的文件而毫无提示。
  // 数据一致时这个等式必然成立（同一组筛选、同一个范围），对不上就是取数之间数据变了。
  // 整行没算出来的群（`events === null`）两张表都留空，不比。
  for (const r of rows) {
    const tally = counts.get(r.key)?.reduce((a, b) => a + b, 0) ?? 0;
    if (r.events !== null && tally !== r.events) {
      throw new Error(
        `数据刚有更新（群「${r.label}」分类合计 ${tally}，事件量 ${r.events}），请刷新页面后重新导出`,
      );
    }
  }

  const sorted = [...rows].sort((a, b) => compareRoomEvents(b, a));
  return {
    name: level === "level1" ? "群×一级分类" : "群×二级分类",
    caption: summary([`共 ${rows.length} 个群`], view, meta, filters, now),
    // 宽表有几十列：横向滚动时冻住首列「群名称」。
    stickyColumns: 1,
    columns: [
      ...PIVOT_HEAD,
      ...categories.map((c) => ({ header: c.header, width: CATEGORY_WIDTH })),
      ...PIVOT_TAIL,
    ],
    rows: sorted.map((r) => [
      r.label,
      view.roomMerchant(r.key),
      view.roomMerchantGroup(r.key),
      view.roomManager(r.key),
      r.events,
      ...inScope.map((scoped, i) =>
        r.events === null || !scoped ? null : (counts.get(r.key)?.[i] ?? 0),
      ),
      roomCoverageLabel(r).text,
    ]),
  };
}
