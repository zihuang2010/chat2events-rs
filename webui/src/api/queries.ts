/** 数据获取的 React 绑定。缓存、重试、失效全部交给 TanStack Query，不自己造。 */

import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import {
  loadDataset,
  loadMessages,
  type LoadedDataset,
  type SourceKind,
  type SourcePreference,
} from "./source";
import type { MessageRow } from "@/domain/schemas";

export function useDataset(): UseQueryResult<LoadedDataset, Error> {
  const [params] = useSearchParams();
  const value = params.get("source");
  const source: SourcePreference = value === "api" || value === "mock" ? value : "auto";
  return useQuery({
    queryKey: ["dataset", source],
    queryFn: () => loadDataset(source),
    // T+1 跑批，一天只换一次数据。窗口重新聚焦时不该再打一轮接口。
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
