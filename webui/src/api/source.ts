/** 只读数据装载：接口错误直接交给页面报告。 */

import { buildTaxonomyIndex, type TaxonomyIndex } from "@/domain/metrics";
import type { Dataset } from "@/domain/schemas";
import { fetchDataset, type DatasetWindow } from "./client";

export {
  fetchAgentAggs as loadAgentAggs,
  fetchCategories as loadCategories,
  fetchEvent as loadEvent,
  fetchEventsPage as loadEventsPage,
  fetchMessages as loadMessages,
  fetchRoomAggs as loadRoomAggs,
  fetchSummary as loadSummary,
} from "./client";

export type SourceKind = "api";

export interface LoadedDataset extends Dataset {
  source: SourceKind;
  taxIndex: TaxonomyIndex;
  loadedAt: number;
}

/**
 * 词表混版由后端 `/api/dataset` 校验，冲突直接返回 409。
 *
 * **一趟。** 它的响应体自带完整 meta，所以不需要先探一次 `/api/meta`
 * （那个路由 2026-09-19 已删，理由见 `fetchDataset`）。
 */
export async function loadDataset(period: DatasetWindow = {}): Promise<LoadedDataset> {
  const raw = await fetchDataset(period);
  return {
    source: "api",
    taxIndex: buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version),
    loadedAt: Date.now(),
    meta: raw.meta,
    groupDaily: raw.groupDaily,
  };
}
