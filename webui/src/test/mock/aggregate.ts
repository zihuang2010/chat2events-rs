/**
 * 测试专用聚合对照：把事件样本算成聚合接口的形状。
 *
 * ⚠️ **它同时是后端 SQL 的对拍参照物。** 这里用的是 `domain/metrics` 那几个纯函数，
 * 也就是搬进 SQL 之前的那一份口径；`src/web/tests.rs` 里那组断言拿同一批数据
 * 比对两边。两边分家的那天，这里会先变。
 *
 * ⚠️ **筛选必须逐条对应后端 `web::query::Filters::clause`**。聚合那几路还要再夹
 * 一条**已知成功群日**（后端的 `KNOWN_OK_DAYS`）—— 少一条，模拟数据就会比真接口
 * 多算一批事件，而两边都看起来正常。
 *
 * ⚠️ **明细那一路（`mockEventsPage`）反过来：必须不夹那一条。** 真接口的
 * `/api/events` 不按已知成功群日过滤（明细是核实工具，抽取失败的群日上抽出了什么
 * 正是要看的）。两边在同一件事上口径相反的时候，mock 下总数与行必然同集合、
 * 翻页夹不出问题，只有打真接口才露出来 —— 这正是那个缺口一直没被发现的原因。
 */

import type { EventSorting, QueryFilters } from "@/api/client";
import {
  aggregate,
  decorate,
  filterRooms,
  groupDayStatus,
  isMerchant,
  isOverdue,
  isUnreplied,
  quantile,
  type TaxonomyIndex,
} from "@/domain/metrics";
import type {
  AgentAgg,
  CategoryAgg,
  DecoratedEvent,
  EventRow,
  EventsPage,
  GroupDailyRow,
  Meta,
  RoomAgg,
  SummaryRow,
} from "@/domain/schemas";
import { RESPONSE_BIN_EDGES } from "@/domain/definitions";
import { dayOf } from "@/lib/format";

/**
 * **已知成功群日**（`CONTEXT.md`「术语」一节）的集合。判据住在 `groupDayStatus`，
 * 后端那份实现是 `src/web/query.rs` 的 `KNOWN_OK_DAYS`。
 */
function okDays(groupDaily: readonly GroupDailyRow[]): ReadonlySet<string> {
  const ok = new Set<string>();
  for (const cell of groupDaily) {
    if (groupDayStatus(cell) === "ok") ok.add(`${cell.roomid}\0${cell.dt}`);
  }
  return ok;
}

/** 逐条对应后端 `Filters::clause`。**顺序无关，但每一条都要在。** */
function matches(event: DecoratedEvent, f: QueryFilters, lastDay: string): boolean {
  if (f.room && event.roomid !== f.room) return false;
  if (f.agent && !event.agents.includes(f.agent)) return false;
  if (f.responder && event.first_responder !== f.responder) return false;
  if (f.types?.length && (event.event_type === null || !f.types.includes(event.event_type)))
    return false;
  // 「未归类」= 有主类但不在词表里。打标未完成（NULL）不算 —— 那是「还没算」。
  if (
    f.typesExclude?.length &&
    (event.event_type === null || f.typesExclude.includes(event.event_type))
  )
    return false;
  const sla = f.slaSec ?? 1800;
  switch (f.status) {
    case "unreplied":
      if (!isUnreplied(event)) return false;
      break;
    case "replied":
      if (!(isMerchant(event) && event.first_agent_reply_time !== null)) return false;
      break;
    case "push":
      if (isMerchant(event)) return false;
      break;
    case "backlog":
      if (!(isUnreplied(event) && event.occurred_on < lastDay)) return false;
      break;
    default:
      break;
  }
  if (f.overdueOnly !== null && f.overdueOnly !== undefined) {
    if (isOverdue(event, sla) !== f.overdueOnly) return false;
  }
  // ⚠️ **只匹配摘要**，与后端的 `e.summary LIKE ?` 一致。群名 / 客服名的匹配
  // 由页面在**已经返回的聚合行**上本地做 —— 那些行数有界，不需要下推。
  if (f.q && !event.summary.toLowerCase().includes(f.q.toLowerCase())) return false;
  return true;
}

/**
 * 窗口 ＋ 筛选，一次筛到位。
 *
 * `scope` 决定要不要再夹一条**已知成功群日**：
 *   * `"known-ok-days"` —— 聚合那几路，承重不变量 5 的分母边界；
 *   * `"all"` —— **明细那一路**，与真接口的 `/api/events` 一致，见模块文档。
 */
function select(
  events: readonly EventRow[],
  groupDaily: readonly GroupDailyRow[],
  tax: TaxonomyIndex,
  f: QueryFilters,
  scope: "known-ok-days" | "all" = "known-ok-days",
  rooms?: Meta["rooms"],
): { rows: DecoratedEvent[]; lastDay: string } {
  const ok = okDays(groupDaily);
  const lastDay = f.to ?? "9999-12-31";
  // 商家分组 / 业务经理是群的属性，事件上没有 —— 要靠群元数据解析出符合条件的群。
  // 缺了元数据不能当成没筛（那会让模拟数据比真接口多算一批群，两边都看起来正常）。
  if ((f.merchantGroup || f.businessManager) && !rooms) {
    throw new Error("按商家分组 / 业务经理筛选需要群元数据（rooms）");
  }
  const allowed =
    f.merchantGroup || f.businessManager
      ? new Set(filterRooms(rooms!, f).map((room) => room.roomid))
      : null;
  const inWindow = decorate(events, tax).filter(
    (event) =>
      (!f.from || event.occurred_on >= f.from) &&
      (!f.to || event.occurred_on <= f.to) &&
      (!allowed || allowed.has(event.roomid)) &&
      (scope === "all" || ok.has(`${event.roomid}\0${event.occurred_on}`)),
  );
  return { rows: inWindow.filter((event) => matches(event, f, lastDay)), lastDay };
}

const repliedSecs = (rows: readonly DecoratedEvent[]): number[] =>
  rows
    .filter((e) => isMerchant(e) && e.firstReplySec !== null)
    .map((e) => e.firstReplySec as number)
    .sort((a, b) => a - b);

function byKey<T>(rows: readonly T[], keyOf: (row: T) => string): Map<string, T[]> {
  const out = new Map<string, T[]>();
  for (const row of rows) {
    const key = keyOf(row);
    const bucket = out.get(key);
    if (bucket) bucket.push(row);
    else out.set(key, [row]);
  }
  return out;
}

export function mockSummary(
  events: readonly EventRow[],
  groupDaily: readonly GroupDailyRow[],
  tax: TaxonomyIndex,
  f: QueryFilters,
  rooms?: Meta["rooms"],
): SummaryRow {
  const { rows, lastDay } = select(events, groupDaily, tax, f, "known-ok-days", rooms);
  const agg = aggregate(rows, f.slaSec ?? 1800, lastDay);
  const perDay = byKey(rows, (e) => e.occurred_on);
  const perHour = byKey(rows, (e) => e.first_msg_time.slice(11, 13));
  const secs = repliedSecs(rows);
  // 桶边界与真接口传上去的那一份是同一个常量，区间语义也逐字相同：
  // 第一个闭区间 [0, b0]，其余左开右闭，最后一个 (b[last], ∞)。
  const buckets = Array.from({ length: RESPONSE_BIN_EDGES.length + 1 }, () => 0);
  for (const sec of secs) {
    const index = RESPONSE_BIN_EDGES.findIndex((edge) => sec <= edge);
    buckets[index === -1 ? RESPONSE_BIN_EDGES.length : index]! += 1;
  }
  return {
    ...agg,
    byDay: [...perDay]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([day, mine]) => {
        const dayed = repliedSecs(mine);
        const merchant = mine.filter(isMerchant).length;
        const overdue = mine.filter((e) => isOverdue(e, f.slaSec ?? 1800)).length;
        return {
          day,
          events: mine.length,
          merchant,
          unreplied: mine.filter(isUnreplied).length,
          overdue,
          overdueRate: merchant ? overdue / merchant : null,
          p50: quantile(dayed, 0.5),
          p90: quantile(dayed, 0.9),
        };
      }),
    byHour: [...perHour]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([hour, mine]) => ({
        hour: Number(hour),
        events: mine.length,
        overdue: mine.filter((e) => isOverdue(e, f.slaSec ?? 1800)).length,
      })),
    replyBuckets: buckets,
  };
}

export function mockRoomAggs(
  events: readonly EventRow[],
  groupDaily: readonly GroupDailyRow[],
  tax: TaxonomyIndex,
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
  rooms?: Meta["rooms"],
): RoomAgg[] {
  const { rows, lastDay } = select(events, groupDaily, tax, f, "known-ok-days", rooms);
  return [...byKey(rows, (e) => e.roomid)]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([roomid, mine]) => {
      const agg = aggregate(mine, f.slaSec ?? 1800, lastDay);
      return {
        roomid,
        events: agg.events,
        merchant: agg.merchant,
        unreplied: agg.unreplied,
        unrepliedRate: agg.unrepliedRate,
        overdue: agg.overdue,
        overdueRate: agg.overdueRate,
        backlog: agg.backlog,
        p50: agg.p50,
        p90: agg.p90,
        series: [...byKey(mine, (e) => e.occurred_on)]
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([day, cells]) => ({ day, events: cells.length })),
        // 与后端的 `ROW_NUMBER() ... ORDER BY n DESC, k` 同序同截断。
        topGroups: [
          ...byKey(
            mine.filter((e) => e.event_type !== null),
            (e) =>
              groups?.length
                ? String(
                    (() => {
                      const i = groups.findIndex((types) => types.includes(e.event_type!));
                      return i === -1 ? groups.length : i;
                    })(),
                  )
                : e.event_type!,
          ),
        ]
          .map(([key, cells]) => ({ key, count: cells.length }))
          .sort((a, b) => b.count - a.count || a.key.localeCompare(b.key))
          .slice(0, 4),
      };
    });
}

export function mockAgentAggs(
  events: readonly EventRow[],
  groupDaily: readonly GroupDailyRow[],
  tax: TaxonomyIndex,
  f: QueryFilters,
  rooms?: Meta["rooms"],
): AgentAgg[] {
  const { rows } = select(events, groupDaily, tax, f, "known-ok-days", rooms);
  const perAgent = new Map<string, DecoratedEvent[]>();
  for (const event of rows) {
    for (const agent of new Set(event.agents)) {
      const bucket = perAgent.get(agent);
      if (bucket) bucket.push(event);
      else perAgent.set(agent, [event]);
    }
  }
  const sla = f.slaSec ?? 1800;
  return [...perAgent]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([agent, mine]) => {
      // ⚠️ 参与与首响归属是两个口径，不能互相替代（见后端 `read_agents`）。
      const owned = mine.filter((e) => e.first_responder === agent);
      const ownedMerchant = owned.filter(isMerchant);
      const secs = repliedSecs(ownedMerchant);
      const overdue = ownedMerchant.filter((e) => isOverdue(e, sla)).length;
      const perDay = byKey(mine, (e) => e.occurred_on);
      return {
        agent,
        involved: mine.length,
        rooms: new Set(mine.map((e) => e.roomid)).size,
        owned: owned.length,
        merchantOwned: ownedMerchant.length,
        replySamples: secs.length,
        overdue,
        overdueRate: ownedMerchant.length ? overdue / ownedMerchant.length : null,
        p50: quantile(secs, 0.5),
        p90: quantile(secs, 0.9),
        series: [...perDay]
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([day, cells]) => ({
            day,
            involved: cells.length,
            owned: cells.filter((e) => e.first_responder === agent).length,
          })),
        roomIds: [...new Set(mine.map((e) => e.roomid))].sort(),
      };
    });
}

export function mockCategories(
  events: readonly EventRow[],
  groupDaily: readonly GroupDailyRow[],
  tax: TaxonomyIndex,
  f: QueryFilters,
  groups?: readonly (readonly string[])[],
  rooms?: Meta["rooms"],
): CategoryAgg[] {
  const { rows } = select(events, groupDaily, tax, f, "known-ok-days", rooms);
  // 打标未完成的整个排除，与后端 `e.event_type IS NOT NULL` 一致。
  const typed = rows.filter((e) => e.event_type !== null);
  const keyOf = (event: DecoratedEvent): string | null => {
    if (!groups?.length) return event.event_type;
    const index = groups.findIndex((types) => types.includes(event.event_type!));
    // 落不进任何分组的归兜底桶（下标＝组数），与后端那个 `ELSE` 一致；
    // 丢掉它们会让占比合计悄悄少一块。
    return String(index === -1 ? groups.length : index);
  };
  const perKey = new Map<string, DecoratedEvent[]>();
  for (const event of typed) {
    const key = keyOf(event);
    if (key === null) continue;
    const bucket = perKey.get(key);
    if (bucket) bucket.push(event);
    else perKey.set(key, [event]);
  }
  return [...perKey]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([key, mine]) => {
      const merchant = mine.filter(isMerchant).length;
      const unreplied = mine.filter(isUnreplied).length;
      const secs = repliedSecs(mine);
      return {
        key,
        count: mine.length,
        merchant,
        unreplied,
        unrepliedRate: merchant ? unreplied / merchant : null,
        p50: quantile(secs, 0.5),
        p90: quantile(secs, 0.9),
        series: [...byKey(mine, (e) => e.occurred_on)]
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([day, cells]) => ({ day, events: cells.length })),
      };
    });
}

/** 后端 `web::params::MAX_PAGE` 那条翻页护栏。**只有这个模拟数据源知道它** —— 页面不需要。 */
const MAX_PAGE = 200;

/**
 * 与后端的 `ORDER BY` 逐条同序，否则翻页的内容对不上：
 * **NULL 一律排最后**（两个方向都是 —— 「没回复」不是「很快」），`id` 收尾保证唯一。
 *
 * ⚠️ **不按已知成功群日过滤**（`select` 的 `"all"`）—— 与真接口的 `/api/events`
 * 一致。此前这里过滤了而真接口不过滤，于是 mock 下总数与行必然同集合、
 * 翻页夹不出问题，真接口上却把人夹在更早的页码上。见模块文档。
 *
 * 总数、页数、截断标志的形状与真接口一致，页面据此翻页，不做任何算术。
 */
export function mockEventsPage(
  events: readonly EventRow[],
  groupDaily: readonly GroupDailyRow[],
  tax: TaxonomyIndex,
  f: QueryFilters,
  page: number,
  pageSize: number,
  sorting: EventSorting = {},
  rooms?: Meta["rooms"],
): EventsPage {
  const { rows } = select(events, groupDaily, tax, f, "all", rooms);
  const key = sorting.sort;
  const sign = sorting.dir === "desc" ? -1 : 1;
  const pick = (e: DecoratedEvent): string | number | null => {
    switch (key) {
      case "time":
        return e.first_msg_time;
      case "reply":
        return e.first_agent_reply_time;
      case "wait":
        return e.firstReplySec;
      case "room":
        return e.roomid;
      case "last":
        return e.last_msg_time;
      default:
        return null;
    }
  };
  const ordered = key
    ? [...rows].sort((a, b) => {
        const [x, y] = [pick(a), pick(b)];
        if (x === null || y === null) {
          if (x === y) return a.id - b.id;
          return x === null ? 1 : -1;
        }
        const cmp = typeof x === "number" ? x - (y as number) : x.localeCompare(y as string);
        return cmp * sign || a.id - b.id;
      })
    : [...rows].sort((a, b) => a.occurred_on.localeCompare(b.occurred_on) || a.id - b.id);
  const needed = Math.ceil(ordered.length / pageSize);
  return {
    rows: ordered.slice((page - 1) * pageSize, page * pageSize),
    total: ordered.length,
    pages: Math.min(needed, MAX_PAGE),
    truncated: needed > MAX_PAGE,
  };
}

/** 跨天判定与 `decorate` 同一份定义，导出给测试用。 */
export const crossesDay = (event: EventRow): boolean =>
  dayOf(event.last_msg_time) !== event.occurred_on;
