/**
 * 指标层：从事实行算出所有展示用的数。**纯函数，不碰 DOM、不发请求、不读全局状态。**
 * 指标口径由领域测试验证，页面交互由视图测试验证。
 *
 * 三条硬口径写在这里一次，视图不再各自实现：
 *   1. 首响 / 未回复 / 超时的分母是【商家发起事件数】，不是事件总数。
 *   2. 分位数一律在事件明细上现算，绝不对每日 p50 取平均（分位数不可加）。
 *   3. null 表示「没算出来」，一路保持 null，绝不在中途兜底成 0。
 */

import { dayOf } from "@/lib/format";
import { UNTYPED } from "./definitions";
import { workSecsBetween } from "./worktime";
import type {
  AgentAgg,
  CategoryAgg,
  DecoratedEvent,
  EventRow,
  GroupDailyRow,
  Meta,
  RoomAgg,
  TaxonomyType,
} from "./schemas";

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
      // 首响走**工作时段口径**（`domain/worktime`），不是墙钟差 —— 与后端
      // `metric_daily.first_reply_p*_sec` 和 webUI 取数的 SQL 是同一份定义。
      firstReplySec:
        e.first_agent_reply_time === null
          ? null
          : workSecsBetween(e.first_msg_time, e.first_agent_reply_time),
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

/**
 * 没有群日记录只能判未知，不能推断当天应该处理或已经成功。
 *
 * ⚠️ 返回 `"ok"` 就是术语表里的**已知成功群日**（`CONTEXT.md`「术语」一节）——
 * 承重不变量 5 的分母边界。**后端那份实现是 `src/web/query.rs` 的 `KNOWN_OK_DAYS`**，
 * 改这里必须一起看那边：两边分家是静默的，页面照样显示一个看起来合理的数字。
 * （`freshness` 那一列由后端算好随群日记录下发，见 `read_group_days`。）
 */
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
      // ⚠️ **打标状态不进 `uncertainRooms` / `uncertainDays`。** 那两个集合的含义是
      // 「这个群日的**事件计数**不可信」，而事件数是抽取的产物，跟标签算没算完无关：
      // 抽取成功的群日，事件日计数就是可信的。混进来的后果是静默的 —— 一个群日
      // `pending` 就能让 `EventTrends` 把那天**所有分类**的折线断成 null，而那里的
      // 解释文案条件是 `cov.failed`（此时为 0），于是图空一片、一个字解释都没有。
      // 标签的缺口由 `pendingLabels` / `failedLabels` 单独表达，也照旧让 `complete` 为假。
      if (row?.extraction_status === "ok") {
        if (row.classification_status === "pending") pendingLabels += 1;
        if (row.classification_status === "failed") failedLabels += 1;
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

/**
 * 群表的一行 = **聚合接口的一行 ＋ 群日记录**，这里只做拼接，不再算任何指标。
 *
 * 分工是硬的，不是偏好：
 *   * `events` / `merchant` / 分位数 / `overdue` / `backlog` / `series` / `topGroups`
 *     必须由数据库算（分位数不可加、超时线是查询期参数），来自 `/api/rooms`；
 *   * `msgs` / `senders` / 各类失败天数只在 `b_merchant_group_metric_daily` 里，
 *     而群日记录是「群数 × 天数」，本来就不会爆，所以留在前端拼。
 *
 * ⚠️ **整段抽取失败的群，计数一律给 `null` 而不是 0**（承重不变量 4 的形状）：
 * 聚合接口只统计「本轮已知成功」的群日，所以那种群在 `/api/rooms` 里根本没有行 ——
 * 拿「没有行」当 0 会把一个抽取失败的群显示成一个很清静的群。
 */
export function roomRollup(params: {
  aggs: readonly RoomAgg[];
  groupDaily: readonly GroupDailyRow[];
  rooms: Meta["rooms"];
  days: readonly string[];
  dayset: ReadonlySet<string>;
  labelOf: (roomid: string) => string;
  /** 一级分类，**下标必须与请求 `/api/rooms` 时传的 `groups` 一致** */
  parents: readonly { name: string }[];
  query: string;
}): RoomRow[] {
  const { aggs, groupDaily, rooms, days, dayset, labelOf, parents, query } = params;
  const out: RoomRow[] = [];
  const aggByRoom = new Map(aggs.map((agg) => [agg.roomid, agg]));
  const cellsByRoom = groupBy(
    groupDaily.filter((row) => dayset.has(row.dt)),
    (row) => row.roomid,
  );

  for (const r of rooms) {
    const cells = cellsByRoom.get(r.roomid) ?? [];
    const agg = aggByRoom.get(r.roomid);
    const label = labelOf(r.roomid);
    if (query && !`${label} ${r.roomid}`.toLowerCase().includes(query) && !agg) continue;

    const cov = coverage(cells, dayset, r.roomid, [r]);
    const allFailed = cov.known === 0;
    const cellsByDay = new Map(cells.map((cell) => [cell.dt, cell]));
    const seriesByDay = new Map((agg?.series ?? []).map((point) => [point.day, point.events]));

    out.push({
      key: r.roomid,
      label,
      events: allFailed ? null : (agg?.events ?? 0),
      merchant: allFailed ? null : (agg?.merchant ?? 0),
      unreplied: allFailed ? null : (agg?.unreplied ?? 0),
      unrepliedRate: agg?.unrepliedRate ?? null,
      p50: agg?.p50 ?? null,
      p90: agg?.p90 ?? null,
      overdue: agg?.overdue ?? 0,
      overdueRate: agg?.overdueRate ?? null,
      backlog: agg?.backlog ?? 0,
      msgs: cells.length ? cells.reduce((s, c) => s + c.msg_count, 0) : null,
      senders: cells.length ? Math.max(...cells.map((c) => c.sender_count)) : null,
      failedDays: cov.failed,
      pendingLabels: cov.pendingLabels,
      failedLabels: cov.failedLabels,
      missingDays: cov.missing,
      unknownDays: cov.unknown,
      totalDays: days.length,
      // 那一天抽取没成，格子就是缺口不是 0 —— 只有群日表答得了这件事。
      series: days.map((d) =>
        groupDayStatus(cellsByDay.get(d)) === "ok" ? (seriesByDay.get(d) ?? 0) : null,
      ),
      topLevel1: (agg?.topGroups ?? []).map((group) => ({
        name: parents[Number(group.key)]?.name ?? group.key,
        count: group.count,
      })),
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
  involvedSeries: (number | null)[];
  ownedSeries: (number | null)[];
  failedCells: number;
  coverageUnknown: boolean;
}

/**
 * 客服表的一行 = **`/api/agents` 的一行 ＋ 群日记录**。这里只做拼接与缺口判定。
 *
 * ⚠️ **`involved`（参与）与 `owned`（首响归属）是两个口径，不能互相替代** ——
 * 把参与当归属，同一起事件的工作量会重复计到每个参与者头上。后端 `read_agents`
 * 分开算，这里也分开放。
 *
 * ⚠️ **缺口按「这个人参与过的群」判，不按全部群判。**
 *
 * 此前的判据是「那天**任一**群不 ok」—— 而 `process/daily/run.rs` 的 `rotate_daily`
 * 自己写着：1000 个群、每轮跑得完 400，某个群可能连续几百天一行数据都没有。
 * **缺格是常态**，于是「任一群」几乎每天都成立：每个客服的两条折线全空、
 * 「完整性未知」恒真 —— 那个保守选择退化成了「永远留空」，等于什么都没说。
 *
 * 改成按 `agg.roomIds`（这个人在窗口内参与过的群）判。残留的敞口是：某个群这个人
 * 参与过、但它在窗口内**每一天都失败**，那它进不了 `roomIds`（那份名单来自成功事件），
 * 于是漏判 —— 这就是下面那句「无法从成功事件反推」说的情况，没法消除，
 * 但比「整天对所有人留空」诚实得多。
 */
export function agentRollup(params: {
  aggs: readonly AgentAgg[];
  groupDaily: readonly GroupDailyRow[];
  days: readonly string[];
  dayset: ReadonlySet<string>;
  labelOf: (agent: string) => string;
  query: string;
}): AgentRow[] {
  const { aggs, groupDaily, days, dayset, labelOf, query } = params;
  const cells = groupDaily.filter((row) => dayset.has(row.dt));
  const cellsByDay = groupBy(cells, (row) => row.dt);
  const failedByRoom = new Map<string, number>();
  for (const cell of cells) {
    if (cell.extraction_status === "failed") {
      failedByRoom.set(cell.roomid, (failedByRoom.get(cell.roomid) ?? 0) + 1);
    }
  }
  // 按天索引一次，下面每个客服拿自己的群名单来查（否则是 客服 × 天 × 格子）。
  const recordedByDay = new Map(
    days.map((day) => [day, new Map((cellsByDay.get(day) ?? []).map((row) => [row.roomid, row]))]),
  );

  const out: AgentRow[] = [];
  for (const agg of aggs) {
    const label = labelOf(agg.agent);
    if (query && !`${label} ${agg.agent}`.toLowerCase().includes(query)) continue;
    // 只看这个人参与过的群 —— 理由见函数头。名单为空时没有判断依据，不留空。
    const unknownDays = new Set(
      days.filter((day) => {
        const recorded = recordedByDay.get(day);
        return agg.roomIds.some((room) => groupDayStatus(recorded?.get(room)) !== "ok");
      }),
    );
    const byDay = new Map(agg.series.map((point) => [point.day, point]));
    const known = (pick: (point: { involved: number; owned: number }) => number) =>
      days.map((day) => (unknownDays.has(day) ? null : byDay.has(day) ? pick(byDay.get(day)!) : 0));

    out.push({
      key: agg.agent,
      label,
      roomIds: agg.roomIds,
      rooms: agg.rooms,
      involved: agg.involved,
      owned: agg.owned,
      merchantOwned: agg.merchantOwned,
      replySamples: agg.replySamples,
      p50: agg.p50,
      p90: agg.p90,
      overdue: agg.overdue,
      overdueRate: agg.overdueRate,
      involvedSeries: known((point) => point.involved),
      ownedSeries: known((point) => point.owned),
      failedCells: agg.roomIds.reduce((sum, room) => sum + (failedByRoom.get(room) ?? 0), 0),
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
  /** 每日事件量。**缺的天不在数组里**，由视图按覆盖度决定补零还是断成缺口 */
  series: readonly { day: string; events: number }[];
}

/**
 * 把 `/api/categories` 的行补成视图要的形态 —— **名字、父类、占比全在前端补**。
 *
 * 后端不认识词表（见 `web/query.rs` 的 `read_categories`），所以它回的 `key`：
 *   * 二级是 `type_id`，在这里查词表拿显示名与父类名；
 *   * 一级是**请求时传上去的分组下标**，在这里按同一个 `parents` 数组换回父类名。
 *     两边必须是同一个数组同一个顺序，否则名字会错位到另一个父类上。
 *
 * `share` 的分母是**当前筛选范围的事件总数**（含打标未完成的），来自 `/api/summary`
 * 的 `events`；分类行只统计已打标的，所以合计不足 100% 是对的。
 */
export function categoryRows(
  rows: readonly CategoryAgg[],
  level: "level1" | "level2",
  tax: TaxonomyIndex,
  parents: readonly { name: string }[],
  total: number,
): CategoryRow[] {
  return rows
    .map((row) => {
      const type = level === "level2" ? tax.get(row.key) : undefined;
      // 一级的 `key` 是分组下标；**等于组数的那个是兜底桶** —— 词表外的编码与
      // `__untyped__` 都在里面，统一叫「未归类」（见 `useAnalytics.parentGroups`）。
      const label =
        level === "level1" ? (parents[Number(row.key)]?.name ?? "未归类") : (type?.name ?? row.key);
      return {
        key: level === "level1" ? label : row.key,
        label,
        parent: type?.parent_name ?? null,
        count: row.count,
        merchant: row.merchant,
        unreplied: row.unreplied,
        unrepliedRate: row.unrepliedRate,
        p50: row.p50,
        p90: row.p90,
        share: total ? row.count / total : 0,
        series: row.series,
      };
    })
    .sort((a, b) => b.count - a.count);
}
