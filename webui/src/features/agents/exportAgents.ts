/**
 * 客服效能导出成 xlsx。版式与群导出相同（理由见 `exportRooms.ts` 顶注）；写出的是页面上
 * `agentRollup` 已经拼好的行，这里**不算任何指标**。
 *
 * 表头比页面写得全：页面上「本人首响」是合并表头、「商家发起」是首响归属下面的小字，
 * 拍平成单行后这层上下文就没了。尤其超时率 —— 群表那一列分母含无响应，这里不含，
 * 两个文件里同名就会被拿去直接比。
 *
 * 没有「数据完整性」列（按参考版式），缺口改由条件行那一句交代：抽取失败的群日不计入，
 * 数字偏低不等于工作量低（承重不变量 4）。
 */

import type { SheetData } from "write-excel-file/browser";
import type { AgentRow } from "@/domain/metrics";
import type { Meta } from "@/domain/schemas";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { Filters } from "@/features/filters/useFilters";
import {
  downloadSheet,
  duration,
  percent,
  summary,
  type SheetView,
} from "@/features/rooms/exportRooms";

const HEADERS = [
  "客服名称",
  "活跃群",
  "参与事件数",
  "首响归属事件数",
  "首响归属·商家发起",
  "首响统计样本数",
  "本人首响 P50",
  "本人首响 P90",
  "本人首响超时率",
];
/** 列宽，单位是字符；一个汉字约占两个。 */
const WIDTHS = [24, 8, 12, 16, 20, 16, 14, 14, 16];

export type AgentSheetView = SheetView & Pick<Analytics, "agentAliasIsAuthoritative">;

export function agentSheet(
  rows: readonly AgentRow[],
  view: AgentSheetView,
  meta: Pick<Meta, "rooms" | "taxonomy" | "taxonomy_version">,
  filters: Filters,
  now: Date,
): SheetData {
  // 与页面默认排序相同：参与事件数倒序。sort 是稳定的，同值保持页面上的先后。
  const sorted = [...rows].sort((a, b) => b.involved - a.involved);
  const incomplete = rows.filter((r) => r.failedCells > 0 || r.coverageUnknown).length;
  // 与页面「企微账号」标签同一个判据：回落成 easyUserId 本身时不标，权威姓名不标。
  const name = (r: AgentRow) =>
    view.agentAliasIsAuthoritative(r.key) || r.label === r.key ? r.label : `${r.label}（企微账号）`;
  return [
    [
      summary(
        [
          `共 ${rows.length} 人`,
          incomplete > 0 && `其中 ${incomplete} 人数据不完整，数字只含已知量`,
        ],
        view,
        meta,
        filters,
        now,
      ),
    ],
    [],
    HEADERS.map((value) => ({ value, fontWeight: "bold" as const })),
    ...sorted.map((r) => [
      name(r),
      r.rooms,
      r.involved,
      r.owned,
      r.merchantOwned,
      r.replySamples,
      duration(r.p50),
      duration(r.p90),
      percent(r.overdueRate),
    ]),
  ];
}

export const downloadAgentSheet = (data: SheetData, view: SheetView) =>
  downloadSheet(data, view, "客服效能", WIDTHS);
