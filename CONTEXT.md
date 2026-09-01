# Chat2Events — 领域

这份文档回答**「是什么」**：术语、业务场景、上游数据的形状、领域类型的字段契约。
换一套实现之后，这里的每一句话仍然成立。

- 工程约束（硬规则 / 承重不变量 / 明确不做）在 `CLAUDE.md`。
- 有过取舍的决策在 `docs/adr/`。

---

## 核心领域

### 处理单元是会话上下文，不是单条消息

这是整个项目唯一重要的前提。多条消息共同描述一个事件：

```text
商家A：5127366458053009229  加14个筒灯
商家A：[图片消息]
平台B：稍等
平台B：已加单
```

这是**一个**事件（加单 → 已受理 → 已加单），不是四个。任何按单条消息抽取的实现都是错的。

### 业务场景：家居服务派单群

**群里只有两方，都是客服，没有终端消费者。**

| `identityType` | 是谁 | 干什么 |
|---|---|---|
| `EXTERNAL` | **商家客服** | 把订单诉求发到群里（加单 / 改单 / 催单 / 换师傅 / 取消 / 加急） |
| `INTERNAL` | **平台客服** | 受理诉求，协调平台的师傅上门做**安装 / 维修 / 拆旧** |

师傅**不在群里** —— 平台客服在群外协调，师傅只在消息正文里被提到（「转给杨师傅」）。样本里 15 个商家客服 × 8 个平台客服。

指标口径不受影响：`agent` = `INTERNAL` = 平台客服，`first_agent_reply_time` 衡量的是**平台对商家的响应速度**。

### 什么不是业务事件

纯寒暄、单独的表情、与订单无关的闲聊。

⚠️ **这个群里这类内容很少**，绝大多数消息都是真实的派单往来。早先文档写的「非业务内容占群聊绝大多数，抽取器的主要工作是把它们丢掉」在派单群里**不成立**，照着写 prompt 会导致漏抽。

⚠️ **平台的「稍等」不是客套，是受理应答。** 全样本出现 172 次，占平台消息 34%，且是 `first_agent_reply_time` 的真实锚点。把它当噪声丢掉会让首响时效系统性偏大（从「稍等」推迟到「已安排」）。「已安排」「已加单」「已催促」「已反馈」「已取消」同理，都属于该事件。

宁可漏，不可编。抽取器无法确定时应返回空，不要生成低置信度的"疑似事件"来填充输出。

### 术语

| 术语 | 含义 |
|---|---|
| `message` | 一条原始消息，NDJSON 的一行 |
| `conversation` | 单个群跨文件、跨天拼接后的**连续**消息流 + 回复链 + @ 目标 |
| `batch` / `window` | 会话切出的一段，抽取模块的**内部**概念，不出现在任何接口上 |
| `event` | 抽取出的结构化业务事件。「问题」是 event 的一个子类型 |
| `occurred_on` | event 的归属日 = 首条来源消息的日期，同时是幂等分片键 |
| `taxonomy` | 版本化的类型词表 |
| `agent` | **平台客服**（`INTERNAL`），主键是 `easyUserId`。与**商家客服**（`EXTERNAL`）相对 —— 两边都是客服，群里没有终端消费者 |
| `师傅` | 平台的上门服务人员。**不在群里**，只在正文中被提及，没有 `easyUserId`，目前不进领域。正文里的师傅手机号由 `body` 掩掉；**姓名不掩**（掩姓规则已删，见 ADR-0001 与下文「`summary` 要求」）|

---

## 数据源事实

### 路径与发现方式

```
<env>/wechat-business-app/wecom-group-message/<yyyyMM>/<corpId>/<officialRoomId>.ndjson
```

**一个文件 = 一个群 × 一个月**，OSS **AppendObject**，每天往同一个文件尾部追加。
文件名 = `officialRoomId`。

**文件不靠 glob 目录发现，靠查 MySQL 索引表 `b_wecom_group_message_month_file`**：

| 字段 | 说的事 |
|---|---|
| `ndjson_object_key` | 相对 object key |
| `ndjson_position` | 下一批 AppendObject 预期字节位置 = **权威的文件末尾** |
| `ndjson_record_count` | 已确认追加记录数 |
| `file_month` | **消息月份** `yyyyMM`（不是接收月份 —— ① 的跨月读取依赖这条） |
| `ndjson_last_append_time` | 最近确认追加时刻（可为 NULL）。**早于窗口起点 = 本轮必然没有新消息**，直接不拉 |
| `file_status` / `is_deleted` | 0 正常 / 1 冻结 · 0 否 / 1 是；两个都是筛选条件 |

取数走 HTTP：`<DOWNLOAD_BASE_URL>/<ndjson_object_key>`，`Range` 增量。见 **ADR-0005**。

### NDJSON 字段（已由样本探明）

**这不是企业微信原始格式，是上游一层自建解析/映射的产物，带版本号。**

```
schemaVersion / parserVersion   bigint   均 = 1
messageKey                      varchar(64)     全表唯一（不使用）
sourceMessageId                 varchar(17-32)  企微原始 msgid（溯源键）
corpId / officialRoomId / easyRoomId / groupId   四个标识
messageTime                     bigint   毫秒，文件内严格升序
callbackReceivedTime            bigint   （不使用）
sender.identityType             INTERNAL / EXTERNAL，100% 填充
sender.easyUserId               varchar(16)  内外统一形态
sender.officialUserId           varchar(4-11) 形态混杂（手机号 / 字母账号），不使用
standardType / sourceMsgType    TEXT / IMAGE / GIF / VIDEO / ...
content                         原文；非文本为 [图片消息] / [GIF消息] / [视频消息] 占位符
analysisText                    上游「已清洗」的纯文本；非文本为空串（走 content 兜底，3742 条样本里 536 条）
semanticPayload.replyTo         回复目标，指向 sourceMessageId
semanticPayload.mentions[]      @ 目标，带 easyUserId
semanticPayload.segments[]      TEXT / MENTION 分段
semanticPayload.mediaItems[]    含 fileAesKey / temporaryDownloadUrl（不使用，见 open-questions.md 第 3 条）
```

样本分布（3742 条那一版）：TEXT 85.6% · IMAGE 14.0% · GIF 0.2% · VIDEO 0.1% · **FILE 0.05%**。

⚠️ **`standardType` 的取值集合是开放的。** 真实 dev 数据里还出现过 `RICH_MEDIA`。
① 对没见过的类型**原样通过**（正文走 `content` 兜底）并每类每轮打一条日志 ——
「什么算业务事件」是 ③ 的活，① 不做业务判断。

⚠️ **`analysisText` 的「已清洗」不可信，本仓库不继承它的清洗结果。** 实测 1850 条：28 条仍残留 `@` 标记；20 条把**被引用的整条原文连同真实姓名**嵌了进来（`"王鸿江：\n<原文>"\n------\n<回复>`）。清洗由 `body` 自己做，见 ② 抽取。

⚠️ **`mentions[]` 只有 `easyUserId` 和 `officialUserId`（后者全 `None`），没有姓名字段。** 所以无法把正文里的 `@王鸿江` 映射回某个 `easyUserId` —— `body` 把 `@名字` 换成固定占位 `@某人`，不做映射。（按位置对应 `第 k 个 @名字 ↔ 第 k 个 mention` 在 15/16 条上成立，但那第 16 条是「同一个人 @ 两次」，映射会给模型一个假身份。）

> ⚠️ 样本中**没有**撤回、系统消息、语音、链接、合并转发。这些类型的结构未验证。
> （`FILE` 已经出现在样本里，`RICH_MEDIA` 出现在真实 dev 数据里。）
>
> ⚠️ **样本会被就地替换。** 当前这版：3742 条 · 跨 5 天（2026-08-25 ~ 08-29）· 时间戳**真实**。
> 文档里带条数的实测数字必须注明是哪一版样本量的。

---
---

## 契约

### 领域 `Message`

**八个字段，每一个都有读取点。端口上每多一个死字段，就是向未来每一个适配器收一次税。**

```
msg_id        <- sourceMessageId          溯源键；replyTo 指向的就是它
room          <- officialRoomId
corp          <- corpId
at            <- messageTime（ms -> timestamp，业务本地时区）
sender_id     <- sender.easyUserId
sender_role   <- sender.identityType      Role::Internal / Role::External
text          <- COALESCE(NULLIF(analysisText,''), content)
reply_to      <- semanticPayload.replyTo.sourceMessageId
```

`sender_role` 的契约：**是领域枚举 `Role`，不是字符串**（Rust 版 `ingest::Role`）。
上游 `identityType` 只有 `INTERNAL` / `EXTERNAL` 两个值，认不出的值 → **该群失败**，
不兜底成任意一边 —— 判成 `Internal` 会把商家算进 `agents`，判成 `External` 会让平台的
回复不再算首响，两个方向都是静默把指标写歪。解析只发生在适配器的取值点那一处，
下游全是 `match`。`event.asker_role` 那一列存的仍是 `INTERNAL` / `EXTERNAL` 原样字符串
（`Role::as_str`），库里的历史数据不受影响。

`text` 的契约：**恒非空**，是这条消息可读的正文；非文本消息给 `[图片消息]` 这样的占位符，保上下文连贯。
「`analysisText` 可能是空串」是**上游的形状**，兜底做在适配器的 `SELECT` 里，不外泄 —— 领域里只有一个文本字段，第二个适配器不用去猜两个有什么区别。

**已删除的字段**（曾经在契约里，全项目零读取点 —— 删的是税，不是功能）：

| 字段 | 为什么删 |
|---|---|
| `msg_type` | 定义在三处，读取点 **0**。`[图片消息]` 占位符来自 `text`，不来自它 |
| `mentions[]` | 读取点 **0**。唯一提到它的是一句注释，内容是「**为什么不用它**」（只有 id 没有姓名，映射会给模型假身份） |
| `plain_text` | 存在的唯一理由是当 `text` 的兜底，已并进适配器的 `COALESCE` |

**不进领域**：`messageKey` · `easyRoomId` · `groupId` · `sourceMsgType` · `standardType` · `callbackReceivedTime` · `sender.officialUserId` · `mediaItems` · `segments` · `mentions` · `location` · `subMessages`

### 领域 `Conversation`

```
msgs             list[Message]，按 at 升序，一个群在窗口内的完整会话
msg_counts       dict[date, (msg_count, sender_count)]，消息级指标搭同一趟车
```

**已删除的字段**（同上表的规矩 —— 全项目零读取点，删的是税不是功能）：

| 字段 | 为什么删 |
|---|---|
| `corp` / `room` | 读取点 **0**。调用方得先有 corp/room 才调得动 `read_room`，`Event` 的那两列来自 `Message` |

**接口粒度 = 群 × 一次运行的完整会话 = 失败隔离粒度 = ③ 的输入。** 四者必须相等。

`sender.officialUserId` 不进领域，意味着 prompt 里**发言人**是角色化匿名标签（`平台A` / `商家B`）。要看某个 `easyUserId` 是谁，回 raw 区查 —— 低频人工操作。

⚠️ **这只匿名了群里的人。终端消费者的 PII 全在正文里** —— 实测 1850 条：193 条（10.4%）带客户手机号、88 条带门牌号级住址、101 处真实姓名（客户 / 师傅 / 群成员自己）。「模型看到的不是人名或手机号」这句话曾经是**破的**，正文原样进 prompt。现在由 `body` 保证，见 **ADR-0001**。

⚠️ **标签必须按整群算一次，不能按段算**（`labels`）。曾经这段逻辑住在 `render` 里、计数器每次调用归零：1096 条切 3 段实测 **32 处标签冲突**（段 1 的「平台B」和段 2 的「平台B」是两个不同的人）、**30 处身份漂移**（同一个人从「平台C」变成「平台F」），二分时每一半还会再洗一次。便签花大力气把跨段事件接住，接住之后模型看到的却是一套洗过牌的角色表 —— 等于在最需要连贯的地方引入了不连贯。样本 23 个发言人，A–Z 装得下。

### `Event`

**事实列**（冻结区不可写）

```
id                      行标识（数据库自增）
corpid
roomid
source_msg_ids          非空；每个 ID 必须真实存在于该次抽取的消息里
first_msg_time          首条来源消息时间
last_msg_time           末条来源消息时间
first_agent_reply_time  首条 INTERNAL 来源消息时间，可空 —— 首响锚点
occurred_on             = date(first_msg_time)，报表归属日 / 幂等分片键
asker                   提问方 easyUserId
asker_role              提问方角色：EXTERNAL=商家发起 / INTERNAL=平台发起（工单推送类）
agents[]                涉及的全部 INTERNAL 成员，全存
first_responder         first_agent_reply_time 那条消息的 sender_id，可空
summary                 事件摘要
```

**标注列**（任何时候可写，**但只有词表升版这一个原因**）

```
event_type
taxonomy_version
```

⚠️ 列名形态受公司《数据库规范》约束（时间字段以 `_time` 结尾、不得裸用 `type`、表名带 `b_` 前缀）
—— 本仓库适用条款与四条已取下的例外见 `docs/database-conventions.md`。

- **时间一律取自来源消息的真实时间戳**，不采信模型自己写的时间。
- `summary` 归**事实列**：它由抽取那一次的模型决定，而冻结区本来就不再跑抽取。这保证冻结区的 `sha256(summary)` 缓存永远命中。

### `summary` 要求

中文 · 一句话 · ≤ 100 字 · 只描述发生了什么和当前状态 · 不含推测 · 不含 ID · 不含脱敏占位符。

**「不含人名」这一条已经撤销。** 原始理由是「模型只看到匿名标签，天然保证」—— 而 `body` 决定不掩正文里的姓名（见 **ADR-0001**），那个前提就没了。实测 476 个 summary 里 **18 条（3.8%）含师傅姓名**（「商家要求订单转给杨师傅，平台已安排」）。

允许它，因为**师傅姓名就是这件事的内容**：去掉「杨」就变成「转给某师傅」，同一订单先后转给两个师傅时区分不了。且师傅是平台的服务人员不是终端消费者，姓 + 称谓不足以定位到个人。**终端消费者的姓名仍然不该出现** —— 那类信息在正文里要么带字段名（已被 `FIELD` 掩成 `<略>`）、要么紧邻手机号（手机号已掩，姓名留着但模型没有理由抄进摘要，实测 0 条）。

**其余三条都有 validator**（Rust 版是 `extract::model::validate`），不靠 prompt 里叮嘱。曾经只校验了长度那一条，另两条纯靠 prompt —— 实测 282 个 summary 确实一条没漏，但那是「没有护栏」不是「已经安全」。`summary` 归**事实列**：冻结区不可写，且 `sha256(summary)` 是 classify 的缓存键 —— **PII 一旦进去就是永久的，缓存还会把它焊死**。所以挡在 validator，**不做落库前 scrub**（那会改内容、让缓存键漂掉）。校验失败把报错原文回灌进下一轮 prompt。

占位符也要挡：正文脱敏后模型看到的是 `<手机号>` / `<略>`，抄进 summary 就成了「商家发来`<手机号>`」—— 不是泄漏，但那是脱敏留下的记号不是内容。

### 指标口径

```
首响时效 = first_agent_reply_time - first_msg_time
未回复   = first_agent_reply_time IS NULL
```

确定性计算，不需要模型判断。**「解决时长」不做** —— 需要先定义「什么算解决」。

```
agent_attribution = first_responder | all_participants     # 默认 first_responder
```

**这个开关只作用于 ⑥ 指标，不作用于 ③ 抽取。** 事实全存，解释随时可重算 —— 换口径不用重跑 LLM。


---

## 词表生命周期

| 阶段 | 状态 | 能出的指标 |
|---|---|---|
| **v0** | 还没有词表，全部 `__untyped__` | 消息量 · 问题总量 · 首响时效 · 客服处理**总量** |
| **归纳** | 攒够 event → LLM 读 `summary` 产出**带名字带描述的草稿** → 业务审阅改名增删 → 定 v1 | — |
| **v1+** | `recompute --taxonomy-version v1` 重打标 + 重算指标 | 以上全部 **+ 分类明细** |

- **v0 期不是缺陷，是明确的上线阶段。** 系统在任何阶段都能完整跑通，不需要等词表。
- **词表是分类明细的前置，打标不是。** 打标每天自动发生（⑤ 分类）。
- **两种 `__untyped__` 严格区分**：
  - `v0` + `__untyped__` = 还没有词表，**系统状态**
  - `vN` + `__untyped__` = 有词表但归不上去，**数据信号**（词表覆盖不足）
- **验收判据**：`vN` + `__untyped__` 占比超过阈值即需升版。阈值数值等真实数据，但**上线前必须定死一个数**，并做成可查数字，否则没人会去看。
- **聚类做发现，分类做打标。** 簇不是类型 —— 簇经过人工命名才变成稳定的词表。词表定下后不再漂移，新 event 只做分类，不参与重新聚类。聚类的角色是**"检查词表漏了什么"**，不是"产出第一版词表"。
- **人工加类只能通过升版。** 不做"给现有版本热加一个类" —— 那会让同一个版本号在不同时间对应两套词表，`taxonomy_version` 就失去意义。
- v1 归纳用**全量** event，之后升版用**滚动窗口**（如最近 3 个月，理由是代表性而非成本）。
- 词表用一个**手写文件**（yaml / sql）维护，人工执行插入。不做词表管理界面。

---

