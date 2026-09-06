/**
 * 指标层：从事实行算出所有展示用的数。**纯函数，不碰 DOM、不发请求、不读全局状态。**
 * 指标口径由领域测试验证，页面交互由视图测试验证。
 *
 * 三条硬口径写在这里一次，视图不再各自实现：
 *   1. 首响 / 未回复 / 超时的分母是【商家发起事件数】，不是事件总数。
 *   2. 分位数一律在事件明细上现算，绝不对每日 p50 取平均（分位数不可加）。
 *   3. null 表示「没算出来」，一路保持 null，绝不在中途兜底成 0。
 */

import { dayOf, parseDateTime } from "@/lib/format";
import { UNTYPED } from "./definitions";
import type { DecoratedEvent, EventRow, GroupDailyRow, Meta, TaxonomyType } from "./schemas";

export type TaxonomyIndex = ReadonlyMap<string, TaxonomyType>;

export function buildTaxonomyIndex(taxonomy: readonly TaxonomyType[]): TaxonomyIndex {
  const m = new Map<string, TaxonomyType>(taxonomy.map((t) => [t.type_id, t]));
  m.set(UNTYPED, {
    type_id: UNTYPED,
    parent_name: "未归类",
    name: "归不上去",
    description: "有词表但归不上去，是数据信号不是系统状态。",
  });
  return m;
}

export function decorate(events: readonly EventRow[], tax: TaxonomyIndex): DecoratedEvent[] {
  return events.map((e) => {
    const type = tax.get(e.event_type);
    return {
      ...e,
      firstReplySec:
        e.first_agent_reply_time === null
          ? null
          : Math.max(
              0,
              (parseDateTime(e.first_agent_reply_time).getTime() -
                parseDateTime(e.first_msg_time).getTime()) /
                1000,
            ),
      level1: type?.parent_name ?? "未归类",
      level2: type?.name ?? e.event_type,
      crossDay: dayOf(e.last_msg_time) !== e.occurred_on,
    };
  });
}

export const isMerchant = (e: DecoratedEvent): boolean => e.asker_role === "EXTERNAL";
export const isUnreplied = (e: DecoratedEvent): boolean =>
  isMerchant(e) && e.first_agent_reply_time === null;
export const isOverdue = (e: DecoratedEvent, slaSec: number): boolean =>
  isMerchant(e) && (e.firstReplySec === null || e.firstReplySec > slaSec);
export const isBacklog = (e: DecoratedEvent, lastDay: string): boolean =>
  isUnreplied(e) && e.occurred_on < lastDay;

export type EventStatusKind = "push" | "unreplied" | "overdue" | "replied";

export function statusOf(e: DecoratedEvent, slaSec: number): EventStatusKind {
  if (!isMerchant(e)) return "push";
  if (isUnreplied(e)) return "unreplied";
  return (e.firstReplySec ?? 0) > slaSec ? "overdue" : "replied";
}

/** 升序数组的分位数。空数组返回 null，**不返回 0**。 */
export function quantile(sortedAsc: readonly number[], p: number): number | null {
  if (sortedAsc.length === 0) return null;
  const i = Math.min(sortedAsc.length - 1, Math.floor(sortedAsc.length * p));
  return sortedAsc[i] ?? null;
}

const repliedSecsAsc = (events: readonly DecoratedEvent[]): number[] =>
  events
    .filter((e) => e.firstReplySec !== null)
    .map((e) => e.firstReplySec as number)
    .sort((a, b) => a - b);

export interface Aggregate {
  events: number;
  rooms: number;
  agents: number;
  merchant: number;
  replied: number;
  unreplied: number;
  push: number;
  p50: number | null;
  p90: number | null;
  overdue: number;
  overdueRate: number | null;
  unrepliedRate: number | null;
  backlog: number;
  crossDay: number;
}

export function aggregate(
  events: readonly DecoratedEvent[],
  slaSec: number,
  lastDay: string,
): Aggregate {
  const merchant = events.filter(isMerchant);
  const secs = repliedSecsAsc(merchant);
  const overdue = merchant.filter((e) => isOverdue(e, slaSec)).length;
  const people = new Set<string>();
  for (const e of events) for (const a of e.agents) people.add(a);

  return {
    events: events.length,
    rooms: new Set(events.map((e) => e.roomid)).size,
    agents: people.size,
    merchant: merchant.length,
    replied: secs.length,
    unreplied: merchant.length - secs.length,
    push: events.length - merchant.length,
    p50: quantile(secs, 0.5),
    p90: quantile(secs, 0.9),
    overdue,
    overdueRate: merchant.length ? overdue / merchant.length : null,
    unrepliedRate: merchant.length ? (merchant.length - secs.length) / merchant.length : null,
    backlog: events.filter((e) => isBacklog(e, lastDay)).length,
    crossDay: events.filter((e) => e.crossDay).length,
  };
}

export interface Coverage {
  cells: number;
  failed: number;
  rooms: string[];
  days: string[];
  complete: boolean;
}

/** 覆盖度。**数据完整时也要显示**，让调用方在结构上没法忘记处理它。 */
export function coverage(
  groupDaily: readonly GroupDailyRow[],
  dayset: ReadonlySet<string>,
  room: string | null,
): Coverage {
  const cells = groupDaily.filter((g) => dayset.has(g.dt) && (!room || g.roomid === room));
  const bad = cells.filter((g) => g.extraction_status === "failed");
  return {
    cells: cells.length,
    failed: bad.length,
    rooms: [...new Set(bad.map((g) => g.roomid))],
    days: [...new Set(bad.map((g) => g.dt))].sort(),
    complete: bad.length === 0,
  };
}

export function dailyCounts(
  events: readonly DecoratedEvent[],
  days: readonly string[],
  predicate?: (e: DecoratedEvent) => boolean,
): number[] {
  const m = new Map(days.map((d) => [d, 0]));
  for (const e of events) {
    if (predicate && !predicate(e)) continue;
    const cur = m.get(e.occurred_on);
    if (cur !== undefined) m.set(e.occurred_on, cur + 1);
  }
  return days.map((d) => m.get(d) ?? 0);
}

export interface RoomRow {
  key: string;
  label: string;
  /** 整段都失败时是 null，不是 0 */
  events: number | null;
  merchant: number | null;
  unreplied: number | null;
  unrepliedRate: number | null;
  p50: number | null;
  p90: number | null;
  overdue: number;
  overdueRate: number | null;
  backlog: number;
  msgs: number;
  senders: number;
  failedDays: number;
  totalDays: number;
  /** 每日事件数，缺格是 null（那天抽取失败），渲染时必须断成缺口 */
  series: (number | null)[];
  topLevel1: { name: string; count: number }[];
}

export function roomRollup(params: {
  events: readonly DecoratedEvent[];
  groupDaily: readonly GroupDailyRow[];
  rooms: Meta["rooms"];
  days: readonly string[];
  dayset: ReadonlySet<string>;
  slaSec: number;
  lastDay: string;
  labelOf: (roomid: string) => string;
  query: string;
}): RoomRow[] {
  const { events, groupDaily, rooms, days, dayset, slaSec, lastDay, labelOf, query } = params;
  const out: RoomRow[] = [];

  for (const r of rooms) {
    const cells = groupDaily.filter((g) => g.roomid === r.roomid && dayset.has(g.dt));
    if (cells.length === 0) continue;
    const mine = events.filter((e) => e.roomid === r.roomid);
    const label = labelOf(r.roomid);
    if (query && !`${label} ${r.roomid}`.toLowerCase().includes(query) && mine.length === 0)
      continue;

    const agg = aggregate(mine, slaSec, lastDay);
    const failedDays = cells.filter((c) => c.extraction_status === "failed").length;
    const allFailed = failedDays === cells.length;
    const l1 = new Map<string, number>();
    for (const e of mine) l1.set(e.level1, (l1.get(e.level1) ?? 0) + 1);

    out.push({
      key: r.roomid,
      label,
      events: allFailed ? null : agg.events,
      merchant: allFailed ? null : agg.merchant,
      unreplied: allFailed ? null : agg.unreplied,
      unrepliedRate: agg.unrepliedRate,
      p50: agg.p50,
      p90: agg.p90,
      overdue: agg.overdue,
      overdueRate: agg.overdueRate,
      backlog: agg.backlog,
      msgs: cells.reduce((s, c) => s + c.msg_count, 0),
      senders: Math.max(0, ...cells.map((c) => c.sender_count)),
      failedDays,
      totalDays: cells.length,
      series: days.map((d) => {
        const cell = cells.find((c) => c.dt === d);
        if (!cell || cell.extraction_status === "failed") return null;
        return mine.filter((e) => e.occurred_on === d).length;
      }),
      topLevel1: [...l1]
        .sort((a, b) => b[1] - a[1])
        .slice(0, 4)
        .map(([name, count]) => ({ name, count })),
    });
  }
  return out;
}

export interface AgentRow {
  key: string;
  label: string;
  roomIds: string[];
  rooms: number;
  /** 参与量：从 agents[] 现算，多人协作各自计入 */
  involved: number;
  /** 首响归属量：agent_metric_daily 的生产口径，未回复的事件不计入任何人 */
  owned: number;
  /** 本人首响归属中的商家事件，是个人超时率分母。 */
  merchantOwned: number;
  /** 实际进入本人首响分位数的有效时长样本。 */
  replySamples: number;
  p50: number | null;
  p90: number | null;
  overdue: number;
  overdueRate: number | null;
  unreplied: number;
  involvedSeries: number[];
  ownedSeries: number[];
  failedCells: number;
}

export function agentRollup(params: {
  events: readonly DecoratedEvent[];
  groupDaily: readonly GroupDailyRow[];
  agents: Meta["agents"];
  days: readonly string[];
  dayset: ReadonlySet<string>;
  slaSec: number;
  labelOf: (agent: string) => string;
  query: string;
}): AgentRow[] {
  const { events, groupDaily, agents, days, dayset, slaSec, labelOf, query } = params;
  const out: AgentRow[] = [];

  for (const ag of agents) {
    const mine = events.filter((e) => e.agents.includes(ag.agent));
    if (mine.length === 0) continue;
    const label = labelOf(ag.agent);
    if (query && !`${label} ${ag.agent}`.toLowerCase().includes(query)) continue;

    const owned = mine.filter((e) => e.first_responder === ag.agent);
    const ownedMerchant = owned.filter(isMerchant);
    const secs = repliedSecsAsc(ownedMerchant);
    const roomIds = [...new Set(mine.map((e) => e.roomid))];
    const overdue = ownedMerchant.filter((e) => isOverdue(e, slaSec)).length;

    out.push({
      key: ag.agent,
      label,
      roomIds,
      rooms: roomIds.length,
      involved: mine.length,
      owned: owned.length,
      merchantOwned: ownedMerchant.length,
      replySamples: secs.length,
      p50: quantile(secs, 0.5),
      p90: quantile(secs, 0.9),
      overdue,
      overdueRate: ownedMerchant.length ? overdue / ownedMerchant.length : null,
      unreplied: mine.filter(isUnreplied).length,
      involvedSeries: dailyCounts(mine, days),
      ownedSeries: dailyCounts(owned, days),
      failedCells: groupDaily.filter(
        (g) => roomIds.includes(g.roomid) && dayset.has(g.dt) && g.extraction_status === "failed",
      ).length,
    });
  }
  return out;
}

export interface CategoryRow {
  key: string;
  label: string;
  parent: string | null;
  count: number;
  merchant: number;
  unreplied: number;
  unrepliedRate: number | null;
  p50: number | null;
  p90: number | null;
  share: number;
}

/** 分类汇总。**只按主类 event_type 统计**，副类不进任何指标。 */
export function categoryRollup(
  events: readonly DecoratedEvent[],
  level: "level1" | "level2",
  tax: TaxonomyIndex,
): CategoryRow[] {
  interface Bucket {
    count: number;
    merchant: number;
    unreplied: number;
    secs: number[];
  }
  const m = new Map<string, Bucket>();
  for (const e of events) {
    const key = level === "level1" ? e.level1 : e.event_type;
    let b = m.get(key);
    if (!b) {
      b = { count: 0, merchant: 0, unreplied: 0, secs: [] };
      m.set(key, b);
    }
    b.count += 1;
    if (isMerchant(e)) b.merchant += 1;
    if (isUnreplied(e)) b.unreplied += 1;
    if (e.firstReplySec !== null && isMerchant(e)) b.secs.push(e.firstReplySec);
  }
  const total = events.length || 1;
  return [...m]
    .map(([key, b]) => {
      const type = level === "level2" ? tax.get(key) : undefined;
      b.secs.sort((a, x) => a - x);
      return {
        key,
        label: level === "level1" ? key : (type?.name ?? key),
        parent: type?.parent_name ?? null,
        count: b.count,
        merchant: b.merchant,
        unreplied: b.unreplied,
        unrepliedRate: b.merchant ? b.unreplied / b.merchant : null,
        p50: quantile(b.secs, 0.5),
        p90: quantile(b.secs, 0.9),
        share: b.count / total,
      };
    })
    .sort((a, b) => b.count - a.count);
}
