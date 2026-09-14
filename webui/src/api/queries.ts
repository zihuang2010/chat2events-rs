/** 数据获取的 React 绑定。缓存、重试、失效全部交给 TanStack Query，不自己造。 */

import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { useLocation, useSearchParams } from "react-router-dom";
import {
  loadAgentAggs,
  loadCategories,
  loadDataset,
  loadEvent,
  loadEventsPage,
  loadMessages,
  loadRoomAggs,
  loadSummary,
  type LoadedDataset,
  type SourceKind,
  type SourcePreference,
} from "./source";
import type { MessageRow } from "@/domain/schemas";
import type { EventSorting, QueryFilters } from "./client";

/**
 * 抽屉里那一个事件。**按 id 单独取**，不依赖它在不在当前页、当前筛选里 ——
 * 明细现在是一页一页翻的，靠「在已加载的数据里找」等于把深链接废掉。
 */
export function useStoredEvent(
  source: SourceKind,
  eventId: number | null,
  taxonomyVersion: string,
) {
  return useQuery({
    queryKey: ["event", source, eventId, taxonomyVersion],
    queryFn: () => {
      if (eventId === null) throw new Error("缺少事件 ID");
      return loadEvent(source, eventId, taxonomyVersion);
    },
    enabled: eventId !== null,
    staleTime: 5 * 60_000,
    retry: false,
  });
}

export function useDataset(): UseQueryResult<LoadedDataset, Error> {
  const [params] = useSearchParams();
  const { pathname } = useLocation();
  const value = params.get("source");
  const source: SourcePreference = value === "api" || value === "mock" ? value : "auto";
  const overview = pathname === "/" || pathname.endsWith("/overview");
  return useWindowDataset(
    source,
    overview ? null : params.get("from"),
    overview ? null : params.get("to"),
  );
}

/** 主视图按 URL 范围取数；群抽屉用默认七天，共用同一份查询缓存。 */
export function useWindowDataset(
  source: SourcePreference,
  from: string | null = null,
  to: string | null = null,
  enabled = true,
): UseQueryResult<LoadedDataset, Error> {
  return useQuery({
    queryKey: ["dataset", source, from, to],
    queryFn: () => loadDataset(source, { from, to }),
    enabled,
    // 每日跑批，一天只换一次数据。窗口重新聚焦时不该再打一轮接口。
    staleTime: 5 * 60_000,
    gcTime: 30 * 60_000,
    refetchOnWindowFocus: false,
    retry: 1,
  });
}

export function useEventMessages(
  source: SourceKind | undefined,
  eventId: number | null,
): UseQueryResult<MessageRow[], Error> {
  return useQuery({
    queryKey: ["messages", source, eventId],
    queryFn: () => {
      if (source === undefined || eventId === null) throw new Error("消息查询缺少数据源或事件 ID");
      return loadMessages(source, eventId);
    },
    enabled: source !== undefined && eventId !== null,
    staleTime: 10 * 60_000,
    retry: 1,
  });
}

/**
 * 聚合接口的缓存策略 —— 与 `useWindowDataset` 一致：**每日跑批，一天只换一次数据**，
 * 重新聚焦窗口不该再打一轮接口。
 *
 * ⚠️ **筛选条件进 queryKey**，所以点回上一个筛选是瞬时的（命中缓存），
 * 只有第一次出现的组合才等服务器。这是「筛选下推」之后交互延迟的主要缓解手段。
 */
const AGG_CACHE = {
  staleTime: 5 * 60_000,
  gcTime: 30 * 60_000,
  refetchOnWindowFocus: false,
  retry: 1,
} as const;

/** 概览 KPI ＋ 按天趋势。取代前端从全量明细现算的那条路。 */
export function useSummary(source: SourceKind | undefined, f: QueryFilters) {
  return useQuery({
    queryKey: ["summary", source, f],
    queryFn: () => loadSummary(source!, f),
    enabled: source !== undefined,
    ...AGG_CACHE,
  });
}

/** 按群聚合行。行数封顶在群数上，与窗口多宽无关。 */
export function useRoomAggs(
  source: SourceKind | undefined,
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
) {
  return useQuery({
    queryKey: ["roomAggs", source, f, groups],
    queryFn: () => loadRoomAggs(source!, f, groups),
    enabled: source !== undefined,
    ...AGG_CACHE,
  });
}

/** 按客服聚合行。 */
export function useAgentAggs(source: SourceKind | undefined, f: QueryFilters) {
  return useQuery({
    queryKey: ["agentAggs", source, f],
    queryFn: () => loadAgentAggs(source!, f),
    enabled: source !== undefined,
    ...AGG_CACHE,
  });
}

/**
 * 分类汇总。`groups` 给了就按父类分（一级），不给就按 `type_id` 分（二级）。
 *
 * ⚠️ **一级和二级是两次请求，不能由一次拆出来** —— 分位数不可加，
 * 一级的 p50 只能由数据库按父类现算。
 */
export function useCategories(
  source: SourceKind | undefined,
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
) {
  return useQuery({
    queryKey: ["categories", source, f, groups],
    queryFn: () => loadCategories(source!, f, groups),
    enabled: source !== undefined,
    ...AGG_CACHE,
  });
}

/**
 * 事件明细的一页。**服务端翻页**，不再把整个窗口拉到浏览器。
 *
 * `placeholderData` 让翻页时保留上一页内容，避免表格闪成空白。
 */
export function useEventsPage(
  source: SourceKind | undefined,
  f: QueryFilters,
  page: number,
  pageSize: number,
  sorting: EventSorting = {},
) {
  return useQuery({
    // 排序进 key：换一列排就是另一份结果，共用缓存会拿到上一列的顺序。
    queryKey: ["eventsPage", source, f, page, pageSize, sorting],
    queryFn: () => loadEventsPage(source!, f, page, pageSize, sorting),
    enabled: source !== undefined,
    placeholderData: (previous) => previous,
    ...AGG_CACHE,
  });
}
