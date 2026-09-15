/**
 * 口径与能力边界的**唯一文案来源**。界面上每个 ⓘ 与口径折叠区
 * 全部引用这里，不各自抄一遍 —— 抄了必然漏改一处。
 */

export const UNTYPED = "__untyped__";
export const DEFAULT_SLA_SEC = 1800;

export const SLA_OPTIONS = [
  { value: 900, label: "15 分钟" },
  { value: 1800, label: "30 分钟" },
  { value: 3600, label: "1 小时" },
  { value: 7200, label: "2 小时" },
  { value: 14400, label: "4 小时" },
] as const;

/**
 * 首响时长直方图的桶边界（秒，升序）与对应标签。**只有这一份。**
 *
 * 边界随 `buckets=` 参数传给 `/api/summary`，后端照着分桶后只回计数 ——
 * 后端不自带一份默认分桶，改这里两边一起变。标签比边界多一个：
 * 第一个桶是闭区间 `[0, 60]`，最后一个是 `(14400, ∞)`。
 */
export const RESPONSE_BIN_EDGES = [60, 300, 900, 1800, 3600, 7200, 14400] as const;
export const RESPONSE_BIN_LABELS = [
  "0–1 分",
  "1–5 分",
  "5–15 分",
  "15–30 分",
  "30–60 分",
  "1–2 小时",
  "2–4 小时",
  ">4 小时",
] as const;

/**
 * 明细翻页的护栏，**与后端 `web::query` 的 `MAX_PAGE` / `PAGE_SIZE_MAX` 相同**。
 * 越界那边直接 400（不夹到边界上），所以这边得先拦住，别把错留给用户看。
 */
export const MAX_PAGE = 200;
export const PAGE_SIZE_MAX = 100;

/**
 * 明细表可排序的列 —— **必须与后端 `Paging::order_by` 的白名单逐字相同**。
 *
 * ⚠️ **少的那几列是有意少的**：`summary` / `last_msg_role` / `followup_wait_max_sec`
 * 不在 `idx_overview` 里，按它们排会让服务端的分页从「只数索引条目」退化成
 * 「整个窗口逐行回表」。在前端给它们挂个排序器**更糟**：那样只会重排当前一页，
 * 而表头看起来像排了全部。要排它们，先把列加进索引。
 */
export const EVENT_SORTS = {
  time: "first_msg_time",
  reply: "first_agent_reply_time",
  wait: "firstReplySec",
  room: "roomid",
  last: "last_msg_time",
} as const;
export type EventSort = keyof typeof EVENT_SORTS;

export const NAV = [
  { key: "overview", path: "/overview", label: "整体概览" },
  { key: "rooms", path: "/rooms", label: "群聊洞察" },
  { key: "events", path: "/events", label: "事件洞察" },
  { key: "agents", path: "/agents", label: "客服效能" },
  { key: "detail", path: "/detail", label: "数据追溯" },
] as const;

export const EVENT_STATUS = {
  unreplied: "无响应",
  replied: "已回复",
  overdue: "已回复（超时）",
  backlog: "积压",
  push: "平台发起",
} as const;

/** 可作为筛选条件的状态（overdue 走独立的超时开关，不重复出现在这里）。 */
export const STATUS_FILTERS = ["unreplied", "replied", "backlog", "push"] as const;
export type StatusFilter = (typeof STATUS_FILTERS)[number];

export const METRIC = {
  events:
    "按事件去重计数。一个事件是 b_merchant_group_event 的一行，可能由多条消息组成，绝不等于消息条数。多客服协作只算一起。",
  rooms:
    "活跃群：当前筛选范围内至少有一起事件的群，按 roomid 去重；选择客服时仅统计其参与事件涉及的群。按事件统计，不等于有消息的群数。抽取失败或缺失的群日不计入，统计可能不完整，以完整性提示为准。",
  agentsInvolved:
    "活跃客服数：对事件的 agents[] 取并集去重，即参与过的平台客服人数，不是首响人数。",
  merchant:
    "asker_role = EXTERNAL 的事件数。首响、无响应、超时三组指标的分母都是它，不是事件总数。",
  unreplied:
    "first_agent_reply_time IS NULL 且 asker_role = EXTERNAL。平台发起的工单推送首响恒 0 秒、永远算已回复，混进分母会让无响应率静默偏低，所以被排除。无响应事件不以 0 秒计入任何分位数。",
  p50: "首条来源消息到客服首条 INTERNAL 来源消息的时间差，取中位数。只统计商家发起且已回复的事件。分位数在事件明细上现算：分位数不可加，对每日 p50 求平均没有意义。工作时段口径 08:30–21:00，时段外的等待不计；周末与节假日照常算工作日。",
  p90: "同 P50 的口径，取 90 分位。用分位数不用均值，一条几小时才回的会把均值整个带偏。",
  overdue:
    "分子是首响超过阈值的商家发起事件，无响应计入分子（T+2 跑批下无响应必然已超过任何阈值）。分母是商家发起事件数。工作时段口径 08:30–21:00：夜里进来次日一早回的不算超时，周末与节假日照常算工作日。",
  backlog:
    "前端派生口径，库里没有这一列：无响应且归属日早于所选区间最后一天。因为没有「已解决」字段，无法计算真正的积压。",
  crossDay:
    "last_msg_time 与 occurred_on 不在同一天的事件。它仍只按开始日计一次，不会在两天各算一次。",
  tail: "末条来源消息发送方角色。EXTERNAL＝商家说完没人接，INTERNAL＝客服收的尾。与 asker_role 同源同形，只是取末条而非首条，确定性计算不经模型。它只说「谁讲了最后一句」，不说这件事办没办完——库里没有解决口径。加这一列之前抽取的历史行是 NULL，冻结区不可回填。",
  involved:
    "活跃量：按事件 ID 去重，从 agents[] 现算。多客服参与同一事件时各自计入，所以各人相加会大于事件总数；全局事件数仍以事件明细去重为准。",
  owned:
    "first_responder 归属的事件数，等于 agent_metric_daily.event_count 的合计（生产口径就是 FirstResponder）。无响应的事件不落在任何人头上，所以它也不等于事件总数。它不是解决量。",
  agentReply:
    "仅统计首位响应客服为本人的商家发起事件。P50 为首响时长中位数，P90 为 90 分位；在有效事件样本上计算。平台发起、仅参与协作及无响应事件不进入本人首响分位数。工作时段口径 08:30–21:00，时段外不计；周末与节假日照常算工作日。",
  agentOverdue:
    "分子为本人首响归属中超过首响阈值的商家事件，分母为本人首响归属的商家事件数。平台发起不计入；未归属客服的无响应事件只在事件范围统计，不分摊至个人。",
  coverage:
    "抽取失败的「群 × 日」在 metric_daily 上事件级列是 NULL 不是 0，在 agent_metric_daily 上是整行缺失。本页事件级数字只含已知量。缺少独立的客服群归属记录，范围内有失败或缺失群日时，不能确认客服数字完整；无记录也不等于零。",
  msgCount:
    "消息总量：在所选日期与群范围内汇总 metric_daily.msg_count，不受事件分类、客服、响应状态、超时或关键词筛选影响。消息级统计不依赖事件抽取，抽取失败的群日仍计入；缺失群日仅展示已知量，无记录时显示暂缺。",
  sourceMessages:
    "当前匹配事件关联的来源消息数，按企业、群和消息 ID 去重，同一消息被多个事件引用只计一次。随事件筛选，不受表格分页影响；跨日事件包含其全部来源消息。它不是群消息总量，也不代表原文目前仍可读取的条数。",
  roomMessageTotal:
    "汇总当前群表的消息总量列，统计所列群在所选日期内的全部消息。关键词可能缩小群集合；分类、客服和响应状态不会把群内消息拆成对应事件的消息总量。抽取失败的群日消息数仍计入，缺失或最新结果未知时仅展示已知量；全部无记录时显示暂缺。",
  heat: "群 × 日的事件量。色深表示事件多，斜纹格表示那天抽取失败（值是 NULL 不是 0）。点格子可下钻到该群该日。",
  primaryOnly:
    "一个事件只有一个分类 event_type，合计恒等于事件数。曾经支持一个事件挂多个类，副类不进任何指标——一个事件计进 N 行会让合计大于事件数。",
  untyped:
    "vN 加 __untyped__ 是数据信号不是系统状态：有词表但归不上去。占比持续偏高就该考虑词表升版。",
  unclassified:
    "主分类为「归不上去」，或主分类编码不在当前词表中的事件，统一计入未归类。占比以当前筛选范围事件总数为分母；未归类不计入已知分类覆盖数，但保留在分类分析中。",
  timezone:
    "库里 first_msg_time 等列是 MySQL DATETIME，不做时区转换，取值即业务本地时间 UTC+8。全站时长口径统一为工作时段 08:30–21:00：首响、超时、后续等待都只算时段内的时间，时段外不计。没有工作日历，周末与节假日照常算工作日。",
  followupWait:
    "首响之后每一次「商家说话 → 客服接话」的间隔取最大，衡量后续轮次跟不跟得上。只算 08:30–21:00 之内的时间，时段外不计；没有工作日历，周末与节假日照常算工作日。与首响同一口径，但它在抽取时就算死了，首响是查询期现算的。末尾没人接的那一段不计——那是「尾部」那一列的事。0 是算出来的事实（确实只有一问一答），不是缺数据；缺数据显示为暂无。平台发起的事件不进入该指标。",
} as const;
