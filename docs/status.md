# 当前状态（Rust 版）

⚠️ 这份文档变得比其他任何一份都快。数字和待办以这里为准，`CLAUDE.md` 只放摘要。

⚠️ **这是 `chat2events-rs` 自己的进度，不是 Python 版的。**
Python 那边 ①→⑦ 已经端到端跑通（实测数字见 `../pychat2events/docs/status.md`）；
这边搬了 ①②。两份 status 不要互相复制。

## 搬运进度

| # | 阶段 | Python | Rust | 说明 |
|---|---|---|---|---|
| ① | 拉取 `pull` | ✅ | ✅ | 索引表 + HTTP `Range`，见 ADR-0005 |
| ① | 摄取 `ingest` | ✅ | ✅ | `list_rooms` / `read_room` / `read_by_ids` 三个出口全在 |
| ② | 会话 | ✅ | ✅ | 同 ①，不独立成模块 |
| ③ | 抽取 `extract` | ✅ | ⚠️ | 只有 `extract::smoke` 一段冒烟。缺：自适应二分 · 段间便签 · 序号↔`msg_id` 映射 · `_body` 脱敏 |
| ④ | 装配 `assemble` | ✅ | ❌ | |
| ⑤ | 分类 `classify` | ✅ | ❌ | |
| ⑥ | 指标 `metrics` | ✅ | ❌ | |
| ⑦ | 落库 `store` | ✅ | ❌ | 连接池已建（启动即握手，连不上当场炸），SQL 一条没写 |
| — | webUI | ❌ | ❌ | 两边都没做 |

**已有模块**：`lib.rs`（crate 根：mod 声明 + 全仓唯一 `Result`/`BoxError` 别名） ·
`main.rs`（入口） · `daily.rs`（编排） · `config.rs` · `window.rs`（跑批窗口类型） ·
`pull.rs` · `ingest.rs`（测试在 `ingest_tests.rs`，`#[path]` 分文件） · `llm.rs` ·
`extract.rs`（冒烟） · `testutil.rs`（测试 fixture，仅 `cfg(test)`）。

**验证**：`cargo test` **29 个用例 / 0.8s** —— `ingest` 20 + `pull` 5 + `llm` 1 + `window` 3。
单元测试跟着被测代码走；`ingest` 用 `#[path]` 分文件（逻辑上同一模块）。

## 已知的坑和已认领的代价

- **月份守卫有盲区，且是故意留的。** 8 月的消息被放进 9 月文件、而窗口整个落在
  8 月时，我们根本不会打开 9 月文件，那条消息静默漏掉、不报错。
  `ingest.rs` 里 `month_guard_known_blindspot` 那条测试**把这个行为断言下来了** —— 它记录代价，
  不是 bug。根治要让 `pull` 认识 `messageTime`，那会破坏「上游字段名只出现在 ingest 里」。
  实际敞口≈0（跨月窗口本来就读两个月文件）。
- **`daily.rs` 是串行 for 循环。** Python 版的 `ROOM_CONCURRENCY` semaphore +
  `asyncio.to_thread` 还没搬。DuckDB 是同步阻塞的，将来要走
  `tokio::task::spawn_blocking`，否则每读一个群会卡住所有在飞的模型调用。
- **`daily.rs` 把 `Conversation` 读完就丢**，只留 `.msgs.len()` —— 因为 ③ 还没接上，
  它才是这份数据的消费者。③ 一接上，那个 `.msgs.len()` 就变成 `extract(conv)`。
- **`extract::smoke` 走的是硬编码样本**，不是 ①② 读出来的会话。它是唯一一条真打端点
  的路径，作用是确认配置、鉴权、结构化输出、以及 `reasoning_effort` 有没有关掉
  （看输出里那个 `推理 0`）。③ 写完它就该死。
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
- `SEGMENT_MSGS` / `ROOM_CONCURRENCY` —— ③ 没搬，还没有这两个值。
  搬的时候照 ADR-0004：**都无默认值，缺失即报错**。
- `pull` 用哪个 TLS feature（`rustls` + `webpki-roots` 还是走系统证书库），
  见 ADR-0005 结尾。

## 2026-09-01 第二轮评审后的改动

一次评审（架构 + 字面量 + 测试布局），全部实施，29 个用例全绿、clippy 零警告：

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

## 下一步

③④ → ⑤⑥⑦。`pull` 已搬完，本机现在有自己的 raw 区（`./data/raw`，10 个群 / 100 条），
③ 一搬过来就有真数据可跑。
