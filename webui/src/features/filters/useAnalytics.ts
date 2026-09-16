/**
 * 视图的统一上下文：把「群日记录 + 筛选条件」算成各视图直接可用的形态，
 * 并把 URL 上的筛选条件翻译成**聚合接口认识的那一组参数**。
 *
 * ⚠️ **这里不再持有事件明细，也不再现算任何指标。** 指标由 `/api/summary`、
 * `/api/rooms`、`/api/agents`、`/api/categories` 在数据库里算完只送数字，
 * 明细由 `/api/events` 一页一页翻。留在这里的只有三样东西：
 *   1. **群日记录**派生的覆盖度与消息量 —— 只有它答得了「这个格子是真的 0
 *      还是抽取失败」，聚合接口替代不了；
 *   2. **标签映射**（群名 / 客服名 / 类型名）—— 词表和名册在前端手上；
 *   3. **`q`**：所有视图共用的同一组后端筛选参数，于是「图表和表格用的是同一批事件」
 *      这条保证从前端的一次 `filter()` 搬到了 SQL 的同一个 `WHERE` 上。
 */

import { useMemo } from "react";
import type { LoadedDataset } from "@/api/source";
import type { QueryFilters } from "@/api/client";
import { coverage, groupDayStatus, type Coverage, type TaxonomyIndex } from "@/domain/metrics";
import { addDays, windowBounds } from "@/lib/format";
import type { Filters } from "./useFilters";

/** 一级分类。**数组下标即 `/api/categories` 的 `groups` 下标**，顺序不能变。 */
export interface ParentGroup {
  name: string;
  types: string[];
}

export interface Analytics {
  dataset: LoadedDataset;
  taxIndex: TaxonomyIndex;
  days: string[];
  dayset: ReadonlySet<string>;
  lastDay: string;
  /** 窗口内每天的群日记录，按天求和后给消息量图用 */
  cells: readonly LoadedDataset["groupDaily"][number][];
  cov: Coverage;
  slaSec: number;
  query: string;
  aliasIsAuthoritative: boolean;
  parents: readonly ParentGroup[];
  /** 传给每个聚合接口的同一组筛选参数 */
  q: QueryFilters;
  roomLabel: (roomId: string) => string;
  roomAliasIsAuthoritative: (roomId: string) => boolean;
  /** 群背后的商家：名称 → 商家 ID → `null`（没关联商家，整块不渲染） */
  roomMerchant: (roomId: string) => string | null;
  agentLabel: (agentId: string) => string;
  agentAliasIsAuthoritative: (agentId: string) => boolean;
  typeLabel: (typeId: string) => string;
}

/** 词表按父类分组。**排序固定**（按父类名），否则 `groups` 下标会在两次请求间漂移。 */
export function parentGroups(dataset: LoadedDataset): ParentGroup[] {
  const byParent = new Map<string, string[]>();
  for (const type of dataset.meta.taxonomy) {
    const bucket = byParent.get(type.parent_name);
    if (bucket) bucket.push(type.type_id);
    else byParent.set(type.parent_name, [type.type_id]);
  }
  // ⚠️ **「未归类」不在这里**：它不是词表里的一个父类，而是「以上都不是」——
  // `__untyped__` 加上词表外的历史编码，后者根本列不出名单。聚合接口把它们
  // 归进**下标等于组数**的兜底桶（后端 `read_categories` 那个 `ELSE`），
  // 由 `categoryRows` 解释成「未归类」。
  return [...byParent]
    .map(([name, types]) => ({ name, types }))
    .sort((a, b) => a.name.localeCompare(b.name, "zh"));
}

/** 兜底桶的下标：等于真实父类的个数。 */
export const UNCLASSIFIED = "未归类";

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
    const agents = new Map(dataset.meta.agents.map((a) => [a.agent, a]));
    const roomLabel = (id: string) => rooms.get(id)?.alias ?? id;
    // ⚠️ **不回落到 meta 的全局位**：那一位现在的含义是「本窗口内至少有一位客服拿到了
    // 权威姓名」，拿它当群名的回落就是跨域借真相 —— 客服那边命中一个，群名这边就被
    // 一起说成权威。后端 `read_filters` 逐个群都带自己的标志，缺席只可能出现在更老的
    // 响应里，那时朝 false 倒是安全的方向。
    const roomAliasIsAuthoritative = (id: string) => {
      const room = rooms.get(id);
      return Boolean(room?.alias && (room.alias_is_authoritative ?? false));
    };
    // 商家那一支的回落链：**商家名 → 商家 ID → 不显示**。
    // 返回 null 表示这个群压根没关联商家，调用方整块不渲染 —— 不能回落成空串，
    // 那会在表格里留一行看不出是「没有」还是「没加载出来」的空白。
    // ⚠️ 不读 `merchant_name_is_authoritative`：回落值是一串裸 BIGINT，
    // 肉眼一看就不是名字，不像客服的 `zhang.san` 会被误当成姓名。
    const roomMerchant = (id: string) => {
      const room = rooms.get(id);
      return room?.merchant_name ?? room?.merchant_id ?? null;
    };
    const agentLabel = (id: string) => agents.get(id)?.alias ?? id;
    // 与 roomAliasIsAuthoritative 同形：per-项优先、回落全局。
    // ⚠️ 别名回落成平台账号时这里必须是 false —— `zhang.san` 是账号不是姓名。
    const agentAliasIsAuthoritative = (id: string) => {
      const agent = agents.get(id);
      return Boolean(
        agent?.alias && (agent.alias_is_authoritative ?? dataset.meta.alias_is_authoritative),
      );
    };
    const typeLabel = (id: string) => dataset.taxIndex.get(id)?.name ?? id;

    const parents = parentGroups(dataset);
    // 父类只是词表里的一个分组，SQL 不认识它 —— 展开成 `types=a,b,c` 再下推，
    // 后端因此不用 join 词表，也就不会多出一处能和前端打架的口径。
    //
    // ⚠️ **展开不出东西时用父类名本身占位**：那是个必然不存在的 `type_id`，
    // 于是结果为空。留空数组的话 `types` 根本不会进 URL，筛选会**静默失效成「全部」**。
    const known = parents.flatMap((parent) => parent.types);
    const types = filters.level2
      ? [filters.level2]
      : filters.level1 && filters.level1 !== UNCLASSIFIED
        ? (parents.find((p) => p.name === filters.level1)?.types ?? [filters.level1])
        : undefined;
    // 「未归类」是**排除式**的：词表外的编码没有名单，只能反着说。
    const typesExclude = !filters.level2 && filters.level1 === UNCLASSIFIED ? known : undefined;

    const q: QueryFilters = {
      from: days[0] ?? from,
      to: lastDay,
      room: filters.room,
      agent: filters.agent,
      ...(types ? { types } : {}),
      ...(typesExclude ? { typesExclude } : {}),
      status: filters.status,
      overdueOnly: filters.overdueOnly,
      q: filters.query.trim() || null,
      slaSec: filters.slaSec,
    };

    return {
      dataset,
      taxIndex: dataset.taxIndex,
      days,
      dayset,
      lastDay,
      cells: dataset.groupDaily.filter(
        (row) => dayset.has(row.dt) && (!filters.room || row.roomid === filters.room),
      ),
      cov: coverage(dataset.groupDaily, dayset, filters.room, dataset.meta.rooms),
      slaSec: filters.slaSec,
      query: filters.query.trim().toLowerCase(),
      aliasIsAuthoritative: dataset.meta.alias_is_authoritative,
      parents,
      q,
      roomLabel,
      roomAliasIsAuthoritative,
      roomMerchant,
      agentLabel,
      agentAliasIsAuthoritative,
      typeLabel,
    };
  }, [dataset, filters, windowDays]);
}

/** 窗口内某一天是否「本轮已知成功」。热力图与折线用它决定哪些格子该断成缺口。 */
export function knownDays(analytics: Analytics): ReadonlySet<string> {
  const byDay = new Map<string, { total: number; ok: number }>();
  for (const cell of analytics.cells) {
    const slot = byDay.get(cell.dt) ?? { total: 0, ok: 0 };
    slot.total += 1;
    if (groupDayStatus(cell) === "ok") slot.ok += 1;
    byDay.set(cell.dt, slot);
  }
  return new Set([...byDay].filter(([, s]) => s.ok > 0).map(([day]) => day));
}
