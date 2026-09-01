# 当前状态（Rust 版）

⚠️ 这份文档变得比其他任何一份都快。数字和待办以这里为准，`CLAUDE.md` 只放摘要。

⚠️ **这是 `chat2events-rs` 自己的进度，不是 Python 版的。**
Python 那边 ①→⑦ 已经端到端跑通（实测数字见 `../pychat2events/docs/status.md`）；
这边 ①~⑦ 也全部搬完（2026-09-01），`daily::run` 真的跑完整条链。
两份 status 不要互相复制 —— 实测数字各测各的。

## 搬运进度

| # | 阶段 | Python | Rust | 说明 |
|---|---|---|---|---|
| ① | 拉取 `pull` | ✅ | ✅ | 索引表 + HTTP `Range`，见 ADR-0005 |
| ① | 摄取 `ingest` | ✅ | ✅ | `list_rooms` / `read_room` / `read_by_ids` 三个出口全在 |
| ② | 会话 | ✅ | ✅ | 同 ①，不独立成模块 |
| ③ | 抽取 `extract` | ✅ | ✅ | 自适应二分 · 段间便签 · 序号↔`msg_id` 映射 · `_body` 脱敏 · 校验重灌，全在 |
| ④ | 装配 `assemble` | ✅ | ✅ | 无端口（溯源守卫）。11 个字段从真实消息算，只有 `summary` 来自模型 |
| ⑤ | 分类 `classify` | ✅ | ✅ | **只有 v0 两个常量，没写 trait** —— 一个适配器 = 假想接缝，见 `classify.rs` 模块注释 |
| ⑥ | 指标 `metrics` | ✅ | ✅ | 纯函数。`recompute` 手动 CLI 没搬 |
| ⑦ | 落库 `store` | ✅ | ✅ | 三张表 + `run_failure`，一个群一个事务。`read_*` 四个读函数没搬（只服务手动重算） |
| — | webUI | ❌ | ❌ | 两边都没做 |

**已有模块**：`lib.rs`（crate 根：mod 声明 + 全仓唯一 `Result`/`BoxError` 别名） ·
`main.rs`（入口） · `daily.rs`（编排） · `config.rs` · `window.rs`（跑批窗口类型） ·
`pull.rs` · `ingest/`（①②） · `llm.rs` · `extract/`（③④） · `classify.rs`（⑤） ·
`metrics/`（⑥） · `store.rs`（⑦） · `testutil.rs`（测试 fixture，仅 `cfg(test)`）。

```text
src/
├── lib.rs · main.rs · config.rs · window.rs · classify.rs · llm.rs · pull.rs · store.rs
├── daily/    mod.rs  tests.rs                          跑批那一轮的编排
├── ingest/   mod.rs  tests.rs                          ①②
├── metrics/  mod.rs  tests.rs                          ⑥
├── extract/  mod.rs  tests.rs                          ③④，见下
│             redact.rs   正文脱敏与订单号正则（ADR-0001）
│             prompt.rs   SYSTEM —— 逐字搬运，改一个字实测结论全作废
│             render.rs   便签 · 匿名标签 · 行号箭头，view 是唯一出口
│             segment.rs  分段与切点选择（ADR-0004）
│             assemble.rs ④ merge / align / assemble / orphans
└── testutil.rs
```

⚠️ **`extract/` 拆成 6 个文件，接口一字未动** —— 对外仍然只有
`Event` / `SegmentModel` / `extract`（＋ 对拍用的 `preview`）。拆的是导航成本不是深度。
另有 `examples/dry.rs`（`--dry`，不花 token）和 `examples/smoke.rs`（唯一一条真打端点的路径）。

**验证**：`cargo test` **80 个用例 / 1.9s**，clippy 零告警 —— `extract` 40 + `ingest` 21 +
`metrics` 7 + `pull` 5 + `daily` 4 + `window` 3 + `store` 2 + `llm` 1。
测试拆分规则：**测试块 ≥ 100 行的模块拆成目录**（`<模块>/tests.rs`），其余留在文件底部。
测试始终是被测模块的**子模块**，私有项照常可见。

⚠️ **`store` 只有两条离线用例**（`stray_days` 冻结区守卫、占位符个数），
写库 SQL 本身要真 MySQL 才跑得到；`LiveModel` 要真端点。两者都没有离线测试，
分别靠 `cargo run` 和 `cargo run --example smoke` 验。

## 已知的坑和已认领的代价

- **月份守卫有盲区，且是故意留的。** 8 月的消息被放进 9 月文件、而窗口整个落在
  8 月时，我们根本不会打开 9 月文件，那条消息静默漏掉、不报错。
  `ingest.rs` 里 `month_guard_known_blindspot` 那条测试**把这个行为断言下来了** —— 它记录代价，
  不是 bug。根治要让 `pull` 认识 `messageTime`，那会破坏「上游字段名只出现在 ingest 里」。
  实际敞口≈0（跨月窗口本来就读两个月文件）。
- **`room_concurrency` 现在压的是模型调用，那个数已经作废。** `daily::run_rooms` 的
  `JoinSet` 背压没变，但每个任务从「读一个群」变成了「读 + 抽 + 落库」。**8 是按本机
  核数量读取量出来的**，现在约束是端点 TPM —— ADR-0004 给了公式
  `N ≈ TPM额度 / 14000`，**去控制台确认额度后重新量**。
- **`run_rooms` 收了一个「一个群干什么」的闭包参数。** 不是给 ⑦ 开接缝（明确不建存储层
  抽象），是为了让循环那三条性质离线可测：每个群恰好记一次 · 到点不算失败 ·
  `Upstream` 整轮死。生产传的是 `run_room`，测试传一个只读的。
- **Rust 的 `regex` 不支持后顾断言**，而 `_PHONE` 的两侧非数字断言是承重的
  （ADR-0001:36 那 363 处差额全在订单号内部）。断言手写在 `extract::phone_spans` 里，
  连带复刻了 Python 正则引擎的两处回溯行为，各有测试钉着。
- **`FIELD` 的零宽前瞻同理改成了捕获组**（吃进分隔符再 `${3}` 吐回去），停在同一个位置。
- **窗口里没有消息，但不是故障。** 索引表登记的 10 个群共 100 条消息，日期落在
  08-26 / 08-28 / 08-29；而今天（09-01）的窗口是 `[08-30, 08-31]` —— 拉取全成功、
  `read_room` 读出 0 条，两件事都对。要看到消息得让 `window` 覆盖 08-29 之前。

## 2026-09-01 架构评审后的五项改动

一次评审（`ingest` 字面量管理 + 项目结构），五项，**没有一项改变模块签名** ——
`list_rooms` / `read_room` / `read_by_ids` 三个出口一字未动，原有 20 个用例全绿。

| # | 改了什么 | 为什么 |
|---|---|---|
| A | 取列**按列名**不按下标；`text` 进必填守卫 | 12 列有 8 列是字符串，下标错位类型兼容、编译通过、守卫放行 —— 是**静默**的。`text` 走 `unwrap_or_default()` 时，「`text` 恒非空」这条契约在读取点被绕过 |
| B | 月份由 `files()` 带进 `scan()`，不从 `filename` 反解；`EXT` / `MONTH_FMT` 提 const | 布局曾被编码两个方向（`room_path` 拼、守卫拆），没有共享定义 |
| C | SQL 从 `const` + `.replace()` 链改成 `select_sql!` 宏 + `format!` | 占位符名字打错原样进 SQL、到 DuckDB 才报错；`.replace` 有先后顺序，先插进去的内容会被后面再扫一遍 |
| D | `main.rs` 瘦身：编排出 `daily.rs`，`RepairEvent` / `CHAT` 出 `extract.rs` | 硬编码样本住在最不可能被删掉的文件里；编排住在 `main` 里则测试调不到 |
| E | 把 `CLAUDE.md` / `CONTEXT.md` / `docs/` 搬进本仓库并改掉 Python 特有措辞 | 注释里的「见 ADR-0005」「承重不变量 6」在本仓库是断链 |

**A 新增一条测试**（`text_never_empty_both_blank_fails_room`），
覆盖 `content` 为 `null` 和为 `""` 两个分支 —— 后者 `COALESCE` 返回空串不是 NULL，只判 NULL 漏得掉。

**B 顺带证实了一件事**：DuckDB 的 `filename` 列**原样回传**我们传进去的路径字符串
（否则 `month_of` 查表会 miss，`month_guard_rejects_foreign_month` 那条测试会红）。
这条前提现在有测试兜着。

## 还没定死的

- `lookback_days` 配置值 **2**（已无代码默认值，必填）。⚠️ 判据仍未量：跨 2 天以上才闭合的占比，
  **要按周几分别看**，> 5% 立刻调 4。（与 Python 版同一个未决项，不要各测一遍。）
- `segment_msgs` 配置值 **400**（`[extract]` 段，已必填，无代码默认值）。
  跟 Python 版 `.env` 同一个值。**省钱旋钮不是质量旋钮**，调大换墙钟是空的
  （墙钟由输出吞吐定，全群输出总量不随分段变）。
- `room_concurrency` 配置值 **8** —— ③ 已经接上，**这个数现在是错的**，见上面「已知的坑」。
- `round_deadline_secs` 配置值 **21600**（6 小时，已必填）。**拍的，没量过** ——
  正常一轮分钟级，这个数只需要「比最坏的正常轮长、比 cron 周期短」。
  真实群数上线后看一轮实际耗时再收紧。
- `pull` 用哪个 TLS feature（`rustls` + `webpki-roots` 还是走系统证书库），
  见 ADR-0005 结尾。

## 2026-09-01 第二轮评审后的改动

一次评审（架构 + 字面量 + 测试布局），全部实施，clippy 零警告。
（当时 29 个用例 —— 那是**第二轮的时点数**，③~⑦ 还没搬；今天是 80，见文末。）

| # | 改了什么 | 为什么 |
|---|---|---|
| 1 | `daily.rs` 按 `IngestError` 变体分流：`Upstream` 整轮退出、其余该群跳过整轮继续 | 此前一个 `?` 把类型上精心区分的三种失败全部上抛 —— 任何群的坏行掀掉整轮，承重不变量 3 在调用方失守 |
| 2 | 列名 const（`ingest` 12 个 `COL_*`、`pull` 6 个）：SELECT 列表与取值点引用同一组常量 | 18 个列名曾各写两遍且必须逐字相同，编译期零校验；现在打错是编译错误 |
| 3 | 新增 `window.rs`：`Window` 类型（`new` / `span`），非空、连续、升序由构造保证；`config::window` 删除 | 窗口概念散落 4 文件 6 处、两处 `min().unwrap()`，其中 `read_by_ids` 对空窗口是可达 panic |
| 4 | `PullError { Round, Room }`：拉取的失败通道类型化，测试改 `matches!` | 此前整轮/群级之分靠「错误出现的位置」隐式表达；错误文案曾是测试唯一能抓的把手 |
| 5 | lib + bin 拆分（`lib.rs`）：全仓唯一 `Result`/`BoxError` 别名（此前四份逐字重复 + main 一处内联） | 别名需要公共落点；`daily::run` 的「可测」承诺从此结构上可兑现 |
| 6 | `ingest` 测试 `#[path]` 分文件到 `ingest_tests.rs`；fixture 合一进 `testutil.rs`（`fresh_root` / `write_month`） | ingest.rs 39% 是测试；`raw()`/`tmp()` 同一配方写两遍，`raw()` 不够用时测试体内又复制其后半段 |
| 7 | 配置全必填：删掉 7 个与 config.toml 逐字相同的 serde 默认值（死代码），`connect_timeout_secs` 补进 config.toml | CLAUDE.md 规矩是「缺字段直接崩，不给默认值」；默认值唯一的效果是掩盖 toml 丢字段 |
| 8 | 零碎：`MonthFile` / `SCHEMA_VERSION` / `PARSER_VERSION` 去 pub（无跨模块读者）；`messageTime / 1000` 补毫秒→秒注释；`SEEN_UNKNOWN` 锁 `unwrap` 改 `is_ok_and`（日志装饰不再持有 panic 路径）；`scan` 空文件列表早退（删掉两处 5 行双写）；跨文件复制的技术论证收敛为指向权威处的引用 | — |

签名有变：① 的三个出口从 `days: &[NaiveDate]` 改收 `&Window`（`architecture.md` 已同步）。
明确没做：`tests/` 集成测试（`daily::run` 需要真实 MySQL 与端点，无法离线跑）；
`llm::strict_schema` 的「T 必须是 struct」启动期自检（③ 落地时补，今天冒烟已覆盖唯一的 T）。

## 2026-09-01 搬完 ③④⑤⑥⑦

`daily::run` 现在真的跑完 ① → ③④ → ⑤⑥ → ⑦。

**与 Python 版对拍（`examples/dry.rs`）**：同一份 823 条样本、同一条 `view`，
prompt **59664 字节逐字节相同**，分段边界同为 `(0,277) (277,535) (535,823)`，
`[段外引用]` 三段计数同为 6/0/6 · 14/0/3 · 3/0/2。
这一条直接验证了 `_body`（含手写的手机号边界）· `_labels` · `render` · `_segments` ·
`_cut` 五件事的搬运等价，**一个 token 都没花**。复现：

```sh
cargo run --example dry -- <raw_root> <corp> <room> <since> <until> [segment_msgs]
```

**真端点冒烟（`examples/smoke.rs`）**：6 条手写消息，走生产的 `LiveModel`。实测
`prompt=1153 / completion=129 / reasoning=0`、2.7s、抽出 2 个事件 ——
其中 5 条消息（含平台的「稍等」）正确归成**一个**事件、首响锚在「稍等」那条。
这条同时确认了 dashscope **认** `SegmentExtraction` 那个带 `$defs`/`$ref` 的嵌套 schema
（`strict: true`），以及 `reasoning_effort=none` 真的生效。

**真数据端到端跑通（2026-09-01，`lookback_days` 临时改 4 → 窗口 `[08-28, 08-31]`）**：

| 数 | 值 |
|---|---|
| 群 / 消息 / 事件 | 10 / **83** / **23**（`ok=10 failed=0 empty=0`） |
| 墙钟 | **17.3s**（首轮）· 6.2s（重跑，连接已热） |
| 单次调用 | 1.0~3.6s，input 1.1~2.0k / output 52~385 token |
| `reasoning_tokens` | **全部 0**（哨兵一次没喊） |
| 落库 | `event` 23 行 / 10 群 / 2 天 · `metric_daily` **40 行**（10 群 × 4 天）全 `ok` · `agent_metric_daily` 12 行 / 7 人 · `run_failure` **0** |

**库里核过的承重不变量**：溯源为空 0 · 时间倒挂 0 · summary 超长 0 ·
事件级 NULL 0（全 `ok` 时就该是 0）· `merchant_event_count > event_count` 0 ·
`unreplied_count > merchant_event_count` 0。未回复 3 条（`first_agent_reply_time IS NULL`）。

**幂等性实测**：原样重跑一轮，`event` / `metric_daily` / `agent_metric_daily`
三张表行数**一个不多一个不少**（23 / 40 / 12）—— 分片删重写是对的，没有重复插入。

**校验重灌（`MAX_RETRIES=1`）在生产路径上真的触发了一次**：模型把三个订单号写进了
`summary`，报错原文回灌后自我修正通过。这条路径此前只有单元测试覆盖。

⚠️ **`lookback_days` 已改回 2。** 常规跑批窗口是 `[T-2, T-1]`，而本机 raw 区那 100 条
消息落在 08-26 / 08-28 / 08-29 —— 09-01 之后再跑会是 10 个群全部 `empty`、一行不写。
**那是对的，不是 bug。** 要再跑一轮验证得先把窗口挪回消息上。

## 2026-09-01 第三轮评审：结构 · 字面量 · 注释

一次全仓审核（架构 + 目录 + 硬编码 + 注释正确性），七项全部实施。
**没有一项改变模块的对外接口**，`cargo test` 78 → **80 个用例全绿**，clippy 零告警。

| # | 改了什么 | 为什么 |
|---|---|---|
| A | `sender_role` / `asker_role` 从 `String` 换成 `ingest::Role` 枚举；解析只在读取点发生一次，认不出的 `identityType` = 该群失败 | 这是全仓**唯一一个能静默损坏数据**的架构问题：`== "INTERNAL"` 在 4 个模块比较 8 次，打错一个字母会同时让 `labels` 标反、`agents` 恒空、首响 p50/p90 全 `NULL`，而编译器和测试都不会红（测试也用字面量造数据，错得一致就一起绿）。旧的 `unwrap_or_default()` 还会让上游新增身份类型静默变成空串 |
| B | `extract.rs`（1080 行）拆成 `extract/` 六个文件；`ingest` / `metrics` / `daily` 转目录模块 | 一个文件里挨着放 6 件互不相干的事（正则脱敏 / prompt / 渲染 / 分段 / 端口校验 / 装配），改便签规则要先滚过 200 行正则。**深度没变，对外仍是三样** |
| C | 测试分文件规则定死：**测试块 ≥ 100 行拆目录**，其余留文件底部；`#[path]` 全部去掉 | `#[path]` 是绕过，Rust 原生做法是目录模块。此前 `ls src/` 13 个文件里 3 个是测试，看不出来 |
| D | `store` 的 4 个表名 + `FAILURE_COLS` 提 const，DELETE / INSERT / `check_schema` 引用同一组 | 列名早就收进 `*_COLS` 了，表名没有：`b_merchant_group_agent_metric_daily` 这个 35 字符的名字写了 4 遍。`run_failure` 的列是唯一一组没常量、也没进占位符测试的 |
| E1 | 脱敏占位符提 `MASK_PHONE` / `MASK_FIELD` / `MASK_AT`，新增 `the_prompt_and_the_masks_agree` | 三处必须逐字一致（`body` 产出 · `PLACEHOLDER` 拦截 · `SYSTEM` 教模型认）。不一致 = 模型拿占位符当关联线索、validator 也拦不住它进 `summary`，而 **`sha256(summary)` 是 ⑤ 的缓存键，PII 进去就焊死**。prompt 那份保持字面量（逐字搬运不能动），一致性交给测试 |
| E2/E3 | `examples/dry.rs` 的 `segment_msgs` 默认值 400 删掉，改必填；`llm.rs` 的 `tcp_keepalive(60)` 提具名 const | 前者与「`segment_msgs` 无默认值，缺失即报错」的规矩**相反** —— 对拍工具悄悄用一个和配置无关的数跑，而分段边界正是它要验的东西。后者是全仓唯一没有名字的时间常量 |
| F | 五条**已证实说错**的注释 | `main.rs` 说「抽取结果走 stdout」而全仓没有一个 `println!`（结果走 MySQL）；`extract` 的测试指向不存在的 `examples/xcheck.rs`（是 `dry.rs`）；`ingest` 说「③⑦ 还没搬过来」（早搬完了，字段确实仍无读取点但理由失效）；`store::check_schema` 说查「五张表」实查四张（第五张 `taxonomy` 是 v1 的，不查是对的）；`status.md` 开头说「这边搬了 ①②」 |

**明确查过但不动的两处**（免得下次评审再提一遍）：

- **`Conversation.corp` / `.room` 是死字段** —— 实测零读取点（`daily::run_room` 一路带
  自己的参数）。但 `CONTEXT.md` 的领域契约里 `Conversation` 就是这个形状，且 webUI 下钻
  会用。**只改了那条说错的理由，字段保留。**
- **`store::write_room` 11 个参数 / `run_room` 8 个** —— 参数多是**刻意的成本**：
  拆开就意味着调用方可以只写一半，而这个函数存在的全部理由就是承重不变量 2
  （一个群的写入不可分割）。收进一个 struct 只是把 8 个字段换个地方写，
  复杂度没被集中，只是挪了个位置。

**没做的**：`labels()` 的 `"平台"` / `"商家"` 前缀、`render` 的 `"%H:%M:%S"`、
失败原因文案 —— 各只出现**一次**且都在自己唯一的归属地，提成 const 只是把定义
挪到 200 行以外，读者反而要多跳一次。

## 下一步

1. **重新量 `room_concurrency`** —— 现在压的是端点 TPM 不是本机核数（ADR-0004 的
   `N ≈ TPM额度 / 14000`）。⚠️ 上面 17.3s 那个数**量不出任何东西**：10 个群 83 条消息，
   离限流差几个数量级。
2. **`store` 的写库 SQL 仍然没有自动化测试** —— 上面是手工跑的一轮，不是 `cargo test`
   跑得到的东西。真要覆盖得起一个 MySQL 容器，今天没做。
3. webUI（只读）。
