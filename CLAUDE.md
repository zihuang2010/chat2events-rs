# Chat2Events

从企业微信会话存档的群聊日志中抽取**结构化业务事件**，产出群 / 客服维度指标，落库 MySQL。

不是聊天机器人，不是问答系统。**T+2 跑批，跳过当天和昨天，跑完即退出，没有常驻服务。**
**webUI 是唯一旁路，且只读** —— **事实与指标只从 MySQL 取数**（原文下钻读 `source_messages`
展示列，不碰文件系统、没有 `raw_root`），不写表、不调模型，跑批不知道它存在。
唯一的出站 HTTP 是 `web/roster.rs`：**展示别名**（客服姓名 / 商家名称）走 Nacos 找到的内部
服务，**取不到必须回落显示 ID** —— 别名不进指标、不进聚合键、不落库，理由在那个文件的顶注。

## 七个阶段

`OSS → mirror → ingest → extract/assemble → 保存事实 → channel → classify → 更新标签与分类指标 → webUI(只读)`

**六个阶段模块全在 `src/stage/` 下**，三个进程编排在 `src/process/`，只读旁路 `src/web/`，
内核（`boot` · `config` · `llm` · `window` · `worktime` · `rejection`）留 crate 根 ——
四类东西写在路径上，不靠注释区分。判据见 `src/lib.rs` 顶注。

| # | 阶段 | 模块 | 端口 | 出口类型 |
|---|---|---|---|---|
| ① | 摄取 | `stage/mirror/` ＋ `stage/ingest/` | 无（契约是 `ingest/mod.rs` 的 `//!`） | `Message` |
| ② | 会话 | 同 ①，**不独立成模块**（分组必须下推给源） | `read_room()` | `Conversation` |
| ③ | 抽取 | `stage/extract/` | `SegmentModel` —— 2 个适配器 = **真**接缝 | `EventDraft` |
| ④ | 装配 | `stage/extract/assemble.rs` | **无，且不该有**（溯源守卫） | `Event`（只有事实列） |
| ⑤ | 分类 | `stage/classify/` | 无 —— struct 不是 trait | `Label`（一个事件一个类） |
| ⑥ | 指标 | `stage/metrics.rs` | 无（纯函数） | 指标行 |
| ⑦ | 落库 | `stage/store/` | 无（已排除，MySQL 是唯一目标） | — |

**端口判据：一个适配器 = 假想接缝，两个 = 真接缝。** 逐个论证 → `docs/architecture.md`。

`process/daily/` 是把七阶段串起来的**编排**，不是阶段；`main.rs` 只做两件事：
`boot::Boot` 起进程、调 `process::daily::run`。
另有两个**人工触发**进程，都不写 `b_merchant_group_event`、不参与跑批：
`process/taxonomy/`（**词表由人手写** → `review` 试打 → `emit-sql` → 人工执行；机器归纳两条路都已放弃）·
`process/recompute.rs`（升版重打标，只写标注列）。
入口在 `src/bin/`（写生产库的运维入口，随 release 发布）；`examples/` 只剩
`dry` / `smoke` / `tzcheck` 三个不写库的诊断工具。**编排住在 lib 里**，
入口只负责 `boot::Boot` 起进程再调它 —— `Boot::llms()` 建两队模型时一并打启动日志。

## 文档地图

一条事实只写在一处，互相引用不复制。核心文档如下，其余设计理由在代码注释里。

| 文档 | 装什么 |
|---|---|
| **`CLAUDE.md`**（本文件） | 全局地图 · 不变量摘要 · 硬规则摘要 · 明确不做。每次自动注入 |
| `docs/invariants.md` | **八条承重不变量全文 —— 动它们之前必读** |
| `CONTEXT.md` | 术语 · 业务场景 · 上游数据形状 · 领域类型契约 · 词表生命周期 |
| `docs/architecture.md` | 七模块各自内部 · 端口上什么不许出门 · MySQL 表键的理由 · webUI |
| `docs/database-conventions.md` | 公司《数据库规范》的适用条款与四条已取下的例外 |
| `docs/deploy.md` | 构建 · 部署 · 目标机约束 |
| `docs/deploy-webui.md` | 只读工作台在 39.98.175.5:30001 的 runbook（nginx · systemd · 验证） |

⚠️ **决策记录（`docs/adr/`）和进度流水账（`docs/status.md`）已删。**
那些取舍的**结论和实测数字都内联在对应的代码注释里** —— 「为什么是这个值」
去读那个常量 / 函数头上的文档注释，别再找 ADR。

## 承重不变量（摘要，全文 → `docs/invariants.md`）

**这八条错一条就是静默的数据损坏。** 摘要够你认出「我正在碰它」，不够你改它 —— 要改先读全文。

1. **冻结**：默认两天窗口下，`occurred_on < T-3` 事实列不可写，标注列只在词表升版时整体重来。否则删重写制造**抖动而非修正**，报表做不了同比环比。
2. **两个分片同一个事务**：默认 `T-3` 与 `T-2` 一个事务。event 会在分片间移动（`occurred_on` 由模型判断的首条消息决定），分两个事务会让它一个分片都不在、或两个都在。
3. **失败分阶段隔离**：抽取失败保留旧事实；打标失败保留新事实与已完成批次标签，分类指标不发布。两种都记 `run_failure`，整轮继续。
4. **`Ok([])` 与 `Failed` 绝不混淆**：`ok` → 事件级 **0**；`failed` → 事件级 **NULL**。**绝不用 0 表示「没算出来」。**
5. **客服分类指标整群发布**：本群打标全部成功后才写新指标；事实重写时清除旧分类指标。聚合同时检查抽取、打标状态，未完成不是 0。
6. **溯源**：`source_msg_ids` 非空且每个 ID 真实存在。**模型根本不接触 `msg_id`** —— prompt 里是段内 1-based 序号，代码映射回去，越界即校验失败。
7. **正文脱敏**：给模型的正文必须过 `_body`。三件事同时：PII 出境 · 正文冒充行框架（**不变量 6 的绕过路径**）· 顺序依赖。**只掩锚点确定的东西**，姓名和自由文本地址一概不碰。
8. **标识体系**：`agent` = `easyUserId`（16 位定长），`room` = `officialRoomId`（= 文件名）。人用 easy、群用 official 是**有意为之**（各取最稳的），别「顺手统一」。

## 硬规则

- **按群控制内存** —— 读取下推 DuckDB；抽取由 `ingest.room_concurrency` 限制群数。有界 channel 只传企业、群和事件数；打标按群查库，群数和全局批次名额由 `classify.concurrency` 限制。
- **抽取段串行、打标批次并行** —— 抽取后一段读取前一段便签；模型吃不下才对半切。打标批次之间无便签依赖，最多 50 条摘要一批，跨群共享并发名额。
- **保存与打标独立提交** —— 先保存事件，再发送群任务；抽取收尾后排空打标队列。标签未完成是 NULL，不能写成 `__untyped__`。内存 channel 不承诺跨重启恢复。
- **人工补标恢复** —— `src/bin/recover.rs` 从群日状态补齐未完成分类（含零事件），保留已有标签与冻结事实；自动调度仍不跨重启恢复。首次缺失标签的补齐与词表升版重打是两种操作。
- **人工重跑失败群** —— `src/bin/retry.rs` 按 `run_failure` 挑活：抽取失败按**每条失败行自带的窗口分组**重跑（那是删重写范围，放宽一天就多抽一天），打标失败取**并集**交给 `recover`（那是筛选范围，给宽无害）。**「已经修好的」靠查询排除不靠删行** —— `run_failure` 只增不改，判据与 webUI 的 `KNOWN_OK_DAYS` 同源，改一处必看另一处。`window_since IS NULL` 的历史行跳过并告警。
- **事实新鲜度只认事实凭据** —— `fact_completed_time` 仅由成功保存事实推进；标签更新不能恢复旧事实新鲜度。升级后的未知历史凭据保持 NULL。
- **代码风格交给 rustfmt** —— 没有 `rustfmt.toml`，**不加是有意的**。提交前 `cargo fmt --check` 必须干净。
- **抽取实现必须可替换** —— 模型名 / API key / prompt 都是 ③ 的内部细节，换模型只换一个 `SegmentModel` 适配器。
- **模型输出必须先校验再落库** —— **校验分三档，判据是「缩小问题能不能解决它」**，
  不是「错得多严重」。全都先重问一次（同段重问便宜，切小再跑贵，顺序不能反）：
  - **规模相关**（序号越界 / ref / `msg_indexes` 空 / summary 超 `VARCHAR(200)` 列宽）→
    仍不过就**切小再试**，走 `extract` 那套自适应二分 —— 模型数不清行号、
    或把一长段揉成一条 589 字的 summary，多半都是因为这一段太长。
  - **规模无关 · PII**（summary 含手机号 / 抹完什么都不剩）→ 该批次失败。切了也一样犯。
  - **规模无关 · 可读性**（脱敏占位符 / **订单号** / summary 超 100 字但不超列宽）→
    **根本不算失败**：占位符与订单号就地抹除，超长放行（100 字是契约，硬闸是列宽，
    过了列宽归上一档）。

  **不落库半个事件**；字段级兜底修补只此一处，范围钉死在 `redact::NOISE` 上。
  ⚠️ 三档曾经是一档：可读性那档把 11 个群按 PII 处罚（9 个只因 ×1 条 summary 带了个脱敏
  记号），规模相关那档则**永不触发二分** —— 400 行的段里数错一个行号就一把打掉整群。
  ⚠️ **订单号 2026-09-16 从 PII 档挪进可读性档**：它是业务标识不是个人信息
  （`redact::body` 一直这么写，正文里一个字符都不掩），而全或无的成功率是 `(1-p)^n` ——
  实测某派单群一段抽出 149 个事件，重问后仍有 5 条 summary 抄了单号（改对 96.6%），
  十天窗口照样 0 条落库，三次重跑三次归零。**手机号留在 PII 档，那是真 PII。**
- **让程序错误显式暴露** —— 配置缺字段直接 panic（错误要在进程起来第一秒暴露）。⚠️ **判据是失败隔离粒度，不是「会不会被编译掉」**：群 / 日级失败一律走 `Result`（panic 会掀翻整轮），`panic!` / `unwrap` / `expect` 只留给启动期资源和构造已保证的不变量。**承重不变量绝不用 `debug_assert!`** —— 那个才会在 release 里蒸发。

## 明确不做

- **跑批**不引入 Kafka / Celery / Spark / 向量库 / 任何常驻服务。**webUI 是这张清单上唯一的例外，且只读。**
- 不做在线实时处理、不做增量订阅 —— **T+2 跑批是明确前提**。
- 不给 ① 加基类 / 工厂 / 注册表 / `SOURCE_TYPE` 配置项。换数据源靠新写一个适配器文件。
- 不做多租户 / 权限 / 审计（`corpid` 只是 event 的一个属性）· 不建存储层抽象接口 · 不做词表管理界面 / ORM / migration 框架。
- 不做「解决时长」—— 需要先定义「什么算解决」。
- **不把 ①~⑦ 这条线做成 agent**。

## 当前状态

日常跑批、词表试打与重打标已经接通。失败群的重跑由 `src/bin/retry.rs` 自动挑活（挑群与定窗口查库算，重跑本身仍走 `daily::run_span` / `daily::recover`）。只读工作台由 `src/bin/webui.rs` 独立启动，取数与原文契约在 `web/`，不参与跑批。

验证命令与适用范围见 `docs/deploy.md` 的「上线前的检查」：默认测试覆盖离线逻辑与本地 HTTP 模型协议；`mysql_` 测试在隔离 MySQL 上验证事务和只读取数，CI 显式执行。
真实 OSS 测试仍需手动启用；离线协议通过不能代替真实模型业务质量验收。

⚠️ **指标口径有三份实现**，全部由 `webui/src/domain/parity-vectors.json` 的**金标向量**钉住：

| 谁 | 算什么 | 谁在钉它 |
|---|---|---|
| 后端只读 SQL | 页面上的每个数字 | `mysql_summary_matches_the_frontend_definitions`（真 SQL） |
| 前端 `domain/metrics` | 指标与视图测试的对照物 | `test/mock/parity.test.ts` 跑 `mockSummary` |
| **跑批 `stage/metrics`** | `metric_daily.first_reply_p*_sec`，**BI 报表直连读它** | `quantile::tests` 离线跑 · `mysql_quantile_sql_exit_*` 跑真 SQL |

**改口径必须同时改三边并更新金标** —— 分家是静默的，页面照样显示一个看起来合理的数字。
⚠️ 第三份此前**不在任何对拍里**（`expected` 从 events 算，而 `groupDaily.first_reply_p50_sec`
是**输入**）。2026-09-19 补了一组 `quantileCases`（原始秒数 → 期望 p50/p90），并把分位数
定义收进 `src/quantile.rs` 的两个出口（照 `worktime` 那个形状）。
⚠️ **样本会被就地替换**，文档里带条数的实测数字必须注明是哪一版样本量的。


## 检索代码：先走 codebase-memory-mcp

本仓库已建索引（2329 节点 / 11001 边）。**结构性问题一律先查图** —— 一次几百 token，同样的问题 grep 全仓是几万。

`search_graph`（找符号：自然语言 / `name_pattern` / `semantic_query`）· `trace_path`（谁调用了 X / X 调用了谁）·
`get_code_snippet`（读源码）· `get_architecture`（整体结构）· `detect_changes`（改动影响面）。
**字面量 / 配置 / 非代码**还是 `search_code` 或 Grep —— 图不装这些。

- **图里没有 ≠ 代码里没有。** 下「没有任何地方调用它」这种结论之前先 `check_index_coverage(scopes=["."])`。
  ⚠️ `schema.sql` 是 `parse_partial`（DDL 里的中文注释噎住了解析器），**建表相关的事直接读文件**，别信图。
  `webui/src/test/mock/aggregate.ts` 此前也是，原因是文件里嵌了 4 个**字面 NUL 字符**（复合键分隔符写成了真 NUL 而不是 `\0` 转义）——
  那还让 `grep -r` **静默跳过整个文件**。2026-09-19 已改成转义，`file` 判定回到 UTF-8 文本、grep 搜得到；**索引标记待下次重新索引后确认**。
- 搬模块 / 改文件名之后跑一次 `index_repository(mode="full")`；日常小改由 watch 自动刷新。
