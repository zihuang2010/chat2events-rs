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
  groupDayStatus,
  isBacklog,
  isMerchant,
  isOverdue,
  isUnreplied,
  type Aggregate,
  type Coverage,
  type TaxonomyIndex,
} from "@/domain/metrics";
import type { DecoratedEvent } from "@/domain/schemas";
import { addDays, windowBounds } from "@/lib/format";
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
  roomAliasIsAuthoritative: (roomId: string) => boolean;
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
    const { from, to } = windowBounds(allDays, filters.from, filters.to);

    // 只补齐已加载窗口内的自然日，避免 URL 的远端日期触发无界枚举。
    const days = windowDays ? [...windowDays] : [];
    if (!windowDays) {
      const loadedFrom = allDays[0]!;
      const loadedTo = allDays[allDays.length - 1]!;
      const start = from < loadedFrom ? loadedFrom : from;
      const end = to > loadedTo ? loadedTo : to;
      if (start <= end) {
        for (let day = start; day < end; day = addDays(day, 1)) days.push(day);
        days.push(end);
      }
    }
    const dayset = new Set(days);
    const lastDay = days[days.length - 1] ?? to;

    const rooms = new Map(dataset.meta.rooms.map((r) => [r.roomid, r]));
    const agentAlias = new Map(dataset.meta.agents.map((a) => [a.agent, a.alias]));
    const roomLabel = (id: string) => rooms.get(id)?.alias ?? id;
    const roomAliasIsAuthoritative = (id: string) => {
      const room = rooms.get(id);
      return Boolean(
        room?.alias && (room.alias_is_authoritative ?? dataset.meta.alias_is_authoritative),
      );
    };
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

    const knownByRoom = new Map<string, Set<string>>();
    for (const cell of dataset.groupDaily) {
      if (groupDayStatus(cell) !== "ok") continue;
      const known = knownByRoom.get(cell.roomid) ?? new Set<string>();
      known.add(cell.dt);
      knownByRoom.set(cell.roomid, known);
    }
    // 失败重跑保留旧事实供核实，但这些事实不能作为本轮已知成功的指标。
    const windowEvents = dataset.events.filter(
      (e) => dayset.has(e.occurred_on) && knownByRoom.get(e.roomid)?.has(e.occurred_on),
    );
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
      cov: coverage(dataset.groupDaily, dayset, filters.room, dataset.meta.rooms),
      slaSec: filters.slaSec,
      query,
      aliasIsAuthoritative: dataset.meta.alias_is_authoritative,
      roomLabel,
      roomAliasIsAuthoritative,
      agentLabel,
      typeLabel,
    };
  }, [dataset, filters, windowDays]);
}
