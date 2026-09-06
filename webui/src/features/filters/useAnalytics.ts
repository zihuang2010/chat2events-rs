/**
 * 视图的统一上下文：把「数据集 + 筛选条件」算成各视图直接可用的形态。
 * 每个视图都从这里拿数，所以**筛选结果在图表和表格之间天然一致**，
 * 不存在某张图自己再过滤一遍导致对不上的情况。
 */

import { useMemo } from "react";
import type { LoadedDataset } from "@/api/source";
import {
  aggregate,
  coverage,
  isBacklog,
  isMerchant,
  isOverdue,
  isUnreplied,
  type Aggregate,
  type Coverage,
  type TaxonomyIndex,
} from "@/domain/metrics";
import type { DecoratedEvent } from "@/domain/schemas";
import { addDays } from "@/lib/format";
import type { Filters } from "./useFilters";

export interface Analytics {
  dataset: LoadedDataset;
  taxIndex: TaxonomyIndex;
  days: string[];
  dayset: ReadonlySet<string>;
  lastDay: string;
  /** 只按日期窗口筛出来的事件，与其他条件无关 */
  windowEvents: DecoratedEvent[];
  /** 应用了全部筛选条件的事件，视图一律用它 */
  events: DecoratedEvent[];
  agg: Aggregate;
  cov: Coverage;
  slaSec: number;
  query: string;
  aliasIsAuthoritative: boolean;
  roomLabel: (roomId: string) => string;
  agentLabel: (agentId: string) => string;
  typeLabel: (typeId: string) => string;
}

export function useAnalytics(
  dataset: LoadedDataset,
  filters: Filters,
  windowDays?: readonly string[],
): Analytics {
  return useMemo(() => {
    const allDays = dataset.meta.days;
    const from =
      filters.from &&
      /^\d{4}-\d{2}-\d{2}$/.test(filters.from) &&
      addDays(filters.from, 0) === filters.from
        ? filters.from
        : (allDays[0] as string);
    const toRaw =
      filters.to && /^\d{4}-\d{2}-\d{2}$/.test(filters.to) && addDays(filters.to, 0) === filters.to
        ? filters.to
        : (allDays[allDays.length - 1] as string);
    const to = toRaw < from ? from : toRaw;

    // 概览传入连续自然日，缺记录的日期也保留在时间轴上。
    const days = windowDays ? [...windowDays] : allDays.filter((d) => d >= from && d <= to);
    const dayset = new Set(days);
    const lastDay = days[days.length - 1] ?? to;

    const roomAlias = new Map(dataset.meta.rooms.map((r) => [r.roomid, r.alias]));
    const agentAlias = new Map(dataset.meta.agents.map((a) => [a.agent, a.alias]));
    const roomLabel = (id: string) => roomAlias.get(id) ?? id;
    const agentLabel = (id: string) => agentAlias.get(id) ?? id;
    const typeLabel = (id: string) => dataset.taxIndex.get(id)?.name ?? id;

    const query = filters.query.trim().toLowerCase();
    const matches = (e: DecoratedEvent, boundary: string): boolean => {
      if (filters.room && e.roomid !== filters.room) return false;
      if (filters.agent && !e.agents.includes(filters.agent)) return false;
      if (filters.level1 && e.level1 !== filters.level1) return false;
      if (filters.level2 && e.event_type !== filters.level2) return false;
      if (filters.overdueOnly !== null && isOverdue(e, filters.slaSec) !== filters.overdueOnly)
        return false;
      if (filters.status === "unreplied" && !isUnreplied(e)) return false;
      if (filters.status === "replied" && !(isMerchant(e) && e.first_agent_reply_time !== null))
        return false;
      if (filters.status === "push" && isMerchant(e)) return false;
      if (filters.status === "backlog" && !isBacklog(e, boundary)) return false;
      if (query) {
        const hay =
          `${e.summary} ${roomLabel(e.roomid)} ${e.roomid} ${e.level1} ${e.level2} ${e.agents
            .map(agentLabel)
            .join(" ")}`.toLowerCase();
        if (!hay.includes(query)) return false;
      }
      return true;
    };

    const windowEvents = dataset.events.filter((e) => dayset.has(e.occurred_on));
    const events = windowEvents.filter((e) => matches(e, lastDay));

    return {
      dataset,
      taxIndex: dataset.taxIndex,
      days,
      dayset,
      lastDay,
      windowEvents,
      events,
      agg: aggregate(events, filters.slaSec, lastDay),
      cov: coverage(dataset.groupDaily, dayset, filters.room),
      slaSec: filters.slaSec,
      query,
      aliasIsAuthoritative: dataset.meta.alias_is_authoritative,
      roomLabel,
      agentLabel,
      typeLabel,
    };
  }, [dataset, filters, windowDays]);
}
