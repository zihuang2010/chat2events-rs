/** 数据获取的 React 绑定。缓存、重试、失效全部交给 TanStack Query，不自己造。 */

import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { useLocation, useSearchParams } from "react-router-dom";
import {
  loadDataset,
  loadMessages,
  type LoadedDataset,
  type SourceKind,
  type SourcePreference,
} from "./source";
import type { MessageRow } from "@/domain/schemas";
import { fetchEvent } from "./client";

export function useStoredEvent(
  source: SourceKind,
  eventId: number | null,
  taxonomyVersion: string,
) {
  return useQuery({
    queryKey: ["event", source, eventId, taxonomyVersion],
    queryFn: () => {
      if (eventId === null) throw new Error("缺少事件 ID");
      return fetchEvent(eventId, taxonomyVersion);
    },
    enabled: source === "api" && eventId !== null,
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
