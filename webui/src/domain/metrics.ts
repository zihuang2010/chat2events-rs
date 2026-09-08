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

function groupBy<T>(rows: readonly T[], keyOf: (row: T) => string): Map<string, T[]> {
  const groups = new Map<string, T[]>();
  for (const row of rows) {
    const key = keyOf(row);
    const bucket = groups.get(key);
    if (bucket) bucket.push(row);
    else groups.set(key, [row]);
  }
  return groups;
}

export function buildTaxonomyIndex(
  taxonomy: readonly TaxonomyType[],
  version: string,
): TaxonomyIndex {
  const m = new Map<string, TaxonomyType>(taxonomy.map((t) => [t.type_id, t]));
  m.set(UNTYPED, {
    type_id: UNTYPED,
    parent_name: "未归类",
    name: version === "v0" ? "未建词表" : "归不上去",
    description:
      version === "v0"
        ? "尚未建立词表，分类暂不可用；事件总量与首响指标仍可用。"
        : "有词表但归不上去，是数据信号不是系统状态。",
  });
  return m;
}

export function decorate(events: readonly EventRow[], tax: TaxonomyIndex): DecoratedEvent[] {
  return events.map((e) => {
    const type = e.event_type === null ? undefined : tax.get(e.event_type);
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
      level1: e.event_type === null ? "打标未完成" : (type?.parent_name ?? "未归类"),
      level2: type?.name ?? e.event_type ?? "打标未完成",
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
  known: number;
  failed: number;
  missing: number;
  unknown: number;
  pendingLabels: number;
  failedLabels: number;
  rooms: string[];
  days: string[];
  complete: boolean;
}

/** 没有群日记录只能判未知，不能推断当天应该处理或已经成功。 */
export function groupDayStatus(
  row: GroupDailyRow | undefined,
): "missing" | "failed" | "unknown" | "ok" {
  if (!row) return "missing";
  if (row.extraction_status === "failed") return "failed";
  return row.freshness === "unknown" ? "unknown" : "ok";
}

export function coverageLabel(cov: Coverage): string {
  if (cov.complete) return "当前窗口抽取完整";
  if (cov.cells === 0) return "当前窗口无群日记录，完整性未知";
  return [
    cov.failed ? `${cov.failed} 个群日抽取失败` : "",
    cov.missing ? `${cov.missing} 个群日无记录，完整性未知` : "",
    cov.unknown ? `${cov.unknown} 个群日最新处理结果未知` : "",
    cov.pendingLabels ? `${cov.pendingLabels} 个群日待打标` : "",
    cov.failedLabels ? `${cov.failedLabels} 个群日打标失败` : "",
  ]
    .filter(Boolean)
    .join(" · ");
}

/** 覆盖度。**数据完整时也要显示**，让调用方在结构上没法忘记处理它。 */
export function coverage(
  groupDaily: readonly GroupDailyRow[],
  dayset: ReadonlySet<string>,
  room: string | null,
  rooms: Meta["rooms"],
): Coverage {
  const cells = groupDaily.filter((g) => dayset.has(g.dt) && (!room || g.roomid === room));
  const byRoom = groupBy(cells, (row) => row.roomid);
  const roomIds = room ? [room] : rooms.map((r) => r.roomid);
  let known = 0;
  let failed = 0;
  let missing = 0;
  let unknown = 0;
  let pendingLabels = 0;
  let failedLabels = 0;
  const uncertainRooms = new Set<string>();
  const uncertainDays = new Set<string>();
  for (const id of roomIds) {
    const byDay = new Map((byRoom.get(id) ?? []).map((row) => [row.dt, row]));
    for (const day of dayset) {
      const row = byDay.get(day);
      const status = groupDayStatus(row);
      if (row?.extraction_status === "ok") {
        if (row.classification_status === "pending") pendingLabels += 1;
        if (row.classification_status === "failed") failedLabels += 1;
        if (row.classification_status !== "ok") {
          uncertainRooms.add(id);
          uncertainDays.add(day);
        }
      }
      if (status === "ok") known += 1;
      else {
        if (status === "failed") failed += 1;
        else if (status === "missing") missing += 1;
        else unknown += 1;
        uncertainRooms.add(id);
        uncertainDays.add(day);
      }
    }
  }
  return {
    cells: cells.length,
    known,
    failed,
    missing,
    unknown,
    pendingLabels,
    failedLabels,
    rooms: [...uncertainRooms],
    days: [...uncertainDays].sort(),
    complete:
      known > 0 &&
      failed === 0 &&
      missing === 0 &&
      unknown === 0 &&
      pendingLabels === 0 &&
      failedLabels === 0,
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
  msgs: number | null;
  senders: number | null;
  failedDays: number;
  pendingLabels: number;
  failedLabels: number;
  missingDays: number;
  unknownDays: number;
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
  const eventsByRoom = groupBy(events, (event) => event.roomid);
  const cellsByRoom = groupBy(
    groupDaily.filter((row) => dayset.has(row.dt)),
    (row) => row.roomid,
  );

  for (const r of rooms) {
    const cells = cellsByRoom.get(r.roomid) ?? [];
    const mine = eventsByRoom.get(r.roomid) ?? [];
    const label = labelOf(r.roomid);
    if (query && !`${label} ${r.roomid}`.toLowerCase().includes(query) && mine.length === 0)
      continue;

    const agg = aggregate(mine, slaSec, lastDay);
    const cov = coverage(cells, dayset, r.roomid, [r]);
    const allFailed = cov.known === 0;
    const l1 = new Map<string, number>();
    for (const e of mine) {
      if (e.event_type !== null) l1.set(e.level1, (l1.get(e.level1) ?? 0) + 1);
    }
    const cellsByDay = new Map(cells.map((cell) => [cell.dt, cell]));
    const counts = dailyCounts(mine, days);

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
      msgs: cells.length ? cells.reduce((s, c) => s + c.msg_count, 0) : null,
      senders: cells.length ? Math.max(...cells.map((c) => c.sender_count)) : null,
      failedDays: cov.failed,
      pendingLabels: cov.pendingLabels,
      failedLabels: cov.failedLabels,
      missingDays: cov.missing,
      unknownDays: cov.unknown,
      totalDays: days.length,
      series: days.map((d, index) => {
        const cell = cellsByDay.get(d);
        if (groupDayStatus(cell) !== "ok") return null;
        return counts[index]!;
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
  involvedSeries: (number | null)[];
  ownedSeries: (number | null)[];
  failedCells: number;
  coverageUnknown: boolean;
}

export function agentRollup(params: {
  events: readonly DecoratedEvent[];
  groupDaily: readonly GroupDailyRow[];
  agents: Meta["agents"];
  rooms: Meta["rooms"];
  days: readonly string[];
  dayset: ReadonlySet<string>;
  slaSec: number;
  labelOf: (agent: string) => string;
  query: string;
}): AgentRow[] {
  const { events, groupDaily, agents, rooms, days, dayset, slaSec, labelOf, query } = params;
  const out: AgentRow[] = [];
  const eventsByAgent = new Map<string, DecoratedEvent[]>();
  for (const event of events) {
    for (const agent of new Set(event.agents)) {
      const bucket = eventsByAgent.get(agent);
      if (bucket) bucket.push(event);
      else eventsByAgent.set(agent, [event]);
    }
  }
  const cells = groupDaily.filter((row) => dayset.has(row.dt));
  const cellsByDay = groupBy(cells, (row) => row.dt);
  const failedByRoom = new Map<string, number>();
  for (const cell of cells) {
    if (cell.extraction_status === "failed") {
      failedByRoom.set(cell.roomid, (failedByRoom.get(cell.roomid) ?? 0) + 1);
    }
  }
  // 没有独立的客服群归属，失败或缺失群日是否涉及该客服无法从成功事件反推。
  const unknownDays = new Set(
    days.filter((day) => {
      const recorded = new Map((cellsByDay.get(day) ?? []).map((row) => [row.roomid, row]));
      return (
        rooms.length === 0 ||
        rooms.some((room) => groupDayStatus(recorded.get(room.roomid)) !== "ok")
      );
    }),
  );
  const knownCounts = (mine: readonly DecoratedEvent[]) =>
    dailyCounts(mine, days).map((count, index) => (unknownDays.has(days[index]!) ? null : count));

  for (const ag of agents) {
    const mine = eventsByAgent.get(ag.agent) ?? [];
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
      involvedSeries: knownCounts(mine),
      ownedSeries: knownCounts(owned),
      failedCells: roomIds.reduce((sum, room) => sum + (failedByRoom.get(room) ?? 0), 0),
      coverageUnknown: unknownDays.size > 0,
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
    if (e.event_type === null) continue;
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
