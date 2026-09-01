# 数据库规范 —— 本仓库适用部分

上游是公司统一的《数据库规范》（根目录 `数据库规范.md`，覆盖全部应用）。
这份文档只抽出**本仓库真正受约束的条款**，外加**四条已经取下的例外及其理由**。

改 `schema.sql` 之前读这里；上游规范改版时回去比对一遍。

⚠️ `schema.sql` 权威的那份在**本仓库根目录**（⑦ 在这边，`cargo test` 还钉着它的
`summary` 列注释与 `SUMMARY_MAX` 一致）。`../pychat2events/schema.sql` 是搬运前的
原件、此刻逐字节相同 —— 按「移过来不是抄过来」的原计划该删，删除跨仓库，留给人工。
**改 DDL 只改本仓库这份。**

---

## 硬性约束（照做，没有商量）

| # | 约束 | 本仓库怎么落 |
|---|---|---|
| 1 | MySQL **8.0+** | InnoDB + `utf8mb4`，承重不变量 2 依赖事务 |
| 2 | 建表统一 **`CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci`** | 五张表全部显式写出，不靠库级默认 |
| 3 | 表名 / 字段名**全小写**，不以数字开头，两个下划线之间不只有数字 | ✓ |
| 4 | **表前缀 `b_`**（公共基础应用 `basic-public-app`） | `b_merchant_group_event` · `b_merchant_group_metric_daily` · `b_merchant_group_agent_metric_daily` · `b_merchant_group_taxonomy` · `b_merchant_group_run_failure` |
| 5 | 非负整数必须 **`UNSIGNED`** | 自增 id、全部 `*_count`、`first_reply_p*_sec` |
| 6 | 长度几乎相等的字符串用 **`CHAR`** 定长 | `easyUserId` → `CHAR(16)`（见下方例外 B） |
| 7 | `VARCHAR` 长度 ≤ 5000，超了改 `TEXT` 独立成表 | 最长的 `summary` 是 `VARCHAR(200)` |
| 8 | 小数用 `DECIMAL`，**禁 `FLOAT` / `DOUBLE`** | 本仓库没有小数列；`centroid` 是 `JSON` |
| 9 | 改字段含义要同步更新**字段注释** | 每列都有 `COMMENT`，不再只写 `--` 行注释 |
| 10 | 禁用保留字；**状态/类型字段不得裸用 `type` / `status`** | `type` → **`event_type`**；`extraction_status` 本来就带前缀 |
| 11 | 时间字段以**业务类型 + `_time`** 结尾 | `first_msg_at` → `first_msg_time`，`last_msg_at` → `last_msg_time`，`first_agent_reply_at` → `first_agent_reply_time` |
| 12 | 索引命名 **`uk_` / `idx_`** | `uk_group_daily` · `idx_shard` · `idx_agent` … |
| 13 | 必须字段 **`id` / `gmt_created_time` / `gmt_modified_time`** | 五张表全加（见下方例外 A：`is_deleted` 不加） |

### 落到代码里的 SQL 写法（受同一份规范管）

- **一律不写 `SELECT *`**，列名必须写明 —— `store.rs` 用 `EVENT_COLS` 这类常量。
- 统计行数用 `COUNT(*)`，不用 `COUNT(列名)` / `COUNT(常量)`。
- **禁存储过程。**
- `IN (...)` 的集合控制在 1000 以内 —— 本仓库最大的 `IN` 是 `occurred_on IN (窗口天数)`，个位数。
- **超过三个表禁止 join**；join 字段类型必须绝对一致。BI 侧最常见的是
  `b_merchant_group_agent_metric_daily` join `b_merchant_group_metric_daily` 查 `extraction_status`（承重不变量 5），双表。
- 数据订正（删除 / 修改）前先 `SELECT` 确认。

---

## 四条已经取下的例外

规范里这四条与本仓库的承重语义冲突，**是有意不遵守的，不是漏了**。谁要改回去，先读完理由。

### A. 不加 `is_deleted` / `deleted_time`

规范把逻辑删除列列为【必须】。本仓库**不加**。

理由：全项目**没有任何软删场景**。`b_merchant_group_event` 是按 `(corpid, roomid, occurred_on)`
**物理 `DELETE` 后重插**（承重不变量 3：任一窗口失败就整群跳过、一行不写），
两张 `b_merchant_group_*metric_daily` 是 `REPLACE` 覆盖写。加一个恒为 0 的 `is_deleted` 不是无害的占位——
**它会误导 BI**：查询的人看到这列就会写 `WHERE is_deleted = 0`，
从而以为存在「被软删的历史行」这种东西，而实际上重写过的数据是真的没了。

⚠️ 需要「被删掉的历史」时，正确的答案是 `./data/raw/` —— 那才是不可变的唯一事实来源。

### B. `corpid` / `roomid` 仍用 `VARCHAR`，不用 `CHAR`

规范第 6 条要求「长度几乎相等就用 char」。`easyUserId` 照做了（`CHAR(16)`），
`corpid` / `roomid` **没有**。

理由是**样本量**，不是偷懒。当前样本 4062 条实测长度分布：

| 字段 | 长度分布 | 不同取值 | 判断 |
|---|---|---|---|
| `easyUserId` | 全部 16 | **25 个** | 定长成立 → `CHAR(16)`，且承重不变量 8 已把它写成契约 |
| `corpId` | 全部 18 | **1 个** | n=1，证明不了定长 |
| `officialRoomId` | 全部 32 | **1 个** | n=1，证明不了定长 |
| `sourceMessageId` | 17 / 18 / 19 / 28 / 32 | — | 明确变长（存在 `JSON` 列里，不单独建列） |

`CHAR(n)` 存不下第 n+1 个字符时会截断或报错——**那是数据损坏**。
拿一个 corp、一个群的观察去赌全量数据的定长性不划算。等真实多 corp 数据到了再收紧。

### C. `VARCHAR` 索引不指定前缀长度

规范第 12 条要求「varchar 字段上建索引必须指定索引长度」。

本仓库进索引的 `VARCHAR` 只有 `corpid`(32) / `roomid`(64)，都是**标识列不是文本列**——
区分度全在末尾，截断前缀只会让 `idx_shard` 退化成必须回表校验，
而它承担的正是「分片删重写」的精确定位。文本区分度那条规则针对的是长文本字段，
本仓库没有这种字段进索引。

### D. 主键索引名不是 `pk_xxx`

规范要求主键索引名为 `pk_字段名`。**MySQL 的聚簇主键索引名恒为 `PRIMARY`，无法命名**——
`CREATE TABLE ... PRIMARY KEY` 不接受索引名。这条在 MySQL 上不可执行，不是选择。
唯一索引和普通索引的 `uk_` / `idx_` 前缀照做。

---

## `id` 在 `b_merchant_group_metric_daily` 上不是稳定行标识

`b_merchant_group_metric_daily` 走 `REPLACE INTO`（靠 `uk_group_daily` 触发冲突替换），
而 **`REPLACE` = `DELETE` + `INSERT`** —— 同一个 `(corpid, roomid, dt)` 每重算一次
就换一个新 `id`，`gmt_created_time` 也跟着重置。

这不影响任何东西（没人拿它做外键），但**别把这个 `id` 当作「这行第一次算出来是什么时候」**。
真要那个信息，看 `gmt_modified_time`，或者去 `b_merchant_group_run_failure` / 日志。

语义键仍然是 `uk_group_daily (corpid, roomid, dt)` —— 覆盖写的行为与加 `id` 之前**完全一致**。
