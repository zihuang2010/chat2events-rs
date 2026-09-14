/**
 * 数据源仲裁：**优先真接口**，探活失败才回落模拟数据，并把回落这件事常驻显示。
 * 绝不让模拟指标被当成真实统计。
 *
 * URL 上可强制：`?source=api` 只走真接口（失败即报错，不回落），`?source=mock` 只走模拟。
 */

import { buildTaxonomyIndex, type TaxonomyIndex } from "@/domain/metrics";
import type {
  AgentAgg,
  CategoryAgg,
  Dataset,
  EventRow,
  EventsPage,
  MessageRow,
  RoomAgg,
  SummaryRow,
} from "@/domain/schemas";
import {
  ApiError,
  fetchAgentAggs,
  fetchCategories,
  fetchDataset,
  fetchEvent,
  fetchEventsPage,
  fetchMessages,
  fetchRoomAggs,
  fetchSummary,
  probeMeta,
  type DatasetWindow,
  type EventSorting,
  type QueryFilters,
  type RawDataset,
} from "./client";
import {
  mockAgentAggs,
  mockCategories,
  mockEventsPage,
  mockRoomAggs,
  mockSummary,
} from "./mock/aggregate";
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

/**
 * ⚠️ **词表混版守卫搬到后端了**（`/api/dataset` 的那条 `EXISTS`，不一致直接 409）。
 * 这里再查一遍已经不可能 —— 事件明细不在这个响应里；而且明细现在是**一页一页翻**的，
 * 靠翻到的那一页去发现混版，翻不到的页就发现不了。
 */
function assemble(
  source: SourceKind,
  raw: RawDataset,
  fallbackReason: string | null,
): LoadedDataset {
  return {
    source,
    fallbackReason,
    taxIndex: buildTaxonomyIndex(raw.meta.taxonomy, raw.meta.taxonomy_version),
    loadedAt: Date.now(),
    meta: raw.meta,
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

/**
 * 聚合接口的数据源仲裁 —— 与 [`loadDataset`] 同一条规则：真接口优先，模拟只在回落时用。
 *
 * ⚠️ **模拟那一支不是「假数据」，它是同一口径的另一份实现**（`mock/aggregate`），
 * 用的就是搬进 SQL 之前的那几个纯函数。两边算出不同的数，说明有一边搬错了。
 */
async function mockContext() {
  const mock = await getMock();
  return { mock, taxIndex: buildTaxonomyIndex(mock.meta.taxonomy, mock.meta.taxonomy_version) };
}

export async function loadSummary(source: SourceKind, f: QueryFilters): Promise<SummaryRow> {
  if (source === "api") return fetchSummary(f);
  const { mock, taxIndex } = await mockContext();
  return mockSummary(mock.events, mock.groupDaily, taxIndex, f);
}

export async function loadRoomAggs(
  source: SourceKind,
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
): Promise<RoomAgg[]> {
  if (source === "api") return fetchRoomAggs(f, groups);
  const { mock, taxIndex } = await mockContext();
  return mockRoomAggs(mock.events, mock.groupDaily, taxIndex, f, groups);
}

export async function loadAgentAggs(source: SourceKind, f: QueryFilters): Promise<AgentAgg[]> {
  if (source === "api") return fetchAgentAggs(f);
  const { mock, taxIndex } = await mockContext();
  return mockAgentAggs(mock.events, mock.groupDaily, taxIndex, f);
}

export async function loadCategories(
  source: SourceKind,
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
): Promise<CategoryAgg[]> {
  if (source === "api") return fetchCategories(f, groups);
  const { mock, taxIndex } = await mockContext();
  return mockCategories(mock.events, mock.groupDaily, taxIndex, f, groups);
}

/**
 * 事件明细的一页 —— 行 ＋ 总数 ＋ 页数 ＋ 截断标志。
 *
 * ⚠️ 两个数据源在这一路上**都不按已知成功群日过滤**（见 `mock/aggregate` 的模块文档）：
 * 口径相反的时候 mock 下发现不了的问题，真接口上照样存在。
 */
export async function loadEventsPage(
  source: SourceKind,
  f: QueryFilters,
  page: number,
  pageSize: number,
  sorting: EventSorting = {},
): Promise<EventsPage> {
  if (source === "api") return fetchEventsPage(f, page, pageSize, sorting);
  const { mock, taxIndex } = await mockContext();
  return mockEventsPage(mock.events, mock.groupDaily, taxIndex, f, page, pageSize, sorting);
}

/**
 * 按 id 单独取一个事件 —— **深链接必须能打开**，哪怕它不在当前筛选、当前页、
 * 甚至当前日期范围里。别人把 `?drawer=123` 发给你，打不开就等于溯源断了。
 */
export async function loadEvent(
  source: SourceKind,
  eventId: number,
  taxonomyVersion: string,
): Promise<EventRow> {
  if (source === "api") return fetchEvent(eventId, taxonomyVersion);
  const { mock } = await mockContext();
  const event = mock.events.find((row) => row.id === eventId);
  if (!event) {
    // 形状要和真接口一致：那边「没有这一行」是 404，不是契约错误 ——
    // 抽屉靠这个区分「事件不存在」和「取数出问题」，两者对用户是两回事。
    throw new ApiError("找不到这个事件", {
      kind: "http",
      status: 404,
      path: `/event/${eventId}`,
      detail: "这个 ID 不在模拟数据里",
    });
  }
  return event;
}

export async function loadMessages(source: SourceKind, eventId: number): Promise<MessageRow[]> {
  if (source === "api") return fetchMessages(eventId);
  const msgs = (await getMock()).messages.get(eventId);
  if (!msgs) {
    throw new ApiError("取不到这些 msg_id", {
      kind: "contract",
      path: `/event/${eventId}/messages`,
      detail: "该事件早于原文留存（加 source_messages 列之前抽取的）",
    });
  }
  return msgs;
}
