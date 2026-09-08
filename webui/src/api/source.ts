/**
 * 数据源仲裁：**优先真接口**，探活失败才回落模拟数据，并把回落这件事常驻显示。
 * 绝不让模拟指标被当成真实统计。
 *
 * URL 上可强制：`?source=api` 只走真接口（失败即报错，不回落），`?source=mock` 只走模拟。
 */

import { buildTaxonomyIndex, decorate, type TaxonomyIndex } from "@/domain/metrics";
import type { Dataset, MessageRow } from "@/domain/schemas";
import {
  ApiError,
  fetchDataset,
  fetchMessages,
  probeMeta,
  type RawDataset,
  type DatasetWindow,
} from "./client";
import type { MockDataset } from "./mock/generator";

export type SourceKind = "api" | "mock";
export type SourcePreference = SourceKind | "auto";

export interface LoadedDataset extends Dataset {
  source: SourceKind;
  /** 回落原因。真接口正常时为 null */
  fallbackReason: string | null;
  taxIndex: TaxonomyIndex;
  loadedAt: number;
}

let mockCache: Promise<MockDataset> | null = null;
const getMock = (): Promise<MockDataset> =>
  (mockCache ??= import("./mock/generator")
    .then(({ buildMockDataset }) => buildMockDataset())
    .catch((error: unknown) => {
      mockCache = null;
      throw error;
    }));

function assemble(
  source: SourceKind,
  raw: RawDataset,
  fallbackReason: string | null,
): LoadedDataset {
  const mismatch = raw.events.find(
    (event) =>
      event.taxonomy_version !== null && event.taxonomy_version !== raw.meta.taxonomy_version,
  );
  if (mismatch) {
    throw new ApiError("事件与词表版本不一致", {
      kind: "contract",
      path: "/events",
      detail: `事件 ${mismatch.id} 使用 ${mismatch.taxonomy_version}，当前词表为 ${raw.meta.taxonomy_version}；请完成重打标后重试`,
    });
  }
  const taxIndex = buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version);
  return {
    source,
    fallbackReason,
    taxIndex,
    loadedAt: Date.now(),
    meta: raw.meta,
    events: decorate(raw.events, taxIndex),
    groupDaily: raw.groupDaily,
  };
}

export async function loadDataset(
  forced: SourcePreference = "auto",
  period: DatasetWindow = {},
): Promise<LoadedDataset> {
  if (forced === "mock") {
    return assemble("mock", await getMock(), "URL 指定 source=mock");
  }
  let meta;
  try {
    meta = await probeMeta();
  } catch (err) {
    // 只有探活不可达才允许演示回落；契约或权限错误必须显式报告。
    if (
      forced === "api" ||
      !(err instanceof ApiError) ||
      err.kind === "contract" ||
      (err.kind === "http" && err.status !== 404 && err.status < 500)
    )
      throw err;
    const reason =
      err instanceof ApiError
        ? `真接口不可用（${err.userMessage}${err.detail ? `，${err.detail}` : ""}）`
        : "真接口不可用";
    return assemble("mock", await getMock(), reason);
  }
  return assemble("api", await fetchDataset(meta, period), null);
}

export async function loadMessages(source: SourceKind, eventId: number): Promise<MessageRow[]> {
  if (source === "api") return fetchMessages(eventId);
  const msgs = (await getMock()).messages.get(eventId);
  if (!msgs) {
    throw new ApiError("取不到这些 msg_id", {
      kind: "contract",
      path: `/event/${eventId}/messages`,
      detail: "可能已超出 raw 区保留期（raw_retention_months）",
    });
  }
  return msgs;
}
