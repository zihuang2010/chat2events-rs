/**
 * 口径与能力边界的**唯一文案来源**。界面上每个 ⓘ、每处「待补数据」标记、
 * 页尾的口径说明，全部引用这里，不各自抄一遍 —— 抄了必然漏改一处。
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
  p50: "首条来源消息到客服首条 INTERNAL 来源消息的时间差，取中位数。只统计商家发起且已回复的事件。分位数在事件明细上现算：分位数不可加，对每日 p50 求平均没有意义。自然时间口径。",
  p90: "同 P50 的口径，取 90 分位。用分位数不用均值，一条几小时才回的会把均值整个带偏。",
  overdue:
    "分子是首响超过阈值的商家发起事件，无响应计入分子（T+2 跑批下无响应必然已超过任何阈值）。分母是商家发起事件数。自然时间口径，跨夜与周末照常计时。",
  backlog:
    "前端派生口径，库里没有这一列：无响应且归属日早于所选区间最后一天。因为没有「已解决」字段，无法计算真正的积压。",
  crossDay:
    "last_msg_time 与 occurred_on 不在同一天的事件。它仍只按开始日计一次，不会在两天各算一次。",
  involved:
    "活跃量：按事件 ID 去重，从 agents[] 现算。多客服参与同一事件时各自计入，所以各人相加会大于事件总数；全局事件数仍以事件明细去重为准。",
  owned:
    "first_responder 归属的事件数，等于 agent_metric_daily.event_count 的合计（生产口径就是 FirstResponder）。无响应的事件不落在任何人头上，所以它也不等于事件总数。它不是解决量。",
  agentReply:
    "仅统计首位响应客服为本人的商家发起事件。P50 为首响时长中位数，P90 为 90 分位；在有效事件样本上计算。平台发起、仅参与协作及无响应事件不进入本人首响分位数。采用自然时间，跨夜与周末照常计时。",
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
    "只按主类 event_type 统计。副类只落在 event_types 列供下钻，不进任何指标：一个事件计进 N 行会让合计大于事件数。",
  untyped:
    "vN 加 __untyped__ 是数据信号不是系统状态：有词表但归不上去。占比持续偏高就该考虑词表升版。",
  unclassified:
    "主分类为「归不上去」，或主分类编码不在当前词表中的事件，统一计入未归类。占比以当前筛选范围事件总数为分母；未归类不计入已知分类覆盖数，但保留在分类分析中。",
  timezone:
    "库里 first_msg_time 等列是 MySQL DATETIME，不做时区转换，取值即业务本地时间 UTC+8。全站走自然时间口径：跨夜与周末照常计时。",
} as const;

/** 库里没有来源的能力。界面上一律标记，不编造。 */
export const DATA_GAPS = [
  {
    title: "已解决 / 解决事件数 / 解决时长",
    detail:
      "没有 resolved 列，也还没有「什么算解决」的定义。抽取端的 still_open 是模型的段内控制位，没进 Event、没落库。",
    needs: "先定义「解决」判据，再在 b_merchant_group_event 上加 resolved_at 与 resolved_by 两列。",
  },
  {
    title: "后续回复时效 / 跟进是否及时",
    detail:
      "需要「待回复诉求」与「对应回复」的配对。库里只有 first_msg_time、last_msg_time、first_agent_reply_time 三个锚点，中间没有逐条角色时间线。下钻原文能人工看，但算不出可聚合的指标。",
    needs: "抽取阶段额外产出诉求与回复的配对，或在事件上存一条按角色分段的时间线。",
  },
  {
    title: "客服回复消息数",
    detail:
      "agent_metric_daily 只有事件数；metric_daily.msg_count 是群级、不分人。这正是用来对冲「只看处理量」的那一列，目前没有。",
    needs: "指标阶段按 (agent, dt, room) 统计 INTERNAL 消息条数，新增一列即可，不需要重跑模型。",
  },
  {
    title: "客服姓名",
    detail:
      "客服仍以 16 位 easyUserId 标识，尚未接入权威姓名。群名称已通过 official_room_id 关联群配置表获取。",
    needs:
      "一张人工维护的花名册映射表，或一个可查的人员接口；由 /api/meta 返回并置 alias_is_authoritative。",
  },
  {
    title: "工作时间口径",
    detail: "没有班表或工作日历，全站走自然时间，跨夜与周末照常计时。",
    needs: "一份工作日历（工作日、班次起止、节假日），首响与超时按工作时长重算。",
  },
  {
    title: "客服「当天实际参与」",
    detail:
      "agents[] 是事件级数组，没有逐条消息的参与日期，所以每日活跃量按事件 occurred_on 归属，跨天事件全部落在开始日。",
    needs: "同「回复消息数」，指标阶段按天统计每人的参与即可。",
  },
] as const;

/** 三个会给出「偏小但看起来正常」的数的口径洞，以及本看板的处理方式。 */
export const PITFALLS = [
  {
    title: "抽取失败的群日",
    risk: "metric_daily 事件级列是 NULL 不是 0，agent_metric_daily 整行缺失。直接求和会得到偏小的数。",
    handling: "每页顶部常驻完整性提示；热力图用斜纹格标出缺格；NULL 一律显示为长横线，绝不显示 0。",
  },
  {
    title: "对 agent 表求和",
    risk: "SUM(agent_metric_daily.event_count) 小于事件数：首响归属口径下，无响应的事件不落在任何人头上。",
    handling:
      "团队总量只用事件明细去重计数；客服页把「活跃量」与「首响归属」两列并排显示，并注明两者都不是解决量。",
  },
  {
    title: "平台发起的事件",
    risk: "asker_role = INTERNAL 的工单推送首响恒 0 秒且永远算已回复，混入会同时拉低分位数与无响应率。",
    handling: "首响、无响应、超时的分母一律是商家发起事件数；明细里单独标为「平台发起」。",
  },
] as const;
