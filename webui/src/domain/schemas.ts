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
    /**
     * 末条来源消息发送方角色 —— asker_role 的镜像，取末条而非首条。
     * EXTERNAL = 商家说完没人接；**null = 加这一列之前抽取的历史行**，不是某一边
     */
    last_msg_role: roleSchema.nullable(),
    /**
     * 后续轮次最长等待秒数 —— 首响之后每次 EXTERNAL→INTERNAL 取最大。
     * **工作时段口径 [08:30, 21:00)**，周末与节假日不扣；全站只有这一列这么算，
     * 与首响时效同一口径，区别是它在抽取时算死、首响查询期现算。0 = 确实没有后续轮次；**null = 没算过**（历史行），两者不混
     */
    followup_wait_max_sec: z.number().int().nonnegative().nullable(),
    /**
     * 二级 type_id。**一个事件一个类** —— 曾经还有一列 `event_types` 存标签全集，
     * 副类只供下钻、不进任何指标，2026-09-14 连同整套多标签机制移除。
     */
    event_type: z.string().nullable(),
    taxonomy_version: z.string().nullable(),
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
    // 标签两列同生同死：打标未完成时一起是 null，完成后一起有值（承重不变量 4）。
    if ((event.event_type === null) !== (event.taxonomy_version === null)) {
      ctx.addIssue({
        code: "custom",
        path: ["taxonomy_version"],
        message: "分类与词表版本必须同时有值或同时为空",
      });
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
    classification_status: z.enum(["pending", "ok", "failed"]),
    /** 只读取数派生：晚于群日记录的失败不能确定影响范围，保守标记未知。 */
    freshness: z.enum(["known", "unknown"]).optional(),
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

/** 消息原文。抽取时落库的渲染快照，给人看的下钻不脱敏；非文本消息 text 是 [图片] 这样的占位符。 */
export const messageSchema = z.object({
  msg_id: z.string(),
  at: DATETIME,
  sender_id: EASY_USER_ID,
  sender_role: roleSchema,
  text: z.string(),
});
export type MessageRow = z.infer<typeof messageSchema>;

/**
 * 群名称来自 b_wecom_merchant_group，逐群标记是否为权威名称。
 * 商家 ID 用字符串保留 BIGINT 精度；客服姓名仍由全局标记控制。
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
  rooms: z.array(
    z.object({
      roomid: z.string(),
      alias: z.string().nullable(),
      merchant_id: z.string().nullable().optional(),
      alias_is_authoritative: z.boolean().optional(),
    }),
  ),
  agents: z.array(
    z.object({
      agent: EASY_USER_ID,
      alias: z.string().nullable(),
      /** 这个 alias 是权威姓名，还是回落显示的平台账号。缺席时回落到 meta 的全局位。 */
      alias_is_authoritative: z.boolean().optional(),
    }),
  ),
  taxonomy: z.array(taxonomyTypeSchema),
  taxonomy_version: z.string(),
  alias_is_authoritative: z.boolean().default(false),
});
export type Meta = z.infer<typeof metaSchema>;

/**
 * 聚合接口的返回 —— **每个字段都和 `metrics.ts` 的 `Aggregate` 同名同义**。
 * 后端 SQL 与前端算法已逐个数字对拍过（`src/web/tests.rs` 里那组断言）；
 * 改这里的任何一个名字，都要同时改 `web/query.rs` 里那条 SELECT。
 */
export const summarySchema = z.object({
  events: z.number(),
  rooms: z.number(),
  agents: z.number(),
  /** 当前匹配事件引用的原文条数，按 (企业, 群, msg_id) 去重 —— 不是各事件条数之和 */
  sourceMessages: z.number(),
  merchant: z.number(),
  replied: z.number(),
  unreplied: z.number(),
  push: z.number(),
  /** 没有已回复的商家事件时是 null，**不是 0** */
  p50: z.number().nullable(),
  p90: z.number().nullable(),
  overdue: z.number(),
  /** 分母为零时是 null —— 「没有商家事件」不能读成「零超时率」 */
  overdueRate: z.number().nullable(),
  unrepliedRate: z.number().nullable(),
  backlog: z.number(),
  crossDay: z.number(),
  /**
   * 只含**确实有事件**的那些天，缺的天由前端补零。
   *
   * ⚠️ `p50` / `p90` 是**当天现算的**，不是窗口分位数摊到每天 —— 分位数不可加。
   * 群抽屉的「每日首响」两条折线读的就是它。
   */
  byDay: z.array(
    z.object({
      day: DATE,
      events: z.number(),
      merchant: z.number(),
      unreplied: z.number(),
      overdue: z.number(),
      /** 分母为零时是 null —— 「那天没有商家事件」不能读成「零超时率」 */
      overdueRate: z.number().nullable(),
      p50: z.number().nullable(),
      p90: z.number().nullable(),
    }),
  ),
  /** 事件到达节奏。按**首条消息**归入小时，只含有事件的小时 */
  byHour: z.array(z.object({ hour: z.number(), events: z.number(), overdue: z.number() })),
  /**
   * 首响时长直方图。**桶边界由前端传上去**（`RESPONSE_BINS`），
   * 所以长度恒等于「边界数 + 1」；没传边界时是空数组。
   */
  replyBuckets: z.array(z.number()),
});
export type SummaryRow = z.infer<typeof summarySchema>;

/** 群 / 客服的按日序列。**缺的天不出行** —— 那天是真的 0 还是抽取失败，只有群日表答得了。 */
const dayPointSchema = z.object({ day: DATE, events: z.number() });

/** 按群一行。只有「必须从明细算」的项；消息数 / 每日序列继续用 groupDaily 拼。 */
export const roomAggSchema = z.object({
  roomid: z.string(),
  events: z.number(),
  merchant: z.number(),
  unreplied: z.number(),
  unrepliedRate: z.number().nullable(),
  overdue: z.number(),
  overdueRate: z.number().nullable(),
  backlog: z.number(),
  p50: z.number().nullable(),
  p90: z.number().nullable(),
  series: z.array(dayPointSchema),
  /**
   * 事件最多的前四个分类。`key` 与 `/api/categories` 同义：给了 `groups` 就是**组下标**，
   * 没给就是 `type_id`。**在数据库里截前四**，行数恒为「群数 × 4」。
   */
  topGroups: z.array(z.object({ key: z.string(), count: z.number() })),
});
export type RoomAgg = z.infer<typeof roomAggSchema>;

/**
 * 按客服一行。**混着两个口径，不能互相替代**：
 * `involved` / `rooms` 是「参与过」，
 * `owned` / `merchantOwned` / `p50` / `p90` / `overdue` 是「首响归属」。
 *
 * ⚠️ **没有 `unreplied`，是删掉的不是漏掉的。** 它此前在「参与过」这一侧算
 * 「未回复的商家事件」，而抽取保证「无平台回复 ⟹ `agents` 为空」—— 未回复的事件
 * 没有任何参与者，这一列**结构上恒为 0**。一个永远是 0 的数字比没有更糟：
 * 它看起来像「这个人没有欠回复的事件」。团队口径的无响应数在 `/api/summary`。
 */
export const agentAggSchema = z.object({
  agent: EASY_USER_ID,
  involved: z.number(),
  rooms: z.number(),
  owned: z.number(),
  merchantOwned: z.number(),
  replySamples: z.number(),
  overdue: z.number(),
  overdueRate: z.number().nullable(),
  p50: z.number().nullable(),
  p90: z.number().nullable(),
  series: z.array(z.object({ day: DATE, involved: z.number(), owned: z.number() })),
  /** 参与过的群。抽屉的分群明细与「数据完整性」那一列要它 */
  roomIds: z.array(z.string()),
});
export type AgentAgg = z.infer<typeof agentAggSchema>;

/**
 * 分类汇总的一行。**后端不认识词表** —— `key` 是 `type_id`（二级），
 * 或调用方传上去的分组下标（一级）。名字、父类、占比全由前端拿 `meta.taxonomy` 补。
 *
 * `p50` / `p90` 必须由后端按该分组现算：分位数不可加，一级的分位数
 * 合不出来（见 `web/query.rs` 的 `read_categories`）。
 */
export const categoryAggSchema = z.object({
  key: z.string(),
  count: z.number(),
  merchant: z.number(),
  unreplied: z.number(),
  unrepliedRate: z.number().nullable(),
  p50: z.number().nullable(),
  p90: z.number().nullable(),
  series: z.array(dayPointSchema),
});
export type CategoryAgg = z.infer<typeof categoryAggSchema>;
export const categoryAggListSchema = z.array(categoryAggSchema);

export const roomAggListSchema = z.array(roomAggSchema);
export const agentAggListSchema = z.array(agentAggSchema);

export const eventListSchema = z.array(eventSchema);

/**
 * `/api/events` 的一页 —— **翻页契约整个在响应体里**。
 *
 * ⚠️ `total` 是**明细自己那个集合**的总数：明细表翻的是窗口内**全部**事件，
 * 不按已知成功群日过滤（抽取失败的群日上抽出了什么，正是要核实的）。
 * 此前分页总数借的是 `/api/summary` 的 `events`，而那个数只算已知成功群日 ——
 * 窗口里一有失败或凭据未知的群日，两个数就不等，取较小值那步把人夹在更早的页码上，
 * **尾部的行永远翻不到，而页面看起来一切正常**。
 *
 * `pages` 由后端算好（总数 ÷ 每页条数，再夹最大页码护栏），前端不做任何算术，
 * 也不需要知道那个护栏的取值。`truncated` = 页数被护栏夹过，据此给一句提示。
 */
export const eventsPageSchema = z.object({
  rows: eventListSchema,
  total: z.number(),
  pages: z.number(),
  truncated: z.boolean(),
});
export type EventsPage = z.infer<typeof eventsPageSchema>;
export const groupDailyListSchema = z.array(groupDailySchema);
export const agentDailyListSchema = z.array(agentDailySchema);
export const failureListSchema = z.array(failureSchema);
export const messageListSchema = z.array(messageSchema);

/**
 * 页面的**上下文**：一次只读事务里的 meta ＋ 群日记录。
 *
 * ⚠️ **不含事件明细。** 它是唯一一个「群数 × 天数 × 每群每天事件数」的集合
 * （1000 群 7 天约 11 万行，直接撞后端的 `max_rows`），而页面要它只是为了在浏览器里
 * 现算指标 —— 那些指标现在由 `/api/summary`、`/api/rooms`、`/api/agents`、
 * `/api/categories` 在数据库里算完只送数字，明细由 `/api/events` 一页一页翻。
 *
 * 留下的群日记录是 `群数 × 天数`，而且它是唯一能回答「这个格子是真的 0 还是抽取失败」
 * 的东西 —— 覆盖度、消息量、热力图缺口全靠它，聚合接口替代不了。
 */
export const rawDatasetSchema = z.object({
  meta: metaSchema,
  groupDaily: z.array(
    groupDailySchema.refine((row) => row.freshness !== undefined, "缺少处理新鲜度"),
  ),
});

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
  readonly groupDaily: readonly GroupDailyRow[];
}
