/** 概览的消息级汇总与等待时长。 */
import { addDays } from "@/lib/format";
import { workSecsBetween } from "@/domain/worktime";
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

/**
 * 消息级汇总。**按日期与群筛选**，与 coverage() 同一口径，两处数字不会对不上。
 *
 * ⚠️ 它是这块看板上唯一**不经过聚合接口**的量纲 —— 消息数在群日表里，
 * 而群日记录是「群数 × 天数」，本来就不会爆，所以留在前端拼（见 `useAnalytics`）。
 */
export function msgRollup(a: Analytics): MsgRollup {
  const cells = a.cells;

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

/** 无响应事件已经等了多久。基准是数据窗口末日 24:00，不是当前墙钟 —— 每日跑批的
 *  数据可能是几天前的，用 now() 会把等待时长算成「从那天到今天」。
 *
 *  与首响同一个工作时段口径（`domain/worktime`）：基准仍写 24:00，落到时段外自然
 *  被钳到当天 21:00，所以不必为它单独挑一个「末日收工时刻」。 */
export function waitedSecFrom(boundaryDay: string, firstMsgTime: string): number {
  return workSecsBetween(firstMsgTime, `${addDays(boundaryDay, 1)} 00:00:00`);
}
