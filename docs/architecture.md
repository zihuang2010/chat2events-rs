# Chat2Events — 模块布局与存储

七阶段的**每个模块内部长什么样**、端口上什么东西不许出门、MySQL 每张表的键为什么是那样。

- 全局地图（七阶段图 + 端口表）在 `CLAUDE.md`。
- 领域类型的字段契约在 `CONTEXT.md`。
- 承重不变量在 `CLAUDE.md` —— 这里的每条设计都得让路给它。

## 为什么是这几个接缝

**① 的「以后可能换数据源」是减法不是加法。** 不加基类、不加注册表、不加 `SOURCE_TYPE` 配置项，**也不加 `MessageSource` trait**（一个适配器 = 假想接缝；契约是文字的价值，写在模块文档注释 `//!` 上）—— 只保证四样东西不出 `ingest/`，且各自只住一个文件：**DuckDB 连接 · SQL · 上游字段语义**在 `read.rs`、**路径布局**在 `layout.rs`。换源那天写一个新文件实现三个方法，其余六个模块一行不动。

**② 有阶段名但不独立成模块。** 按群分组必须下推给源 —— 只有源知道数据怎么摆的（上游就是**一个群一个月一个文件**，分组是免费的）。写一个通用分组器就得先把全部消息读进内存，直接撞硬规则。**五个阶段，四个模块。**

**④ 明确不给端口。** 它是「把模型给的行号换成真实事实」的唯一执行点，也是承重不变量 6（溯源）的守卫。给它一个可替换的接缝，等于给溯源留一个绕过口。**有些地方不给接缝才是设计。**

读侧是 ① 的端口（今天的适配器是 DuckDB），写侧是 MySQL，职责不混：适配器只负责扫描和过滤原始消息，不承担结果存储；MySQL 只存抽取结果与指标，不参与扫描计算。


---

## 模块布局

### ① 摄取 ingest ＋ ② 会话 conversation

```
list_rooms(raw_root, window)                    -> [(corp, room)]
read_room(raw_root, corp, room, window)         -> Conversation
read_by_ids(raw_root, corp, room, window, ids)  -> [Message]      # webUI 下钻
```

拉取见 **ADR-0005**：查索引表 → HTTP `Range` 增量 → 本地是 OSS 的**字节级镜像**
`<raw_root>/<yyyyMM>/<corpId>/<roomId>.ndjson`，「已拉到第几字节」= 文件大小。

- **形态**：本地 NDJSON 上盖 DuckDB 的 `read_json_auto`，列名即领域名。**不物化**。
  **一个群一个月一个文件**，所以「R 个群各查一次」= 各碰各的几 MB，总 I/O 本来就是一遍 ——
  不需要一次全量排序再切片。实测（48 × 样本放大到 200 MB）：**537 MB/s** 全投影 + `ORDER BY`，
  单文件固定开销 **35 ms**。外推 1000 群月末整月 ≈ **70 秒**。
  **Parquet 的触发条件写死：一次跑批的扫描总时长超过 5 分钟。**
- **上游 `camelCase` 字段名只允许出现在这个适配器的 `SELECT ... AS ...` 里。** 其他任何模块出现上游字段名都是错的。
- 启动即断言 `schemaVersion` / `parserVersion`，不匹配**直接失败退出**。不做兼容层、不做字段名回退。
- **端口只有三个方法，四样东西一律不出门**：
  | 不许出门的 | 出门了会怎样 |
  |---|---|
  | `duckdb::Connection` | 调用方能对着它另写一条 SQL，换源即全线崩 |
  | SQL | 逼着未来每个适配器都得支持 SQL |
  | 路径布局（`<yyyyMM>/<corpId>/`） | 路径一变，跑批和 webUI 都要改 |
  | 上游字段语义（如「`analysisText` 可能是空串」） | 兜底逻辑跑到抽取模块里去，换源时没人知道要复现这个怪癖 |
- **`Conversation` 带 `msg_counts`**（每天多少条 / 多少个发言人），一次扫描搭同一趟车 —— 例外说明见 `CLAUDE.md` 硬规则「不把大数据集读进内存」。
- 出口 = 领域 `Message` / `Conversation`（契约见 `CONTEXT.md`）。

**Rust 版的五条实现约定**（都是为了让上面那些约束由构造保证，而不是靠人记得）：

- **窗口是 `window::Window` 类型，不是裸 `&[NaiveDate]`。**「非空、连续、升序」由
  构造保证（`new` 供跑批、`span` 供下钻/测试），使用点不再各自 `min()/max()/first()`
  重推前提 —— 曾经 `read_by_ids` 对空窗口是可达 panic。

- **SQL 是 `select_sql!` 宏，不是 `const`。** 这样拼装那句 `format!` 能在编译期校验
  五个占位符。用 `const` + `.replace("{since}", …)` 的话，占位符打错一个字母会原样
  带进 SQL、到 DuckDB 才报解析错；而且 `.replace` 有先后顺序，先插进去的内容会被
  后面几次 replace 再扫一遍。SQL 仍然是文件顶上一个具名的东西。
- **取列按列名，不按下标**（`r.get(COL_MSG_ID)`），且 SELECT 列表与取值点引用
  **同一组 `COL_*` 常量**（`mirror` 的 6 列同理）—— 列名打错从「运行期
  `InvalidColumnName`」提前到编译错误。「列名即领域名」必须在读取点也成立 ——
  12 列里 8 列都是字符串，下标错位类型兼容、编译通过、守卫也放行，是静默的;
  列名打错则是 `InvalidColumnName`，当场炸。
- **月份由 `files()` 一路带进 `scan()`，不从 `filename` 反解。** 布局只被「拼」一次
  （`room_path`），没有第二处再去「拆」它。`EXT` / `MONTH_FMT` 两个 const 同理。
- **`sender_role` 是 `Role` 枚举，不是 `String`。** 解析只在取值点发生一次，认不出的
  `identityType` 是该群失败（不兜底成任意一边）。理由是**错法静默**：`== "INTERNAL"`
  打错一个字母，`labels` 会把平台客服全标成商家、`assemble` 的 `agents` 恒空、
  ⑥ 的首响 p50/p90 全 `NULL`，三条链路一起坏而编译器不吭声。落库仍走 `as_str()`，
  库里那一列的取值一字未变。
- **必填字段包含 `text`。** 契约头一条是「`text` 恒非空」，而它是 `COALESCE` 兜完底的
  结果 —— 到读取点还是空，说明上游连占位符都没给。NULL 和空串都算缺失
  （`content` 是空串时 `COALESCE` 返回的就是空串，只判 NULL 漏得掉）。

### ③ 抽取 extract ＋ ④ 装配 assemble

**Rust 版分了文件，但接口一字未动** —— 对外仍然只有 `Event` / `SegmentModel` /
`extract`（＋ 对拍用的 `preview`）。拆的是**导航成本，不是深度**：

```text
extract/mod.rs       模块文档 · mod 声明 · pub use —— 一行生产代码都没有
        types.rs     EventDraft · Draft · Event · SUMMARY_MAX
        pipeline.rs  调用链：分段 → 段调用 → 自适应二分
        model.rs     端口 SegmentModel · 校验 · LiveModel（端点知识只在这里）
        redact.rs    正文脱敏与订单号正则（ADR-0001）
        prompt.rs    SYSTEM —— 逐字搬运，改一个字所有带数字的实测结论同时作废
        render.rs    便签 · 匿名标签 · 行号箭头，view 是唯一出口
        segment.rs   分段与切点选择（ADR-0004）
        assemble.rs  ④ merge / align / assemble / orphans
```

`model.rs` 单独成文件是**接缝本身**：换模型 / 换端点只改它一个，`pipeline.rs` 的
分段与二分逻辑一行不动 —— 「什么信号算这一段太大」是端点知识，「太大就切」不是。

`prompt.rs` 单独成文件是有意的：那句「改一个字实测结论全作废」得贴在改它的人眼皮底下。


```rust
async fn extract(msgs: &[Message], model: &impl SegmentModel, segment_msgs: usize)
    -> Result<Vec<Event>, BoxError>          // Ok 可能为空 = 这几天确实没有业务事件

trait SegmentModel {
    async fn call(&self, text: &str, segment_size: usize, open_refs: &BTreeSet<u32>)
        -> Result<Vec<EventDraft>, SegError>;
}
```

模型调用走端口 `SegmentModel`，**两个适配器**：`LiveModel`（生产，真实调用）·
`BisectStub`（测试桩，在 `extract/tests.rs`，只在自检里用）。

⚠️ **Recording / Replay（录音回放）没搬。** Python 版另有两个适配器（`--record` /
`--replay` 互斥入口、内容寻址的 JSONL 录音、回放时重过 validator、必须能重放
`TooBig`、录音永久保留不写清理），本仓库今天没有 —— 要搬时照 Python 版来，
录的仍是 `body` 的产出（已脱敏，不引入新的 PII 外流面）。

- **端点知识跟着端点走**：把 async-openai 的错误翻译成「输出被截断」/「超时」两种信号住在 `LiveModel` 里 —— 换端点写法就变。`TooBig` 这个信号本身留在本模块，由二分逻辑消费：「太大就切」跟谁家端点无关。**「不认连接类错误」这条一起归 Live**（网络断了切成两半也一样断，当成「太大」会让一次故障放大成一整棵调用树）。
- **自检也走这个端口**：`BisectStub` 按 `segment_size` 抛 `TooBig` 逼出二分。它每段返回 `msg_indexes=[1, segment_size]`，**真实的 `merge` 把它换算成 `{lo, hi-1}` 写进 drafts** —— 实际跑过的区间从 drafts 读回来，于是「划分性质」不需要打桩 `one_call` 也断言得了。
- **断言 `cargo test` 就跑得到**：跨文件的性质测试与共享 fixture（含 `BisectStub`）在 `extract/tests.rs`，各实现文件的单元测试在各自文件底部；样本布局用 `testutil`（`fresh_root` / `write_month`）摆成生产形状。

- **接口粒度 = `Conversation` = 群 × 一次运行的完整会话 = 失败隔离粒度**。四者必须相等。
- 内部（对调用方完全不可见）：自适应二分 · 段间便签 · 调模型 · 序号↔`msg_id` 映射 · schema 校验 · 溯源校验 · 重试。
- **「一条消息的正文长什么样」只有一个出口：`body(m) -> String`。** 纯函数，是把正文交给模型的唯一通道。
  只做五件锚点确定的事（删引用块 → `@名字` → 手机号 → 折行 → 结构化字段值），顺序承重；
  **姓名和自由文本地址明确不掩**。规则全文、顺序依赖的理由、实测数字见 **ADR-0001**。
- **「模型这一段看到什么」只有一个出口：`view(msgs, lo, hi, drafts, segment_msgs) -> (text, open_refs)`。** 便签淘汰、段外引用解析、渲染、`open_refs` 这四件事必须彼此一致（`open_refs` == 便签的 ref 集合；`outside` 的 `E<ref>` 只能来自便签；`segment_size` == 段长），曾经平铺在 `one_call` 里靠调用点手写维护 —— 没有模块负责守，也没有地方能测（不桩掉模型就跑不到）。收进来之后由构造保证，`one_call` 只剩三步：建视图 / 发请求 / `merge`。
- **`--dry` 走的是同一条 `view`**，按真实分段逐段渲染（便签为空 —— 它只有跑过模型才有内容）。曾经 `preview` 是 `render(整群)`，那个 prompt 在生产里从不发生：**看的不是要发的东西**。
- **模型只被允许输出四样东西**：段内序号列表 · `summary` · 接哪条便签（`ref`）· 完没完（`still_open`）。
  前两个是内容，后两个是控制。**其余 11 个字段全部由 ④ 装配从真实消息算出，一个都不采信模型。**
- 本模块**不碰 embedding**。

### ⑤ 分类 classify

**Rust 现状（v0）：无端口、无构造器** —— 两个常量（`CURRENT_VERSION = "v0"` /
`UNTYPED`）加一个模块级函数 `classify(summary) -> &'static str`。一个适配器 =
假想接缝，论证在 `classify.rs` 模块注释（CLAUDE.md 的端口表同）。

**v1 落地时再引接缝**：那时它要拿住三样状态 —— 词表（MySQL
`b_merchant_group_taxonomy` 表）、`sha256(summary)` 结果缓存、embedding 缓存 ——
纯函数拿不住，「一次运行构造一次、不是模块级函数」到那天才成立。
⚠️ 届时下游那两条「构造保证 types 与 events 等长」的 `assert` 前提也跟着翻转
（classify 变成可失败的），要一起改走 `Result`。以下几条从 v0 起就成立、v1 不变：

- **确定性是硬约束**：同样的 `summary` + 同样的词表 → 永远同样的 `type`。
- 结果缓存：`sha256(summary) + taxonomy_version -> type`（与 embedding 缓存同一模式）。
- **每次 event 落库都调，包括分片删重写那一次。** 标签不是刻在 event 上的，是每次查出来的 —— 所以分片重写不会丢标签。
- **但打标发生在事务外**，由 `daily` 算好后传给 ⑦。三件事一起解决：
  - **长事务** —— `write_room` 已经 `DELETE` 完分片行，在同一个事务里逐个 event 调 classify，缓存未命中就是一次 embedding HTTP 请求。**持锁发 N 次请求。**
  - **依赖成环** —— classify 要读词表就得 `import store`，而 store 已经 `import classify`。打标提出来之后 store 不再 import classify，环消失。
  - **算两遍** —— 同一个 event 的 type，⑦ 落库一次、⑥ 指标一次，没有共享缓存。
  ⚠️ 这**不违反**上面两条：仍然每次落库现算（只是位置从事务里挪到事务外几行），type 仍不进 `Event`。变的只有「谁调用它」。
- 内部算法不限死（embedding 比 centroid / LLM / 关键词皆可）。确定性由缓存层保证，不由算法保证。

### ⑥ 指标 metrics —— 指标表的唯一来源（写库 SQL 在 ⑦）

- **纯函数模块：零 IO、零 SQL、零 `duckdb`。**
- 两个来源：**事件级**指标读 `Event`；**消息级**指标读 `Conversation.msg_counts`（不依赖抽取，失败的群照样有）。
- 两个入口：
  - `daily` 跑完自动调用，scope = 本次运行的两天
  - 手动重算（`--taxonomy-version vN` 重打标 / `--attribution X` 换归属口径）——
    ⚠️ **这条 CLI 没搬**（Python 版的 `recompute`，见 `docs/status.md` ⑥ 行）。
    事实全存，落地那天不用重跑 LLM；今天生产恒走默认口径 `first_responder`
- 指标表**不受分片冻结约束** —— 它依赖的事实全都还在，随时可整体重算。

### ⑦ 落库 store —— MySQL 唯一写入方

- **所有写库 SQL 都在这一个文件里，一条都不许外流。** 不是「存储层抽象接口」—— MySQL 是当前唯一目标（见 `CLAUDE.md`「明确不做」），这里只保证写库代码集中在一处。
- 建表走手写的 `schema.sql`（**本仓库根目录，权威的那份**），人工执行一次；
  本模块不碰 DDL，但启动期 `check_schema` 对四张会写的表做列级自检 ——
  schema 漂移在 Python 版害过一次（抽取跑完 23 分钟才在 ⑦ 炸掉），现在第一秒暴露。
  ⚠️ `../pychat2events/schema.sql` 是搬运前的原件、此刻逐字节相同；按「移过来不是
  抄过来」的原计划该删，删除跨仓库，留给人工。**改 DDL 只改本仓库这份。**
- 一个群一次运行的**全部**写入，**一个事务**（承重不变量 2）。

### 三个进程

| 进程 | 触发 | 干什么 | 失败语义 |
|---|---|---|---|
| `mirror` | `daily` 的第一步；补数时单独跑（子命令待定） | 查索引表 → HTTP `Range` → 本地镜像 | **群 × 日隔离**（与抽取失败同一条路径）。索引表连不上是整轮失败 |
| `daily` | 每日定时 | ① → ③④ → ⑤⑥ → ⑦ | **群 × 日隔离**，整轮继续 |
| `taxonomy` | **人工触发** | **只产词表，不写 `b_merchant_group_event` 表** | 失败无所谓，不阻塞任何人 |

---

---

## MySQL 表结构

| 表 | 键 | 要点 |
|---|---|---|
| `b_merchant_group_event` | `idx_shard (corpid, roomid, occurred_on)` 分片删重写 | `source_msg_ids` 用 JSON 列，不拆关系表 |
| `b_merchant_group_metric_daily` | `uk_group_daily (corpid, roomid, dt)` REPLACE 覆盖 | 加一列 `extraction_status` |
| `b_merchant_group_agent_metric_daily` | `uk_agent_daily (corpid, room, agent, dt, event_type, taxonomy_version)` **六列** | 加 `room` 使其嵌套进失败隔离粒度 |
| `b_merchant_group_taxonomy` | `uk_taxonomy (version, type_id)` | `name` 必填 · `description` **必填** · `centroid` 可空 |
| `b_merchant_group_run_failure` | 追加 | `(run_date, corpid, roomid, reason)` |

**`b_merchant_group_agent_metric_daily` 为什么是六列**

- `event_type` 进语义键 —— 否则一行只能存总量，存不了「每类各多少个」。
- `taxonomy_version` 进语义键 —— 词表会升版重打标，不记版本这张表就是一堆无法解释的数字。
- **`room` 进语义键** —— 键必须嵌套在「群 × 日」的失败隔离粒度里，否则某个群失败时会用残缺数据覆盖完整数据。跨群总量查询时 `SUM`。
- `event_type` 为空时用显式的 `__untyped__`，**不用 NULL**。
- **不单独存总量行** —— 总量 = 求和。存两处会打架。
- ⚠️ **但 `SUM(event_count)` ≠ 当天事件数，即使这个群完全成功。** `first_responder`
  口径下，**未回复的事件不落在任何人头上**（`first_responder IS NULL` 时
  `metrics::agent_rows` 的归属集合为空、一行不记）。3742 条样本实测：事件 **956**、agent 表合计 **872**，差的 **84** 正好是
  `unreplied_count`。要「团队总处理量」读 `metric_daily.event_count`，别对这张表求和。
  **这和承重不变量 5 是两个不同的洞** —— 那个是失败的群整行缺失，这个是成功的群里
  **没人接的单**。
  ⚠️ `ALL_PARTICIPANTS` 口径不是「修好了」，是**反向偏**：同一样本实测合计 **976 > 956**。
  未回复的 84 个照样丢（它们 `agents` 也是空，实测 0/84 非空），但多客服事件被**按人重复计数**
  （+104）。两个口径都 `SUM` 不出事件数，**方向还相反** —— 这正是「不单独存总量行」的代价，
  总量只有 `metric_daily.event_count` 一个正确来源。

**`taxonomy.description` 为什么必填**

LLM 归纳产出的类只有名字和描述，没有 `centroid`；人工加的类同理。`description` 必填让 `classify` 不依赖向量也能工作；有 `centroid` 就多一条可用路径。

**建表**：一个手写的 `schema.sql`，人工执行一次。字段类型 / 命名 / 必须字段遵循公司《数据库规范》，适用条款与四条已取下的例外见 `database-conventions.md`。不用 `CREATE TABLE IF NOT EXISTS`（会掩盖"表结构变了但没迁移"）。**不引入 ORM 和 migration 框架。** 跑批进程只读写数据，不碰 DDL。

**原始消息不入 MySQL**。溯源靠 `sourceMessageId` 回 raw 区查（`read_by_ids`，单群单月 18 MB ≈ 70 ms）。
⚠️ 措辞修正：**唯一事实来源是 OSS，本地 `./data/raw/` 是它的镜像/缓存** —— 删了能重拉。
今天按**永久保留**跑（磁盘 1T，每天 600 MB、一年 216 GB）——**没有保留期配置项**，
占满手工清（清理逻辑要写要测，而它防的问题今天不存在）。

**embedding 不入 MySQL、不引向量库**。存内容寻址缓存 `sha256(summary) -> vector`，只增不减。10 万条 × 1024 维 f32 约 400MB，全量算余弦是秒级。

---

---

## webUI（只读旁路）

形态已定（2026-08-30）：**前后端分离 · 后端只读 JSON API · 全部 `GET` · 无登录版**（内网可达即可看）。
它与跑批解耦 —— 只从 MySQL 和 ① 的端口取数，**不写任何表、不调模型、不参与跑批**，跑批不知道它存在。
下钻原文走 `read_by_ids`，**不自己再翻译一遍上游字段名**。

- **API 层挣到自己位置的地方，是 `coverage`。** 客服处理量有**两个**都会给出「偏小但看起来正常」的洞：失败的群**整行缺失**不是 0（承重不变量 5），以及成功的群里**未回复的事件不落在任何人头上**（见上文六列那节，3742 条样本实测差 84/956）。直接求和两个都踩。这条今天靠每个查询的人记得去 join；有了 API，join 写在**一处**，而且**每个聚合响应都必须带 `coverage`，即使数据完整** —— 让调用方在结构上没法忘记处理它。
- **给模型的是脱敏正文（不变量 7），给人看的下钻是原文。** 两条路不同，是有意的：主管要核实首响，脱敏版核实不了（「客户 `<手机号>` 要求改期」看不出是哪一单），而主管本来就有权进那个群。
- ⚠️ **权限与认证本期明确不做。** 触发条件是有人提出「某某不该看到某某的数」。
- ⚠️ **未解**：领域里客服只有 `easyUserId`，样本里**没有任何字段带姓名** —— 界面上只能显示一串 ID。需要一张人工维护的映射表或一份人员花名册接口。**不解则客服维度页面没有意义**（群维度和事件明细不受影响）。
