/** 概览的消息级汇总与等待时长。 */
import { addDays, parseDateTime } from "@/lib/format";
import type { Analytics } from "@/features/filters/useAnalytics";
import type { FiltersApi } from "@/features/filters/useFilters";

export interface OverviewProps {
  analytics: Analytics;
  api: FiltersApi;
}

export const MSG_INFO =
  "metric_daily.msg_count 在所选日期与群范围内求和。消息级列，不依赖抽取 —— 抽取失败的群日照样有值。" +
  "它是这块看板唯一不受抽取成败影响的量纲：事件数偏小时，用它判断是真的清静还是抽取漏了。";

export interface DayCell {
  day: string;
  msgs: number;
  /** 发言人次（跨群求和会重复计人），不是人数 */
  senders: number;
  cells: number;
  failed: number;
}

export interface MsgRollup {
  msgs: number;
  byDay: DayCell[];
}

/** 消息级汇总。**按日期与群筛选**，与 coverage() 同一口径，两处数字不会对不上。 */
export function msgRollup(a: Analytics, api: FiltersApi): MsgRollup {
  const room = api.filters.room;
  const cells = a.dataset.groupDaily.filter(
    (g) => a.dayset.has(g.dt) && (!room || g.roomid === room),
  );

  const acc = new Map<string, DayCell>(
    a.days.map((d) => [d, { day: d, msgs: 0, senders: 0, cells: 0, failed: 0 }]),
  );
  for (const c of cells) {
    const cell = acc.get(c.dt);
    if (!cell) continue;
    cell.msgs += c.msg_count;
    cell.senders += c.sender_count;
    cell.cells += 1;
    if (c.extraction_status === "failed") cell.failed += 1;
  }

  const msgs = cells.reduce((s, c) => s + c.msg_count, 0);

  return {
    msgs,
    byDay: a.days.map((d) => acc.get(d) ?? { day: d, msgs: 0, senders: 0, cells: 0, failed: 0 }),
  };
}

/** 无响应事件已经等了多久。基准是数据窗口末日 24:00，不是当前墙钟 —— T+1 跑批的
 *  数据可能是几天前的，用 now() 会把等待时长算成「从那天到今天」。 */
export function waitedSecFrom(boundaryDay: string, firstMsgTime: string): number {
  const end = parseDateTime(addDays(boundaryDay, 1)).getTime();
  return Math.max(0, (end - parseDateTime(firstMsgTime).getTime()) / 1000);
}
