/**
 * 「×一级分类」「×二级分类」宽表的分类部分，群导出（`rooms/exportRooms.ts`）与客服导出
 * （`agents/exportAgents.ts`）共用：分类列、哪些列在计数范围内、把每行的 `counts` 归进各列、
 * 导出前的合计核对。头部列、行序、条件行由各自的 sheet 拼。
 *
 * 只收这四块，是因为它们是口径：范围判断必须与后端 `web/params.rs` 逐条同义，
 * 两份拷贝会悄悄分家，而没有测试能发现前端两份之间的分歧。
 *
 * 分类列取词表全量、顺序固定，换个日期范围再导列也不变，能直接拼表：一级按中文名
 * （`parentGroups` 的顺序），二级按所属一级聚在一起、组内按中文名。`unlabeled` 进
 * 「打标未完成」，`counts` 里的 `__untyped__` 与词表外的历史编码进「未归类」—— 两者绝不合并。
 *
 * 格子留空有两种原因，都不能写 0（承重不变量 4）：
 * - 整行没算出来（`total === null`，如抽取全失败的群）；
 * - 带了分类筛选时范围外的列：这次压根没计数（后端只要带分类筛选，就连打标未完成一起
 *   排掉，见 `web/params.rs`），写 0 会被读成「没有这类事件」。
 */

import type { Meta } from "@/domain/schemas";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { WorkbookSheet } from "./workbook";

/** 分类接口一行里与键无关的部分（`/api/room-categories` · `/api/agent-categories`）。 */
export interface CategoryCounts {
  counts: Record<string, number>;
  unlabeled: number;
}

/** 词表分组（列）与当前的分类筛选（范围外的列留空）。 */
export type PivotView = Pick<Analytics, "parents"> & {
  q: Pick<Analytics["q"], "types" | "typesExclude">;
};

/**
 * 宽表的一行：`total` 是这一行「各分类列 + 未归类 + 打标未完成」应当相加得到的数
 * （群是事件量，客服是参与事件数）；`null` 表示整行没算出来，格子全留空、不核对。
 */
export interface PivotRow {
  key: string;
  label: string;
  total: number | null;
}

const CATEGORY_WIDTH = 14;

/**
 * 返回分类列（含尾部「未归类」「打标未完成」）与每行的格子。`noun` 只用于报错文案：
 * `subject` 是「群」/「客服」，`total` 是合计的名字。
 *
 * ⚠️ 行（第一个 sheet 的数据）是页面早先取的（最多缓存 5 分钟），`aggs` 是点导出时现取的。
 * 中间跑批落库或商家摘要刷新，就会写出「各分类相加 ≠ 合计」的文件而毫无提示。
 * 数据一致时两条都必然成立（同一组筛选、同一个范围），不成立就是取数之间数据变了：
 * - 每行分类合计等于 `total`；
 * - `aggs` 里没有行之外的键 —— 两次取数之间冒出来的新群 / 新客服会被静默丢掉。
 */
export function categoryPivot(
  level: "level1" | "level2",
  rows: readonly PivotRow[],
  aggs: ReadonlyMap<string, CategoryCounts>,
  view: PivotView,
  meta: Pick<Meta, "taxonomy">,
  noun: { subject: string; total: string },
): { columns: WorkbookSheet["columns"]; cells: ReadonlyMap<string, (number | null)[]> } {
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

  const keys = new Set(rows.map((r) => r.key));
  const extra = [...aggs.keys()].filter((key) => !keys.has(key)).length;
  if (extra > 0) {
    throw new Error(
      `数据刚有更新（分类数据里多出 ${extra} 个${noun.subject}），请刷新页面后重新导出`,
    );
  }

  const cells = new Map<string, (number | null)[]>();
  for (const r of rows) {
    const bucket = inScope.map(() => 0);
    const agg = aggs.get(r.key);
    if (agg) {
      for (const [type, n] of Object.entries(agg.counts))
        bucket[column.get(type) ?? unclassified]! += n;
      bucket[unlabeled] = agg.unlabeled;
    }
    const tally = bucket.reduce((a, b) => a + b, 0);
    if (r.total !== null && tally !== r.total) {
      throw new Error(
        `数据刚有更新（${noun.subject}「${r.label}」分类合计 ${tally}，${noun.total} ${r.total}），请刷新页面后重新导出`,
      );
    }
    cells.set(
      r.key,
      inScope.map((scoped, i) => (r.total === null || !scoped ? null : bucket[i]!)),
    );
  }

  return {
    columns: [
      ...categories.map((c) => ({ header: c.header, width: CATEGORY_WIDTH })),
      { header: "未归类", width: 9 },
      { header: "打标未完成", width: 11 },
    ],
    cells,
  };
}
