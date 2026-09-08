# Chat2Events — 模块布局与存储

七阶段的**每个模块内部长什么样**、端口上什么东西不许出门、MySQL 每张表的键为什么是那样。

- 全局地图（七阶段图 + 端口表）在 `CLAUDE.md`。
- 领域类型的字段契约在 `CONTEXT.md`。
- 承重不变量在 `CLAUDE.md` —— 这里的每条设计都得让路给它。

## 为什么是这几个接缝

**① 的「以后可能换数据源」是减法不是加法。** 不加基类、不加注册表、不加 `SOURCE_TYPE` 配置项，**也不加 `MessageSource` trait**（一个适配器 = 假想接缝；契约是文字的价值，写在模块文档注释 `//!` 上）—— 只保证四样东西不出 `ingest/`，且各自只住一个文件：**DuckDB 连接 · SQL · 上游字段语义**在 `read.rs`、**路径布局**在 `layout.rs`。换源那天写一个新文件实现三个方法，其余六个模块一行不动。

**② 有阶段名但不独立成模块。** 按群分组必须下推给源 —— 只有源知道数据怎么摆的（上游就是**一个群一个月一个文件**，分组是免费的）。写一个通用分组器就得先把全部消息读进内存，直接撞硬规则。

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

无人值守跑批使用内部 `read_synced_room(raw_root, corp, room, window, months)`：
`mirror::sync` 返回本轮同步成功的群与月份，`daily` 将该范围直接交给读取，不再扫描目录重新决定名单。
同群任一月份同步失败则整群排除；已确认同步的文件随后缺失则读取报错，不能用残缺月份继续跑。
`list_rooms` / `read_room` 保留给本地检查，`read_by_ids` 仍可读取保留期内未参与本轮跑批的历史文件。

拉取：查索引表 → HTTP `Range` 增量 → 本地是 OSS 的**字节级镜像**
`<raw_root>/<yyyyMM>/<corpId>/<roomId>.ndjson`，「已拉到第几字节」= 文件大小。
本地副本行尾不完整或长度超出索引时显式作废重拉；追加或 `fsync` 失败也清除不可信副本。本地检查、完整性校验与写入在阻塞线程池执行。

- **形态**：本地 NDJSON 上盖 DuckDB 的 `read_json_auto`，列名即领域名，不预建中间表。
  查询会物化窗口过滤、排序后的单群结果；共享实例与内存上限，读取在线程池中执行。
  **一个群一个月一个文件**，所以「R 个群各查一次」= 各碰各的几 MB，总 I/O 本来就是一遍 ——
  不需要一次全量排序再切片。实测（48 × 样本放大到 200 MB）：**537 MB/s** 全投影 + `ORDER BY`，
  单文件固定开销 **35 ms**。外推 1000 群月末整月 ≈ **70 秒**。
  **Parquet 的触发条件写死：一次跑批的扫描总时长超过 5 分钟。**
- **上游 `camelCase` 字段名只允许出现在这个适配器的 `SELECT ... AS ...` 里。** 其他任何模块出现上游字段名都是错的。
- 启动即断言 `schemaVersion` / `parserVersion`，不匹配**直接失败退出**。不做兼容层、不做字段名回退。
- **四样东西一律不出门**：
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
- **取列按列名，不按下标**（`r.get("msg_id")`），列名直接写字面量，不另设 `COL_*` 常量。
  真实文件上的摄取测试执行 SQL 和取列，列名漂移会使测试失败；按下标则可能静默错配同类型字段。
- **月份由 `files()` / `synced_files()` 一路带进 `scan()`，不从 `filename` 反解。** 布局只被「拼」一次
  （`room_path`），没有第二处再去「拆」它。`EXT` / `MONTH_FMT` 两个 const 同理。
- **`sender_role` 是 `Role` 枚举，不是 `String`。** 解析只在取值点发生一次，认不出的
  `identityType` 是该群失败（不兜底成任意一边）。理由是**错法静默**：`== "INTERNAL"`
  打错一个字母，`labels` 会把平台客服全标成商家、`assemble` 的 `agents` 恒空、
  ⑥ 的首响 p50/p90 全 `NULL`，三条链路一起坏而编译器不吭声。落库仍走 `as_str()`，
  库里那一列的取值一字未变。
- **缺时间的消息不能先被窗口过滤丢掉。** SQL 保留 NULL 时间行，由必填守卫报告群级失败；真实 NDJSON 回归测试覆盖缺字段和显式 NULL。

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
        redact.rs    正文脱敏与订单号正则
        prompt.rs    SYSTEM —— 逐字搬运，改一个字所有带数字的实测结论同时作废
        render.rs    便签 · 匿名标签 · 行号箭头，view 是唯一出口
        segment.rs   分段与切点选择
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

`EventDraft` 的字段仅在 crate 内可见；外部适配器通过 `EventDraft::new` 复用已有校验，传入本次调用的段长与便签集合。
构造后不能从 crate 外修改字段；编译失败文档测试锁住这一约束，测试适配器也使用公共构造入口。

⚠️ **Recording / Replay（录音回放）没做。** 真要做，形状是两个额外的 `SegmentModel`
适配器（`--record` / `--replay` 互斥入口、内容寻址的 JSONL 录音、回放时重过 validator、
必须能重放 `TooBig`、录音要写清理）—— 录的必须是 `body` 的产出，
那样已脱敏，不引入新的 PII 外流面。

- **端点知识跟着端点走**：把 async-openai 的错误翻译成「输出被截断」/「超时」两种信号住在 `LiveModel` 里 —— 换端点写法就变。`TooBig` 这个信号本身留在本模块，由二分逻辑消费：「太大就切」跟谁家端点无关。**「不认连接类错误」这条一起归 Live**（网络断了切成两半也一样断，当成「太大」会让一次故障放大成一整棵调用树）。
- **自检也走这个端口**：`BisectStub` 按 `segment_size` 抛 `TooBig` 逼出二分。它每段返回 `msg_indexes=[1, segment_size]`，**真实的 `merge` 把它换算成 `{lo, hi-1}` 写进 drafts** —— 实际跑过的区间从 drafts 读回来，于是「划分性质」不需要打桩 `one_call` 也断言得了。
- **断言 `cargo test` 就跑得到**：跨文件的性质测试与共享 fixture（含 `BisectStub`）在 `extract/tests.rs`，各实现文件的单元测试在各自文件底部；样本布局用 `testutil`（`fresh_root` / `write_month`）摆成生产形状。

- **接口粒度 = `Conversation` = 群 × 一次运行的完整会话 = 失败隔离粒度**。四者必须相等。
- 内部（对调用方完全不可见）：自适应二分 · 段间便签 · 调模型 · 序号↔`msg_id` 映射 · schema 校验 · 溯源校验 · 重试。
- **「一条消息的正文长什么样」只有一个出口：`body(m) -> String`。** 纯函数，是把正文交给模型的唯一通道。
  只做五件锚点确定的事（删引用块 → `@名字` → 手机号 → 折行 → 结构化字段值），顺序承重；
  **姓名和自由文本地址明确不掩**。规则全文、顺序依赖的理由、实测数字
- **「模型这一段看到什么」只有一个出口：`view(msgs, lo, hi, drafts, segment_msgs) -> (text, open_refs)`。** 便签淘汰、段外引用解析、渲染、`open_refs` 这四件事必须彼此一致（`open_refs` == 便签的 ref 集合；`outside` 的 `E<ref>` 只能来自便签；`segment_size` == 段长），曾经平铺在 `one_call` 里靠调用点手写维护 —— 没有模块负责守，也没有地方能测（不桩掉模型就跑不到）。收进来之后由构造保证，`one_call` 只剩三步：建视图 / 发请求 / `merge`。
- **`--dry` 走的是同一条 `view`**，按真实分段逐段渲染（便签为空 —— 它只有跑过模型才有内容）。曾经 `preview` 是 `render(整群)`，那个 prompt 在生产里从不发生：**看的不是要发的东西**。
- **模型只被允许输出四样东西**：段内序号列表 · `summary` · 接哪条便签（`ref`）· 完没完（`still_open`）。
  前两个是内容，后两个是控制。**其余 11 个字段全部由 ④ 装配从真实消息算出，一个都不采信模型。**
- 本模块**不碰 embedding**。
- `LiveModel` 的本地 HTTP 测试覆盖校验回灌、截断与超时分流、连接错误不二分、便签传递；schemars 递归转换所有嵌套对象。抽取和分类共用 `rejection.rs`，逐字证据只回灌模型，日志与失败原因只保留规则摘要。

### ⑤ 分类 classify

**Rust 现状（v1）：一个 `Classifier`，一次运行构造一次，拿住三样状态** ——
词表（`store::read_taxonomy` 读好传进来）· 预渲染的 system prompt · 结果缓存。
当前唯一打标路径是**让大模型从封闭词表选择标签**：一批 50 条 summary、`{index, type_ids}`
结构化输出、校验不过回灌报错重问一次。最多 3 个标签，第一个是主类，副类不重复计入指标。
`daily` 与 `recompute` 都在启动时按指定版本从 MySQL 读取一次词表，交给同一个 `Classifier`。
数据库词表和人工草稿共用 `classify::check_types` 校验；只有显式 v0 允许空词表，正式版本缺失直接失败。
日常跑批先独立保存事实，再通过有界 channel 交接给 `daily/labeling.rs`。

```text
ingest.room_concurrency 个群：读取 → 段串行抽取 → write_room 保存事实
    → channel { corpid, roomid, event_count }
    → 按群 read_events → 去重、缓存 → 每批最多 50 条摘要
    → 全局 classify.concurrency 个批次 → update_event_labels 独立回写
    → 本群全部成功 → finish_classification 发布客服指标
```

两个并发配置均为 8 时，本进程最多同时有 8 个抽取请求和 8 个打标请求。
打标队最多持有 `classify.concurrency` 个群，channel 容量取同一个值；队列只存小任务，不存正文。
任务沿用本轮日期窗口与词表版本。事件数用于核对本次保存结果，不用于盲目按偏移更新；标签按读到的事件 ID 回写。
所有群共享批次名额，满批和不足 50 条的尾批都及时发送，不等待整轮抽取完成。
抽取侧结束后关闭发送端，接收端排空任务与在飞批次，再汇总两阶段结果。
同一群窗口的重叠跑批不在支持范围内；内存 channel 不自动恢复中断任务。
人工 `recover` 从群日未完成状态恢复（包括零事件），只补首次缺失标签，复用分类与发布路径。
它按群日处理，不覆盖中间已完成日期；与日常跑批、重打标错开执行。

**词表是两级的，但只有二级进 `event_type`。** `b_merchant_group_taxonomy.parent_name`
是一级分类名，`type_id` / `name` 是二级（叶子）。一级**不单独建行、不自引用**，
只是叶子上的一个属性列 —— 于是 `event_type` 存的仍是叶子，`uk_agent_daily` 六列
语义键和全部指标一个字不用动；报表要一级维度就 JOIN 词表取 `parent_name` 再 group by。
它在两处起作用：分类 prompt 里按一级分组列出（`## 一级` 标题 + 该组的二级条目，
所以 `Classifier::new` 的顺序归一是按 `(parent_name, type_id)` 排，同组必须连续），
以及 webUI 的一级下钻。**prompt 里明说 `##` 那行不是可选答案** —— 模型输出一级名当
`type_id` 会被 `validate` 当编造拒掉（封闭集合里没有它），走重问一次的既有通道。

**仍然没有 `Classifier` trait，是有意的。** 上一版这里写的是「v1 落地时再引接缝」——
那句话真正要的是**「一次运行构造一次的对象」**，一个 struct 已经满足。而 v0
**不需要第二个实现**：「还没有词表」在库里精确地等于「`b_merchant_group_taxonomy`
里 `v0` 没有行」，于是它就是 `types.is_empty()` 那一行 if。仍然是一个适配器 =
假想接缝，按本仓库自己的判据不写 trait。

- **确定性是硬约束**：同样的 `summary` + 同样的分类策略 → 复用同一份已缓存标签。
- **确定性由缓存层保证，不由算法保证。** 模型打标 `temperature = 0` 也不保证同输入同输出，
  而非冻结区每天重写 `[T-3, T-2]`、同一批 event 会被反复打标 —— 没有跨运行的持久缓存，
  报表就「抖动而非修正」，正是承重不变量 1 要防的那件事。**缓存是承重件，不是优化。**
- 结果缓存：`<cache_dir>/<version>-<策略指纹>.sqlite`，`sha256(summary) -> 标签全集`，
  主键按需读取，SQLite 页缓存目标 2 MiB；不再全量加载历史答案。旧同名 NDJSON 首次按行导入并保留。
  - **键是 `sha256(summary)` 不是 `event_id`** —— 用 id 会跟分片删重写冲突：重跑某个群某天，
    event 全删重建、id 全变，落盘的答案立刻变成孤儿，新 event 又没有标签。
    存 hash 不存原文的第二个理由是 PII：`summary` 里有客户姓名和地址（脱敏明确不掩），
    而缓存**只增不减**。
  - **策略指纹为完整 SHA-256**：包含归一后的词表提示词以及模型端点、模型名、推理设置、
    temperature、实际输出上限，不包含凭证。换策略会换缓存；每轮日志记录模型、词表、提示词和策略指纹。
  - 模型请求不持缓存锁；提交时采用第一个已提交答案，所有并发调用返回该答案。
    一个模型批次事务持久化成功后才返回；读盘与提交都在阻塞线程池中，竞争缓存锁时异步等待。
  - 同一文件持有进程级独占文件锁，第二个进程构造分类器时显式失败。日常、重打标和试打应错开运行。
  - 旧文件导入跳过坏行和未完成尾行，计数进日志；导入标记与答案同事务。持久答案读取继续校验，
    损坏时显式失败，不重新问模型；提交失败后本进程停止写入，重启交给 SQLite 事务恢复。
- **每次事实重写都重新安排打标**，标签不属于 `Event` 事实类型，初始三个标注列均为 NULL。
- **模型请求不持数据库事务**；每批标签独立提交，本群全部成功后再提交客服指标和完成状态。
- **打标失败保留已保存事实和成功批次标签**，记录单独的失败阶段；不生成兜底标签，不发布残缺客服分类指标。
- 后续训练使用沉淀的摘要和标签，先经人工抽查纠错及独立验证，再替换分类实现；不预建训练框架。

### ⑤ 的两个人工触发进程

| 进程 | 入口 | 干什么 |
|---|---|---|
| `taxonomy` | `examples/taxonomy.rs`（`summaries` / `review` / `emit-sql`） | `summaries` 看语料（有哪些说法、各多少条）→ **人手写** `taxonomy_<v>.toml` → `review` 试打产 `review_<v>.md`（未分类率 / 空类数）→ 不行就回去改 → `emit-sql` → 人工执行 `INSERT` |
| `recompute` | `examples/recompute.rs` | 词表升版后按新词表重打标：**只写标注列**，重算 `agent_metric_daily` |

**机器归纳两条路都放弃了**（2026-09-03）。词表是人手写的，
`review` 那一关不可省 —— `event_type` 一旦逐日漂移，就没有报表能建在这个维度上。

`recompute` 的两条边界值得单独记：
  * **只写标注列，事实列一个字节不动** → **不触碰承重不变量 1**，冻结区照样能重打标
    （那正是标注列的定义：「任何时候可写，但只有词表升版这一个原因」）。
  * **不需要承重不变量 2 的两分片同事务** —— 那条约束是关于 event 在分片之间移动
    （`occurred_on` 由模型判断的首条消息决定），而更新标注列不移动任何行。
  * 按数据库行 `id` 定位，按完整标签组合分组更新；`id` 与 `Event` 并排传递，不混进事实类型。
  * **按群分批读** —— 重打一个季度上千个群，一次读全量就是几百万个 `Event` 同时在内存里。
  * **只重打标，不训练** —— 命令只接受可选并发度；未命中缓存的摘要全部交给大模型。

**机器归纳整条线已放弃**（A：LLM 树状 map-reduce，2026-09-02 删；B：本地 embedding +
HDBSCAN + LLM 命名，2026-09-03 删）。B 真跑出过一版 16 个类的词表，实测**不理想**：
整句 embedding 被宾语名词主导，「加单」这一件事按商品品类被切成 7 个类（占 18% 的事件），
「有没有发图」这种消息形态也成了一个类 —— 而 `metric_agent_daily` 按 `event_type` 分组，
同一件事的量摊进 7 行，看不出总量。取舍与实测全文

### ⑥ 指标 metrics —— 指标表的唯一来源（写库 SQL 在 ⑦）

- **纯函数模块：零 IO、零 SQL、零 `duckdb`。**
- 两个来源：**事件级**指标读 `Event`；**消息级**指标读 `Conversation.msg_counts`（不依赖抽取，失败的群照样有）。
- 两个入口：
  - `daily` 跑完自动调用，scope = 本次运行的两天
  - 手动重算（`--taxonomy-version vN` 重打标 / `--attribution X` 换归属口径）——
    事实全存，落地那天不用重跑 LLM；今天生产恒走默认口径 `first_responder`
- 指标表**不受分片冻结约束** —— 它依赖的事实全都还在，随时可整体重算。

### ⑦ 落库 store —— MySQL 唯一写入方

- **所有写库 SQL 都在这一个文件里，一条都不许外流。** 不是「存储层抽象接口」—— MySQL 是当前唯一目标（见 `CLAUDE.md`「明确不做」），这里只保证写库代码集中在一处。
- 建表走手写的 `schema.sql`（**本仓库根目录，权威的那份**），人工执行一次；
  本模块不碰 DDL，但启动期 `check_schema` 对四张会写的表做列级自检 ——
  schema 漂移害过一次（抽取跑完 23 分钟才在 ⑦ 炸掉），现在第一秒暴露。
- 一个群的事实分片重写、抽取指标及旧客服指标清理使用一个事务（承重不变量 2）。保存与打标独立提交。
- 群日记录的 `agent_accounts` 保存后续客服指标需要的账号元信息；`classification_status` 区分待打标、成功和失败。
- `update_event_labels` 与人工 `retag_room` 复用按 ID 更新标签的 SQL；`finish_classification` 与人工重打标复用指标发布逻辑。
- 拉取或读取失败时，`daily` 仅尝试追加 `run_failure`，不修改事实与指标；读取阶段的上游版本错误仍整轮退出。
  记录写入自身失败时保留原始原因并报错，不对非幂等的失败记录追加做重试。

### 四个进程

| 进程 | 触发 | 干什么 | 失败语义 |
|---|---|---|---|
| `mirror` | `daily` 的第一步；补数时单独跑（子命令待定） | 查索引表 → HTTP `Range` → 本地镜像 | **群 × 日隔离**（与抽取失败同一条路径）。索引表连不上是整轮失败 |
| `daily` | 每日定时 | 抽取保存 → channel → 独立打标与指标发布 | **群 × 日分阶段隔离**，整轮继续 |
| `taxonomy` | **人工触发** | **只产词表，不写 `b_merchant_group_event` 表** | 失败无所谓，不阻塞任何人 |
| `recompute` | **人工触发**（词表升版后） | 按新词表重打标：只写标注列 + 重算 `agent_metric_daily` | **群隔离**，一个群一个事务，整轮继续 |

---

---

## MySQL 表结构

| 表 | 键 | 要点 |
|---|---|---|
| `b_merchant_group_event` | `idx_shard (corpid, roomid, occurred_on)` 分片删重写 | `source_msg_ids` 用 JSON 列，不拆关系表 · `event_type`（主类，单值）与 `event_types`（全集，JSON）是**两列**，见下 |
| `b_merchant_group_metric_daily` | `uk_group_daily (corpid, roomid, dt)` REPLACE 覆盖 | 加一列 `extraction_status` |
| `b_merchant_group_agent_metric_daily` | `uk_agent_daily (corpid, room, agent, dt, event_type, taxonomy_version)` **六列** | 加 `room` 使其嵌套进失败隔离粒度 |
| `b_merchant_group_taxonomy` | `uk_taxonomy (version, type_id)` | `name` 必填 · `description` **必填** · 词表由人定稿，不含向量列。从 ⑤ v1 起进 `check_schema` —— **查表不查行**，v0 期没有行是正常状态 |
| `b_merchant_group_run_failure` | 追加 | `(run_date, corpid, roomid, reason)` |

**`b_merchant_group_agent_metric_daily` 为什么是六列**

- `event_type` 进语义键 —— 否则一行只能存总量，存不了「每类各多少个」。
- `taxonomy_version` 进语义键 —— 词表会升版重打标，不记版本这张表就是一堆无法解释的数字。
- **`room` 进语义键** —— 键必须嵌套在「群 × 日」的失败隔离粒度里，否则某个群失败时会用残缺数据覆盖完整数据。跨群总量查询时 `SUM`。
- `event_type` 为空时用显式的 `__untyped__`，**不用 NULL**。
- **进键的是主类，不是全集。** 一个事件确实可能同时属于两件事（`classify::Labels`：
  `event_type` 是主类、`event_types` 是全集）。副类**不进这张表** —— 一个事件计进 N 行
  会让 `SUM(event_count) > 事件数`，客服主管拿它当处理量就是个虚高但看起来正常的数字。
  `event_types` 只给 webUI 下钻用，不建索引。
  想问「换人一共多少起（含副类）」，今天只能扫 `event` 表的 `event_types`，
  **指标表答不了这个问题** —— 那是这个取舍买单的地方。
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

类型由人命名并给出描述；`description` 必填，因为它与名称共同决定模型选择标签的含义。

**建表**：一个手写的 `schema.sql`，人工执行一次。字段类型 / 命名 / 必须字段遵循公司《数据库规范》，适用条款与四条已取下的例外见 `database-conventions.md`。不用 `CREATE TABLE IF NOT EXISTS`（会掩盖"表结构变了但没迁移"）。**不引入 ORM 和 migration 框架。** 跑批进程只读写数据，不碰 DDL。

**原始消息不入 MySQL**。溯源靠 `sourceMessageId` 回 raw 区查（`read_by_ids`，单群单月 18 MB ≈ 70 ms）。
⚠️ 措辞修正：**唯一事实来源是 OSS，本地 `./data/raw/` 是它的镜像/缓存** —— 删了能重拉。
保留期由 `raw_retention_months` 控制，清理起点锚在本轮窗口最早月；本轮需要的月份不会被删除。
原文超过保留期时，`read_by_ids` 显式报告缺失 ID。

**embedding 不入 MySQL、不引向量库**。⑤ v1 走的是「模型从封闭词表里选」，
**根本不发 embedding 请求**，而归纳那条用本地 embedding 的路 2026-09-03 也删了 ——
`sha256(summary) -> vector` 的内容寻址缓存今天不存在，词表表上的 `centroid` 列也
一并删掉（没有任何路径能产出它）。真要做时的形状仍是：只增不减，
10 万条 × 1024 维 f32 约 400MB，全量算余弦是秒级。

**入库的是打标结果缓存，不是向量**：`sha256(summary) -> type_id`，落文件不落 MySQL，
形状见上面 ⑤ 那节。

---

---

## webUI（只读旁路）

形态已定（2026-08-30）：**前后端分离 · 后端只读 JSON API · 全部 `GET` · 无登录版**（内网可达即可看）。
它与跑批解耦 —— 只从 MySQL 和 ① 的端口取数，**不写任何表、不调模型、不参与跑批**，跑批不知道它存在。
下钻原文走 `read_by_ids`，**不自己再翻译一遍上游字段名**。

`src/bin/webui.rs` 独立启动只读后端，HTTP 与查询实现集中在 `web/`（`serve` 路由 · `budget` 限额 · `query` 只读 SQL），复用 MySQL 和 `read_by_ids`。
`GET /api/meta` 提供可用日期、群与客服标识、当前词表；`GET /api/dataset?from=...&to=...` 在同一个明确的 REPEATABLE READ 只读事务中读取 meta、event 与群日记录。
默认请求最近七天，日期筛选进入后端查询并参与前端缓存键；全部历史仍可显式选择。群抽屉按需加载独立七天，窗口外事件由 `GET /api/event/{id}` 读取，原文由 `GET /api/event/{id}/messages` 读取。
已完成事件的词表版本与 meta 不一致时显式报错；未完成群的标签三列在读取端暂不发布，事实仍可查看。
群日响应包含 `classification_status`，前端独立显示待打标和打标失败，并排除未完成分类结果。
`freshness` 是只读派生状态：缺少 `fact_completed_time`，或抽取失败时间晚于 / 等于该凭据时为 unknown。
两种时间都采用微秒精度；打标不修改事实凭据。失败查询通过群与阶段时间索引定位，不聚合全企业历史失败。
只读入口解析自己的配置（`web/config.rs`，跑批的 `config.rs` 里没有任何 `Web*`）；重查询与原文扫描分别限制并发，结果按行数和字节限制，超限显式报错。
现表未记录失败窗口，不能推断抽取失败影响哪天；保守保留未知，不修改旧事实，也不把它映射成 failed 或 0。
前端 `groupDayStatus` 与 `coverage` 统一解释成功、失败、缺记录和最新结果未知；聚合仅消费已知成功群日的事件，旧事实仍可通过行 ID 核实。

- **完整性与数值一起展示。** 消息级和事件级具有不同失败语义；客服首响归属量不能代替团队事件总数。只读取数负责快照与新鲜度，指标模块负责统一的 `coverage`，各视图不再自行用“没有 failed”推断完整。
- **给模型的是脱敏正文（不变量 7），给人看的下钻是原文。** 两条路不同，是有意的：主管要核实首响，脱敏版核实不了（「客户 `<手机号>` 要求改期」看不出是哪一单），而主管本来就有权进那个群。
- ⚠️ **权限与认证本期明确不做。** 触发条件是有人提出「某某不该看到某某的数」。
- ⚠️ **未解**：领域里客服只有 `easyUserId`，样本里**没有任何字段带姓名** —— 界面上只能显示一串 ID。需要一张人工维护的映射表或一份人员花名册接口。**不解则客服维度页面没有意义**（群维度和事件明细不受影响）。

---

## 端口判据：一个适配器 = 假想接缝，两个 = 真接缝

③ 有第二个实现，值得写 trait。
**⑤ 上了 v1 之后仍然没写 trait**—— `Classifier` 是个 struct，
一次运行构造一次，拿住词表和结果缓存。「v1 落地时再引接缝」那句话真正要的是
「一次运行构造一次的对象」，struct 已经满足；而 v0 **不需要第二个实现** ——
「还没有词表」精确等于「taxonomy 表里 v0 没有行」，就是 `types.is_empty()` 那一行 if。**① 明确不写 `MessageSource` trait** —— 契约是**文字**的价值，写成 trait 壳只多一处「改签名要改两处」的负担，`ingest/mod.rs` 顶上那段模块文档注释（`//!`）就是端口本身。**④ 明确不给端口** —— 它是承重不变量 6（溯源）的守卫，给它接缝等于给溯源留绕过口。**② 有阶段名但不独立成模块**（分组必须下推给源）：
