/**
 * 群维度指标导出成 xlsx。写出的是页面上 `roomRollup` 已经拼好的行，这里**不算任何指标**
 * —— 再算一遍就是第四份口径实现（CLAUDE.md「指标口径有三份实现」），分家了没有金标能发现。
 *
 * 版式：第 1 行导出条件，第 2 行空着，第 3 行表头。⚠️ 空行是有意的：Excel 排序会把与表头
 * 相连的非空行一并当成表格，摘要行贴着表头就会被认成表头、真表头被排进数据里。
 *
 * 缺失值（null）写空格子，绝不写 0 —— 承重不变量 4。
 *
 * 同一个文件里有第二个 sheet「群×事件类型」（长表，一行 = 群 × 二级类型），数据来自
 * `/api/room-categories`。它同样不算指标：每行的事件量就是接口给的数，范围与第一个 sheet
 * 同一组筛选，所以每个群各行相加（含「打标未完成」）等于第一个 sheet 的「事件量」。
 */

import type { SheetData } from "write-excel-file/browser";
import { EVENT_STATUS, SLA_OPTIONS } from "@/domain/definitions";
import {
  buildTaxonomyIndex,
  compareRoomEvents,
  level2Label,
  managerLabels,
  roomCoverageLabel,
  type RoomRow,
} from "@/domain/metrics";
import type { Meta, RoomCategoryAgg } from "@/domain/schemas";
import { formatDateTime } from "@/lib/format";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { Filters } from "@/features/filters/useFilters";

const HEADERS = [
  "群名称",
  "商家名称",
  "商家分组",
  "业务经理",
  "消息总量",
  "事件量",
  "商家发起",
  "首响 P50",
  "首响 P90",
  "无响应",
  "无响应率",
  "超时率",
  "数据完整性",
];
/** 列宽，单位是字符；一个汉字约占两个。 */
const WIDTHS = [30, 24, 14, 12, 10, 10, 10, 11, 11, 9, 10, 9, 30];

const TYPE_HEADERS = [
  "群名称",
  "商家名称",
  "商家分组",
  "业务经理",
  "一级分类",
  "二级类型",
  "事件量",
  "数据完整性",
];
const TYPE_WIDTHS = [30, 24, 14, 12, 14, 22, 10, 30];

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

/** 条件行与文件名用到的那部分页面状态，群导出与客服导出共用。 */
export type SheetView = Pick<Analytics, "days" | "lastDay" | "roomLabel" | "agentLabel">;

/** 页面实际展示的日期区间（URL 上的日期已被夹进已加载窗口）。 */
const span = (view: SheetView) => ({ from: view.days[0] ?? view.lastDay, to: view.lastDay });

/** 时长存成 Excel 的「天」，配 `[h]:mm:ss`：看着是时长，排序与计算仍是数值。 */
export const duration = (sec: number | null) =>
  sec === null ? null : { value: sec / 86_400, format: "[h]:mm:ss" };
export const percent = (v: number | null) => (v === null ? null : { value: v, format: "0.0%" });

export function roomSheet(
  rows: readonly RoomRow[],
  view: RoomSheetView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): SheetData {
  // 与页面默认排序相同：事件量倒序，缺失排最后。sort 是稳定的，同值保持页面上的先后。
  const sorted = [...rows].sort((a, b) => compareRoomEvents(b, a));
  return [
    [summary([`共 ${rows.length} 个群`], view, meta, filters, now)],
    [],
    HEADERS.map((value) => ({ value, fontWeight: "bold" as const })),
    ...sorted.map((r) => [
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
  ];
}

/**
 * 「群×事件类型」sheet。群的顺序与第一个 sheet 相同（事件量倒序），群内类型按事件量倒序。
 *
 * 一级 / 二级的显示名与事件明细（`decorate`）同一套：`key` 为 null 是「打标未完成」，
 * 词表里找不到的编码（`__untyped__` 以外的历史编码）一级落「未归类」、二级原样写编码。
 * 没有事件的类型不出行，所以「数据完整性」才是判断缺的是真 0 还是数据不全的依据。
 *
 * ⚠️ 整段抽取都失败的群（`events === null`）没有任何 (群, 类型) 行可写，但不能悄悄消失：
 * 写一行只有群信息和「N / N 日失败」的空行，类型与事件量留空（不是 0）。
 */
export function roomTypeSheet(
  rows: readonly RoomRow[],
  cells: readonly RoomCategoryAgg[],
  view: RoomSheetView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): SheetData {
  const tax = buildTaxonomyIndex(meta.taxonomy, meta.taxonomy_version);
  const cellsByRoom = new Map<string, RoomCategoryAgg[]>();
  for (const cell of cells) {
    const bucket = cellsByRoom.get(cell.roomid);
    if (bucket) bucket.push(cell);
    else cellsByRoom.set(cell.roomid, [cell]);
  }
  const body: SheetData = [];
  let roomsWritten = 0;
  for (const r of [...rows].sort((a, b) => compareRoomEvents(b, a))) {
    const identity = [
      r.label,
      view.roomMerchant(r.key),
      view.roomMerchantGroup(r.key),
      view.roomManager(r.key),
    ];
    const coverage = roomCoverageLabel(r).text;
    const typed = (cellsByRoom.get(r.key) ?? []).map((cell) => {
      const type = cell.key === null ? undefined : tax.get(cell.key);
      return {
        level1: cell.key === null ? "打标未完成" : (type?.parent_name ?? "未归类"),
        level2: type?.name ?? cell.key ?? "打标未完成",
        count: cell.count,
      };
    });
    if (typed.length === 0) {
      if (r.events === null) {
        body.push([...identity, null, null, null, coverage]);
        roomsWritten += 1;
      }
      continue;
    }
    typed.sort((a, b) => b.count - a.count || a.level2.localeCompare(b.level2, "zh"));
    for (const t of typed) body.push([...identity, t.level1, t.level2, t.count, coverage]);
    roomsWritten += 1;
  }
  return [
    [summary([`共 ${roomsWritten} 个群`], view, meta, filters, now)],
    [],
    TYPE_HEADERS.map((value) => ({ value, fontWeight: "bold" as const })),
    ...body,
  ];
}

/**
 * 文件离开页面之后，靠这一行说清它是在什么条件下导出的。日期与首响阈值总是写
 * （超时率随阈值变），其余条件只写生效的；导出时间是因为窗口末尾那一两天还没冻结，
 * 下一轮跑批可能重写，同一组条件隔天再导，数字可以不同。
 *
 * `scope` 是各自的行数说明（「共 N 个群」/「共 N 人」…），排在筛选条件之后、导出时间之前。
 */
export function summary(
  scope: readonly (string | false)[],
  view: SheetView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): string {
  const sla =
    SLA_OPTIONS.find((option) => option.value === filters.slaSec)?.label ?? `${filters.slaSec} 秒`;
  const query = filters.query.trim();
  const { from, to } = span(view);
  const parts = [
    `日期 ${from} ~ ${to}（${view.days.length} 天，UTC+8）`,
    `首响阈值 ${sla}`,
    filters.room && `群聊：${view.roomLabel(filters.room)}`,
    filters.agent && `客服：${view.agentLabel(filters.agent)}`,
    filters.merchantGroup && `商家分组：${filters.merchantGroup}`,
    filters.businessManager &&
      `业务经理：${managerLabels(meta.rooms).get(filters.businessManager) ?? filters.businessManager}`,
    query && `事件摘要：${query}`,
    filters.level1 && `一级分类：${filters.level1}`,
    filters.level2 && `二级分类：${level2Label(meta, filters.level2)}`,
    filters.status && `状态：${EVENT_STATUS[filters.status]}`,
    filters.overdueOnly !== null && `超时：${filters.overdueOnly ? "仅超时" : "仅未超时"}`,
    ...scope,
    `导出于 ${formatDateTime(now).slice(0, 16)}`,
  ];
  return parts.filter(Boolean).join("｜");
}

/** 一个文件两个 sheet：群维度指标 ＋ 群×事件类型。文件名沿用 `群维度指标_{起}_{止}.xlsx`。 */
export async function downloadRoomSheets(
  sheets: { metrics: SheetData; types: SheetData },
  view: SheetView,
): Promise<void> {
  // 点了才加载：写 xlsx 的库只有导出用得上，不进首屏。
  const { default: writeXlsxFile } = await import("write-excel-file/browser");
  const { from, to } = span(view);
  const sheet = (data: SheetData, name: string, widths: readonly number[]) => ({
    data,
    sheet: name,
    columns: widths.map((width) => ({ width })),
    stickyRowsCount: 3,
  });
  await writeXlsxFile([
    sheet(sheets.metrics, "群维度指标", WIDTHS),
    sheet(sheets.types, "群×事件类型", TYPE_WIDTHS),
  ]).toFile(`群维度指标_${from}_${to}.xlsx`);
}

/** sheet 名同时是文件名前缀：`{name}_{起}_{止}.xlsx`。 */
export async function downloadSheet(
  data: SheetData,
  view: SheetView,
  name: string,
  widths: readonly number[],
): Promise<void> {
  // 点了才加载：写 xlsx 的库只有导出用得上，不进首屏。
  const { default: writeXlsxFile } = await import("write-excel-file/browser");
  const { from, to } = span(view);
  await writeXlsxFile(data, {
    sheet: name,
    columns: widths.map((width) => ({ width })),
    stickyRowsCount: 3,
  }).toFile(`${name}_${from}_${to}.xlsx`);
}
