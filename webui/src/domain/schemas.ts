/**
 * 领域契约。**字段名与列类型逐字对齐 `schema.sql`**，接口层不做映射。
 *
 * 用 zod 定义、再由 zod 推导 TS 类型，契约只有一处：接口返回的每一条记录都在
 * 进入指标层之前被校验。这不是洁癖 —— 这块看板是给上级看的，一个把 NULL 当 0
 * 的字段、一个少了 `agents` 的记录，产出的都是「偏小但看起来正常」的数字。
 * 宁可在边界上显式失败，也不静默渲染错的。
 */

import { z } from "zod";
import { formatDateTime, parseDateTime } from "@/lib/format";

const DATE = z.iso.date();
const DATETIME = z
  .string()
  .regex(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/, "时间须为 YYYY-MM-DD HH:mm:ss")
  .refine((value) => {
    const date = parseDateTime(value);
    return Number.isFinite(date.getTime()) && formatDateTime(date) === value;
  }, "时间必须是有效的 UTC+8 日期时间");
const EASY_USER_ID = z.string().min(1);

/** 群里的两方，**都是客服**：EXTERNAL 是商家客服，INTERNAL 是平台客服。 */
export const roleSchema = z.enum(["EXTERNAL", "INTERNAL"]);

/** b_merchant_group_event 的一行。 */
export const eventSchema = z
  .object({
    id: z.number().int().positive(),
    corpid: z.string(),
    roomid: z.string(),
    /** 溯源：非空，且每个 ID 必须真实存在于该次抽取的消息里 */
    source_msg_ids: z.array(z.string()).min(1, "source_msg_ids 不可为空，溯源会断"),
    first_msg_time: DATETIME,
    last_msg_time: DATETIME,
    /** 首条 INTERNAL 来源消息时间。**NULL 表示未回复，不是 0 秒** */
    first_agent_reply_time: DATETIME.nullable(),
    /** = date(first_msg_time)，报表归属日 */
    occurred_on: DATE,
    asker: EASY_USER_ID,
    asker_role: roleSchema,
    /** 涉及的全部 INTERNAL 成员。未回复的事件这里是空数组 */
    agents: z.array(EASY_USER_ID),
    first_responder: EASY_USER_ID.nullable(),
    summary: z.string(),
    /** 二级 type_id，**主类**。指标只按它统计 */
    event_type: z.string(),
    /** 全集，第一个恒等于 event_type。副类只供下钻，不进任何指标 */
    event_types: z.array(z.string()).min(1),
    taxonomy_version: z.string(),
  })
  .superRefine((event, ctx) => {
    if (event.occurred_on !== event.first_msg_time.slice(0, 10)) {
      ctx.addIssue({
        code: "custom",
        path: ["occurred_on"],
        message: "归属日必须等于首条消息日期",
      });
    }
    if (
      event.last_msg_time < event.first_msg_time ||
      (event.first_agent_reply_time !== null &&
        (event.first_agent_reply_time < event.first_msg_time ||
          event.first_agent_reply_time > event.last_msg_time))
    ) {
      ctx.addIssue({
        code: "custom",
        path: ["first_agent_reply_time"],
        message: "事件时间顺序不一致",
      });
    }
    if (event.event_types[0] !== event.event_type) {
      ctx.addIssue({ code: "custom", path: ["event_types"], message: "首个分类必须等于主分类" });
    }
    if (
      (event.first_agent_reply_time === null) !== (event.first_responder === null) ||
      (event.first_responder !== null && !event.agents.includes(event.first_responder))
    ) {
      ctx.addIssue({
        code: "custom",
        path: ["first_responder"],
        message: "首响时间、首响客服与参与客服不一致",
      });
    }
  });
export type EventRow = z.infer<typeof eventSchema>;

/** b_merchant_group_metric_daily 的一行。事件级列在抽取失败时是 NULL，不是 0。 */
export const groupDailySchema = z
  .object({
    corpid: z.string(),
    roomid: z.string(),
    dt: DATE,
    /** 消息级，不依赖抽取，失败的群照样有 */
    msg_count: z.number().int().nonnegative(),
    sender_count: z.number().int().nonnegative(),
    event_count: z.number().int().nonnegative().nullable(),
    /** asker_role = EXTERNAL 的事件数。下面三个数的分母是它，不是 event_count */
    merchant_event_count: z.number().int().nonnegative().nullable(),
    unreplied_count: z.number().int().nonnegative().nullable(),
    first_reply_p50_sec: z.number().int().nonnegative().nullable(),
    first_reply_p90_sec: z.number().int().nonnegative().nullable(),
    extraction_status: z.enum(["ok", "failed"]),
  })
  .superRefine((row, ctx) => {
    const counts = [row.event_count, row.merchant_event_count, row.unreplied_count];
    const values = [...counts, row.first_reply_p50_sec, row.first_reply_p90_sec];
    if (
      row.extraction_status === "failed"
        ? values.some((value) => value !== null)
        : counts.some((value) => value === null)
    ) {
      ctx.addIssue({
        code: "custom",
        path: ["extraction_status"],
        message: "失败群日的事件指标必须为 NULL，成功群日的事件计数不可为 NULL",
      });
    }
  });
export type GroupDailyRow = z.infer<typeof groupDailySchema>;

/** b_merchant_group_agent_metric_daily 的一行。语义键是前六列。 */
export const agentDailySchema = z.object({
  corpid: z.string(),
  room: z.string(),
  agent: EASY_USER_ID,
  dt: DATE,
  event_type: z.string(),
  taxonomy_version: z.string(),
  /** first_responder 归属口径。未回复的事件不落在任何人头上 */
  event_count: z.number().int().nonnegative(),
});
export type AgentDailyRow = z.infer<typeof agentDailySchema>;

export const failureSchema = z.object({
  run_date: DATE,
  corpid: z.string(),
  roomid: z.string(),
  dt: DATE.optional(),
  reason: z.string(),
});
export type FailureRow = z.infer<typeof failureSchema>;

/** 版本化类型词表。两级，但只有二级（叶子）进 event_type。 */
export const taxonomyTypeSchema = z.object({
  type_id: z.string(),
  /** 一级分类名，只用于分组，不进 event_type、不进任何语义键 */
  parent_name: z.string(),
  name: z.string(),
  description: z.string().default(""),
});
export type TaxonomyType = z.infer<typeof taxonomyTypeSchema>;

/** 消息原文。走摄取端口 read_by_ids 取回，给人看的下钻不脱敏。 */
export const messageSchema = z.object({
  msg_id: z.string(),
  at: DATETIME,
  sender_id: EASY_USER_ID,
  sender_role: roleSchema,
  text: z.string(),
});
export type MessageRow = z.infer<typeof messageSchema>;

/**
 * 群名与客服姓名在库里**不存在**（领域里只有 officialRoomId 和 easyUserId）。
 * 后端若能提供花名册就填 alias，并把 alias_is_authoritative 置真；
 * 否则前端一律显示 ID 并标注「待补」，绝不把占位名当成真名端出去。
 */
export const metaSchema = z.object({
  corpid: z.string(),
  days: z
    .array(DATE)
    .min(1, "至少要有一天数据，否则整块看板没有量纲")
    .refine(
      (days) => days.every((day, index) => index === 0 || day > days[index - 1]!),
      "日期必须唯一且按升序排列",
    ),
  rooms: z.array(z.object({ roomid: z.string(), alias: z.string().nullable() })),
  agents: z.array(z.object({ agent: EASY_USER_ID, alias: z.string().nullable() })),
  taxonomy: z.array(taxonomyTypeSchema),
  taxonomy_version: z.string(),
  alias_is_authoritative: z.boolean().default(false),
});
export type Meta = z.infer<typeof metaSchema>;

export const eventListSchema = z.array(eventSchema);
export const groupDailyListSchema = z.array(groupDailySchema);
export const agentDailyListSchema = z.array(agentDailySchema);
export const failureListSchema = z.array(failureSchema);
export const messageListSchema = z.array(messageSchema);

/** 指标层实际消费的形态：原始行 + 派生字段。派生只算一次。 */
export interface DecoratedEvent extends EventRow {
  /** 首响秒数。**NULL 表示未回复，绝不折成 0** */
  readonly firstReplySec: number | null;
  readonly level1: string;
  readonly level2: string;
  /** last_msg_time 与 occurred_on 不同天。它仍只在开始日计一次 */
  readonly crossDay: boolean;
}

export interface Dataset {
  readonly meta: Meta;
  readonly events: readonly DecoratedEvent[];
  readonly groupDaily: readonly GroupDailyRow[];
  readonly agentDaily: readonly AgentDailyRow[];
  readonly failures: readonly FailureRow[];
}
