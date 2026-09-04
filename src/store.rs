//! ⑦ 落库 store —— **MySQL 唯一写入方。所有写库 SQL 都在这个文件里，一条都不许外流。**
//!
//! 不是「存储层抽象接口」—— MySQL 是当前唯一目标，这里只保证写库代码集中在一处。
//! 建表走手写的 `schema.sql`，人工执行一次；本模块**不碰 DDL**。
//!
//! **一个群一次运行的全部写入 = 一个事务**（承重不变量 2）。理由不是洁癖：
//! `occurred_on = date(first_msg_time)`，而「首条来源消息是哪条」由模型判断 ——
//! 同一个 event 会在 `T-2` / `T-1` 两个分片之间移动。分两个事务提交，中间失败就会造成
//! 它**一个分片都不在**，或者**两个分片都在**。
//!
//! 失败隔离粒度 = 群 × 本次窗口，三种失败语义不同（承重不变量 3 / 4 / 5）：
//!
//! | 失败在哪 | `event` / `agent` 表 | `group` 表 | `run_failure` |
//! |---|---|---|---|
//! | **拉取** | 不写 | **不写**（连消息都不全，写 0 就是拿 0 冒充「没算出来」） | 写 |
//! | **抽取** | 不写 | 写，`extraction_status='failed'`，事件级 NULL | 写 |
//! | 无（`Ok([])`）| 按分片删重写 | 写，`ok`，事件级 **0** | — |

use crate::{
    BoxError,
    classify::{Labels, TaxonomyType},
    extract::Event,
    ingest::Role,
    metrics::{AgentRow, GroupRow},
    window::Window,
};
use chrono::{NaiveDate, NaiveDateTime};
use sqlx::{MySqlPool, Row};

/// 一次 `INSERT` 最多带几行。MySQL 的预处理占位符上限是 65535，`event` 表 15 列 ——
/// 500 行 = 7500 个，留着一个数量级的余量。跑批一个群一天几百个事件，正常撞不到。
const BATCH: usize = 500;

/// 表名 —— DELETE / INSERT / [`check_schema`] 三处引用**同一个常量**。
/// `b_merchant_group_agent_metric_daily` 这个 35 字符的名字曾经写了 4 遍，
/// 打错一个字母是运行期的 `Table doesn't exist`，跟 `COL_*` 同一个理由。
///
/// ⑤ 上了 v1 之后 `b_merchant_group_taxonomy` 有了真实读取点（[`read_taxonomy`]），
/// 所以第五张表也进了这份清单和 [`check_schema`]。**查表不查行** —— v0 期
/// 这张表一行都没有是正常状态，不是启动失败。
const T_EVENT: &str = "b_merchant_group_event";
const T_GROUP: &str = "b_merchant_group_metric_daily";
const T_AGENT: &str = "b_merchant_group_agent_metric_daily";
const T_FAILURE: &str = "b_merchant_group_run_failure";
const T_TAXONOMY: &str = "b_merchant_group_taxonomy";

/// ⚠️ `event_type` 与 `event_types` 是**两列不是一列**：前者是主类（单值，进指标
/// 语义键），后者是全集（JSON，只给 webUI 下钻）。副类不进任何指标 —— 一个事件
/// 计进 N 行会让 `SUM(event_count) > 事件数`，见 [`crate::classify::Labels`]。
const EVENT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary, \
    event_type, event_types, taxonomy_version";
const GROUP_COLS: &str = "corpid, roomid, dt, msg_count, sender_count, event_count, \
    merchant_event_count, unreplied_count, first_reply_p50_sec, first_reply_p90_sec, \
    extraction_status";
const AGENT_COLS: &str = "corpid, room, agent, dt, event_type, taxonomy_version, event_count";
/// `run_failure` 的列。四组列名里它曾经是唯一没有常量的一组 —— INSERT 语句里写一遍、
/// [`check_schema`] 的清单里再写一遍，也没进那条占位符个数的测试。
const FAILURE_COLS: &str = "run_date, corpid, roomid, reason";
/// [`EVENT_COLS`] 的前 12 个 —— **事实列**。末尾三个 `event_type` / `event_types` /
/// `taxonomy_version` 是标注列，不在这里：[`read_events`] 还原的是 `Event`，而 `Event`
/// 只装事实列（标签不刻在它上面，是每次算出来的）。下面那条测试钉住「它必须是
/// EVENT_COLS 的前缀」。
const EVENT_FACT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary";
/// `IN (...)` 的集合上限（`docs/database-conventions.md`：控制在 1000 以内）。
const IN_MAX: usize = 1000;

/// 只有 classify 真正读的四列。**`centroid` 不在这里** —— 今天两条归纳路径产出的
/// 都只有名字和描述，那一列恒 NULL（`schema.sql` 留着「有就多一条路径」）。
/// `parent_name` 在：词表是两级的，一级要进分类 prompt 的分组标题。
const TAXONOMY_COLS: &str = "type_id, parent_name, name, description";

/// `(?, ?, …)`，个数**从列名串自己数出来** —— 手写一个数字，加列时忘了改就是一次
/// 运行期的 `Column count doesn't match`。
fn values(cols: &str) -> String {
    format!("({})", holes(cols.split(',').count()))
}

/// `?, ?, …`，用于 `IN (…)`。
fn holes(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// 落在窗口外的 `occurred_on` —— **承重不变量 1（冻结区事实列不可写）的守卫**。
///
/// 读窗口保证 `first_msg_time ∈ days` ⇒ `occurred_on ∈ days`，这里守住那个构造前提。
/// 拆成纯函数是为了让它**离线可测** —— 本模块其余部分要真 MySQL 才跑得动。
fn stray_days(events: &[Event], days: &Window) -> Vec<NaiveDate> {
    // 区间比较而不是 `days().contains()`：窗口连续（`window.rs` 构造保证），
    // 两者判一样的事，但线性查找在 `backfill` 的长窗口上是 O(事件数 × 天数)。
    let (since, until) = (days.since(), days.until());
    let mut out: Vec<NaiveDate> = events
        .iter()
        .map(|e| e.occurred_on)
        .filter(|d| *d < since || *d > until)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// 一个群一次运行的**全部**写入，**一个事务**。
///
/// `events = None` 表示这个群没算出来：`event` 表**一行都不删不写**
/// （少一个窗口就重写等于用残缺数据覆盖完整数据）；`agent` 表同理不动（承重不变量 5）；
/// `group` 表写不写由调用方给的 `group` 切片决定 —— **抽取失败**时 `daily` 传满窗口的行
/// （消息级指标不依赖抽取，`extraction_status='failed'` 要把「残缺」带出去），
/// **拉取失败**时传空（连消息都不全，见模块头那张表）；两种都记一行 `run_failure`。
///
/// `events = Some(_)`（**含空列表** = 这个群这几天确实没有业务事件，正常）：
/// 按 `(corpid, roomid, occurred_on)` 分片删重写。
///
/// `types[i]` 是 `events[i]` 的标签 —— 由 `daily` 在**事务外**算好传进来（见 ⑤ 的注释：
/// 在这里调 classify 就是持锁发 N 次 embedding 请求，还会让 store 反向依赖 classify）。
#[allow(clippy::too_many_arguments)]
// ⚠️ 参数多是有原因的，别为了好看拆成几个函数：拆开就意味着调用方**可以只写一半** ——
//    而这个函数存在的全部理由就是「一个群的写入不可分割」（承重不变量 2）。
pub async fn write_room(
    pool: &MySqlPool,
    run_date: NaiveDate,
    corp: &str,
    room: &str,
    days: &Window,
    events: Option<&[Event]>,
    types: &[Labels],
    taxonomy_version: &str,
    reason: Option<&str>,
    group: &[GroupRow],
    agent: &[AgentRow],
) -> Result<(), BoxError> {
    if let Some(evs) = events {
        assert_eq!(
            evs.len(),
            types.len(),
            "types 必须与 events 一一对应（构造保证）"
        );
        // 承重不变量 1：写之前挡住，不是写完再查
        let stray = stray_days(evs, days);
        if !stray.is_empty() {
            return Err(format!(
                "{room}: 事件落在窗口外 {stray:?}，会写穿冻结区（窗口 {} ~ {}）",
                days.since(),
                days.until()
            )
            .into());
        }
    }

    let mut tx = pool.begin().await?;

    match events {
        None => {
            let sql = format!(
                "INSERT INTO {T_FAILURE} ({FAILURE_COLS}) VALUES {}",
                values(FAILURE_COLS)
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(run_date)
                .bind(corp)
                .bind(room)
                .bind(reason.unwrap_or("未记录原因"))
                .execute(&mut *tx)
                .await?;
        }
        Some(evs) => {
            // **`BETWEEN` 而不是逐日 `IN`。** `Window` 的「非空、连续、升序」由构造
            // 保证（`window.rs`），所以两者选中的行**一字不差**；而 `IN` 的占位符
            // 个数 = 窗口天数，日常 2 天无所谓，`examples/backfill.rs` 给一个上千天的
            // 窗口就同时破掉 `IN_MAX = 1000` 那条规约。`BETWEEN` 恒定两个占位符，
            // 走的还是同一个 `idx_shard`，顺带跟 `retag_room` 的写法统一了。
            let sql = format!(
                "DELETE FROM {T_EVENT} WHERE corpid = ? AND roomid = ? \
                 AND occurred_on BETWEEN ? AND ?"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(corp)
                .bind(room)
                .bind(days.since())
                .bind(days.until())
                .execute(&mut *tx)
                .await?;

            for chunk in evs.iter().zip(types).collect::<Vec<_>>().chunks(BATCH) {
                let sql = format!(
                    "INSERT INTO {T_EVENT} ({EVENT_COLS}) VALUES {}",
                    vec![values(EVENT_COLS); chunk.len()].join(", ")
                );
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                for (e, t) in chunk {
                    q = q
                        .bind(&e.corpid)
                        .bind(&e.roomid)
                        .bind(serde_json::to_string(&e.source_msg_ids)?)
                        .bind(e.first_msg_time)
                        .bind(e.last_msg_time)
                        .bind(e.first_agent_reply_time)
                        .bind(e.occurred_on)
                        .bind(&e.asker)
                        .bind(e.asker_role.as_str())
                        .bind(serde_json::to_string(&e.agents)?)
                        .bind(e.first_responder.as_deref())
                        .bind(&e.summary)
                        // **标注列。** 标签不刻在 event 上，是每次落库现算的（包括分片
                        // 删重写这一次）—— 所以分片重写不会丢标签。
                        // 主类进 `event_type`（指标只看它），全集进 `event_types`。
                        .bind(t.primary())
                        .bind(serde_json::to_string(t.all())?)
                        .bind(taxonomy_version);
                }
                q.execute(&mut *tx).await?;
            }
        }
    }

    // metric_agent_daily 按 (corpid, room, dt) 删重写 —— **只碰抽取成功的那些天**。
    // 承重不变量 5：失败的群上这张表是整行缺失 / 保持原样，不是 0，与 event 表一致。
    // 键含 room，所以键嵌套在「群 × 日」的失败隔离粒度里，残缺覆盖在结构上不可能发生。
    if events.is_some() {
        // `BETWEEN` 同上：窗口连续，占位符恒定两个。
        let sql =
            format!("DELETE FROM {T_AGENT} WHERE corpid = ? AND room = ? AND dt BETWEEN ? AND ?");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(corp)
            .bind(room)
            .bind(days.since())
            .bind(days.until())
            .execute(&mut *tx)
            .await?;

        for chunk in agent.chunks(BATCH) {
            let sql = format!(
                "INSERT INTO {T_AGENT} ({AGENT_COLS}) VALUES {}",
                vec![values(AGENT_COLS); chunk.len()].join(", ")
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for r in chunk {
                q = q
                    .bind(&r.corp)
                    .bind(&r.room)
                    .bind(&r.agent)
                    .bind(r.dt)
                    .bind(&r.event_type)
                    .bind(&r.taxonomy_version)
                    .bind(r.event_count);
            }
            q.execute(&mut *tx).await?;
        }
    }

    // group 表用 REPLACE：语义键是 uk_group_daily，靠它触发冲突。
    // ⚠️ REPLACE = DELETE + INSERT，所以这张表上的 id 每重算一次就换一个新值 ——
    //    id 不是稳定行标识，语义键才是。
    for chunk in group.chunks(BATCH) {
        let sql = format!(
            "REPLACE INTO {T_GROUP} ({GROUP_COLS}) VALUES {}",
            vec![values(GROUP_COLS); chunk.len()].join(", ")
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = q
                .bind(&r.corp)
                .bind(&r.room)
                .bind(r.dt)
                .bind(r.msg_count)
                .bind(r.sender_count)
                .bind(r.event_count)
                .bind(r.merchant_event_count)
                .bind(r.unreplied_count)
                .bind(r.first_reply_p50_sec)
                .bind(r.first_reply_p90_sec)
                .bind(r.status.as_str());
        }
        q.execute(&mut *tx).await?;
    }

    // 提交之前任何一步 `?` 早退，`tx` 被 drop 时 sqlx 自动回滚 —— 不需要手写 rollback。
    tx.commit().await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 读 —— 三个读者：每轮开跑取一次词表 · `taxonomy` 归纳 · `recompute` 重打标。
// 跑批的**写入**路径一行都不读库。
// ─────────────────────────────────────────────────────────────────────────────

/// 取某一版词表。**没有行 = v0**（还没有词表），不是错误 —— `Classifier` 拿到空列表
/// 就走「全 `__untyped__`、不发请求」那条路。
///
/// `ORDER BY type_id` 不是装饰：词表会被渲染成 system prompt，顺序变了 prompt 就变了，
/// 而 ⑤ 的确定性要求同样的词表给出同样的答案。库里的行序没有保证，这里钉死它。
pub async fn read_taxonomy(pool: &MySqlPool, version: &str) -> Result<Vec<TaxonomyType>, BoxError> {
    let sql =
        format!("SELECT {TAXONOMY_COLS} FROM {T_TAXONOMY} WHERE version = ? ORDER BY type_id");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(version)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| TaxonomyType {
            type_id: r.get(0),
            parent_name: r.get(1),
            name: r.get(2),
            description: r.get(3),
        })
        .collect())
}

/// 归纳的输入：一段时间内**去重后**的 summary 及其出现次数，高频在前。
///
/// **`GROUP BY` 下推给 MySQL**（硬规则：过滤/投影/分组尽量下推）—— 归纳只关心
/// 「有哪些不同的说法」，把几万条原样拉进内存再在进程里去重等于白读一遍。
/// 次数留给 `review.md` 排序：先让人看高频的那几类对不对。
pub async fn read_summary_counts(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<Vec<(String, i64)>, BoxError> {
    let sql = format!(
        "SELECT summary, COUNT(*) AS n FROM {T_EVENT} WHERE occurred_on BETWEEN ? AND ? \
         GROUP BY summary ORDER BY n DESC, summary"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(since)
        .bind(until)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// `recompute` 要扫哪些群。**按群分批读是硬规则**（不把大数据集读进内存）：
/// 重打标一个季度、上千个群，一次 `read_events` 全量会把几百万个 `Event` 拉进内存。
pub async fn read_event_rooms(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<Vec<(String, String)>, BoxError> {
    let sql = format!(
        "SELECT DISTINCT corpid, roomid FROM {T_EVENT} WHERE occurred_on BETWEEN ? AND ? \
         ORDER BY corpid, roomid"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(since)
        .bind(until)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// 一个群一段时间内的全部 event，还原成领域类型。
///
/// 两个 JSON 列走 `CAST(... AS CHAR)`：sqlx 没开 `json` feature（写入侧是
/// `bind(to_string(..))`，本来就不需要），读回来当字符串再 `from_str` 是同一条路的反向。
///
/// `asker_role` 复用 [`Role::parse`]，不在这里再写一遍 `== "INTERNAL"` ——
/// 那条契约只该成立一次。库里存的正是 `Role::as_str` 写下去的字面量，
/// 认不出说明有人手改过库，该显式报错。
pub async fn read_events(
    pool: &MySqlPool,
    corp: &str,
    room: &str,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<(Vec<u64>, Vec<Event>), BoxError> {
    // 列表从 [`EVENT_FACT_COLS`] 派生，不手抄一遍 —— 抄一遍就会和写入侧漂。
    // 两个 JSON 列套 CAST；`agents` 这个词在别的列名里不出现（`first_agent_reply_time`
    // 是 `agent_` 不是 `agents`），所以按名字替换是安全的。
    let cols = EVENT_FACT_COLS
        .replace("source_msg_ids", "CAST(source_msg_ids AS CHAR)")
        .replace("agents", "CAST(agents AS CHAR)");
    // `id` **接在末尾**，不放开头：前 12 个下标是按 `EVENT_FACT_COLS` 的顺序取的，
    // 插在前面会让它们整体错位一格，而 8 列都是字符串、错位类型兼容、编译通过 ——
    // 正是 `ingest` 那边论证过的那种静默错法。
    //
    // ⚠️ **`id` 是 `store` 与 `recompute` 之间的搬运物，不进 [`Event`]。**
    // `Event` 的契约是「只装事实列」，而 `id` 是数据库的行标识不是业务事实；
    // 它单独一路并排传给 [`retag_room`]，跟 `events` / `types` 那对平行切片同一个形态。
    let sql = format!(
        "SELECT {cols}, id FROM {T_EVENT} \
         WHERE corpid = ? AND roomid = ? AND occurred_on BETWEEN ? AND ? \
         ORDER BY first_msg_time, summary"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(corp)
        .bind(room)
        .bind(since)
        .bind(until)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| {
            let role: String = r.get(8);
            let id: u64 = r.get(12);
            Ok((
                id,
                Event {
                    corpid: r.get(0),
                    roomid: r.get(1),
                    source_msg_ids: serde_json::from_str(&r.get::<String, _>(2))?,
                    first_msg_time: r.get::<NaiveDateTime, _>(3),
                    last_msg_time: r.get::<NaiveDateTime, _>(4),
                    first_agent_reply_time: r.get(5),
                    occurred_on: r.get(6),
                    asker: r.get(7),
                    asker_role: Role::parse(&role).ok_or_else(|| {
                        format!("库里的 asker_role「{role}」不是 INTERNAL/EXTERNAL")
                    })?,
                    agents: serde_json::from_str(&r.get::<String, _>(9))?,
                    first_responder: r.get(10),
                    summary: r.get(11),
                },
            ))
        })
        .collect::<Result<Vec<(u64, Event)>, BoxError>>()
        .map(|v| v.into_iter().unzip())
}

/// 词表升版后的**重打标**：一个群、一段时间、**一个事务**。
///
/// 与 [`write_room`] 的分片删重写是两件事，别混：
///   * **只写标注列**（`event_type` / `taxonomy_version`），事实列一个字节不动 ——
///     所以它**不触碰承重不变量 1**，冻结区照样可以重打标（那正是标注列的定义：
///     「任何时候可写，但只有词表升版这一个原因」）。
///   * **不需要承重不变量 2 的两分片同事务**。那条约束是关于 event 在分片之间移动
///     （`occurred_on` 由模型判断的首条消息决定），而更新标注列不移动任何行。
///     一个群一个事务只是为了「标注列和客服日指标不会各对一半」。
///
/// **按行 `id` 定位。** 曾经按 `summary`：分组之后每个标签组合各发一条
/// `UPDATE … occurred_on BETWEEN … AND summary IN (…)`，而 `summary` 是 `TEXT`、
/// 索引不上，于是每条语句都要在 `idx_shard` 上把该群该区间**重扫一遍** ——
/// D 个不同标签组合就是 D 遍，`O(D × 该群行数)`，还是在持锁的事务里。
/// 这是整条线上唯一的实质超线性点。
///
/// 换 `id` 不丢任何东西：[`read_events`] 不去重（没有 `GROUP BY`），
/// 同一个 summary 的每一行都在返回里各占一个 id，所以覆盖的行集**一字不差**——
/// 「一条语句顺手覆盖同 summary 多行」是那个写法的附带效率，不是正确性依赖。
/// `id` 仍然**不进 [`Event`]**（事实列契约），它跟 `types` 一样是平行切片。
///
/// `corpid` / `roomid` / `occurred_on` 三个条件**留着不删**：主键定位下它们不花钱，
/// 但万一 id 串了群，它们让语句改不动别人的行 —— 失败隔离粒度由构造守住。
///
/// 返回真正被改动的行数。⚠️ MySQL 默认只数**值发生了变化**的行 —— 重跑一次
/// recompute 会返回 0，那是幂等，不是失败。
#[allow(clippy::too_many_arguments)]
pub async fn retag_room(
    pool: &MySqlPool,
    corp: &str,
    room: &str,
    since: NaiveDate,
    until: NaiveDate,
    ids: &[u64],
    types: &[Labels],
    taxonomy_version: &str,
    agent: &[AgentRow],
) -> Result<u64, BoxError> {
    assert_eq!(
        ids.len(),
        types.len(),
        "types 必须与 events 一一对应（构造保证）"
    );
    // 按**整个 Labels** 分组，不按主类 —— 按主类分组会把「主类相同、副类不同」的
    // 两批行并成一条 UPDATE，其中一批的 `event_types` 会被写成另一批的。
    let mut by_type: std::collections::BTreeMap<&Labels, Vec<u64>> =
        std::collections::BTreeMap::new();
    for (id, t) in ids.iter().zip(types) {
        by_type.entry(t).or_default().push(*id);
    }

    let mut tx = pool.begin().await?;
    let mut changed = 0u64;
    for (t, mut group) in by_type {
        // 同一行不会在 `read_events` 的结果里出现两次，但去重的代价是零、
        // 而重复 id 会让 `rows_affected` 的账不准 —— 顺手排掉。
        group.sort_unstable();
        group.dedup();
        for chunk in group.chunks(IN_MAX) {
            let sql = format!(
                "UPDATE {T_EVENT} SET event_type = ?, event_types = ?, taxonomy_version = ? \
                 WHERE corpid = ? AND roomid = ? AND occurred_on BETWEEN ? AND ? \
                 AND id IN ({})",
                holes(chunk.len())
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(t.primary())
                .bind(serde_json::to_string(t.all())?)
                .bind(taxonomy_version)
                .bind(corp)
                .bind(room)
                .bind(since)
                .bind(until);
            for x in chunk {
                q = q.bind(*x);
            }
            changed += q.execute(&mut *tx).await?.rows_affected();
        }
    }

    // 客服日指标整段删重写 —— 语义键含 event_type + taxonomy_version，重打标必然改它。
    // 键含 room，所以这里仍然嵌在「群 × 日」的粒度里（承重不变量 5 的结构保证不变）。
    let sql = format!("DELETE FROM {T_AGENT} WHERE corpid = ? AND room = ? AND dt BETWEEN ? AND ?");
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(corp)
        .bind(room)
        .bind(since)
        .bind(until)
        .execute(&mut *tx)
        .await?;
    for chunk in agent.chunks(BATCH) {
        let sql = format!(
            "INSERT INTO {T_AGENT} ({AGENT_COLS}) VALUES {}",
            vec![values(AGENT_COLS); chunk.len()].join(", ")
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = q
                .bind(&r.corp)
                .bind(&r.room)
                .bind(&r.agent)
                .bind(r.dt)
                .bind(&r.event_type)
                .bind(&r.taxonomy_version)
                .bind(r.event_count);
        }
        q.execute(&mut *tx).await?;
    }

    tx.commit().await?;
    Ok(changed)
}

/// 启动期自检：跑批读写的**五张**表在不在，列对不对。
///
/// 第五张 `b_merchant_group_taxonomy` 从 ⑤ 的 v1 起进来 —— 每轮开跑都要读它取词表。
/// **查的是表和列，不是行数**：v0 期这张表一行都没有是正常状态（「还没有词表」），
/// 不是启动失败。
///
/// 存在的理由是一条真实事故：改了 `schema.sql` 但 dev 库没迁移，
/// **抽取跑完 23 分钟才在落库那步炸掉**（`1054 Unknown column`）。跑批是无人值守的，
/// 让 DDL 漂移在第一秒暴露，而不是在烧完一轮 token 之后。
pub async fn check_schema(pool: &MySqlPool) -> Result<(), BoxError> {
    for (table, cols) in [
        (T_EVENT, EVENT_COLS),
        (T_GROUP, GROUP_COLS),
        (T_AGENT, AGENT_COLS),
        (T_FAILURE, FAILURE_COLS),
        (T_TAXONOMY, TAXONOMY_COLS),
    ] {
        let rows = sqlx::query(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = ?",
        )
        .bind(table)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            return Err(format!("表 {table} 不存在 —— 先人工跑一次 schema.sql").into());
        }
        let have: std::collections::BTreeSet<String> = rows
            .iter()
            .map(|r| r.get::<String, _>(0).to_lowercase())
            .collect();
        let missing: Vec<&str> = cols
            .split(',')
            .map(str::trim)
            .filter(|c| !have.contains(&c.to_lowercase()))
            .collect();
        if !missing.is_empty() {
            return Err(format!("表 {table} 缺列 {missing:?} —— schema.sql 漂移了").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extract::Event, ingest::Role};

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, day).unwrap()
    }

    fn ev(day: u32) -> Event {
        Event {
            corpid: "C".into(),
            roomid: "R".into(),
            source_msg_ids: vec!["m1".into()],
            first_msg_time: d(day).and_hms_opt(9, 0, 0).unwrap(),
            last_msg_time: d(day).and_hms_opt(9, 0, 0).unwrap(),
            first_agent_reply_time: None,
            occurred_on: d(day),
            asker: "EXT".into(),
            asker_role: Role::External,
            agents: vec![],
            first_responder: None,
            summary: "商家要求加单".into(),
        }
    }

    /// 承重不变量 1：写穿冻结区必须在写之前就被挡住。
    #[test]
    fn events_outside_the_window_are_reported_not_written() {
        let w = Window::span(d(25), d(26));
        assert!(
            stray_days(&[ev(25), ev(26)], &w).is_empty(),
            "窗口内的不该报"
        );
        assert_eq!(
            stray_days(&[ev(25), ev(24), ev(27), ev(24)], &w),
            [d(24), d(27)]
        );
    }

    /// 占位符个数从列名串数出来 —— 加列忘了改数字就是一次运行期的列数不匹配。
    #[test]
    fn placeholder_count_follows_the_column_list() {
        assert_eq!(EVENT_COLS.split(',').count(), 15);
        assert_eq!(GROUP_COLS.split(',').count(), 11);
        assert_eq!(AGENT_COLS.split(',').count(), 7);
        assert_eq!(FAILURE_COLS.split(',').count(), 4);
        assert_eq!(TAXONOMY_COLS.split(',').count(), 4);
        // 读回来还原 Event 的那 12 列，必须就是 EVENT_COLS 去掉末尾两个标注列 ——
        // 加一列事实列却忘了改这里，`read_events` 会静默少读一个字段。
        assert_eq!(EVENT_FACT_COLS.split(',').count(), 12);
        assert!(
            EVENT_COLS.starts_with(EVENT_FACT_COLS),
            "事实列不再是 EVENT_COLS 的前缀了"
        );
        assert_eq!(
            EVENT_COLS[EVENT_FACT_COLS.len()..].trim_start_matches(", "),
            "event_type, event_types, taxonomy_version"
        );
        // `read_events` 的 CAST 按列名替换 —— 这两个名字必须各自只出现一次，
        // 否则会替换到别的列上（`first_agent_reply_time` 不含 `agents`，这条钉住它）。
        assert_eq!(EVENT_FACT_COLS.matches("agents").count(), 1);
        assert_eq!(EVENT_FACT_COLS.matches("source_msg_ids").count(), 1);
        assert_eq!(values("a, b, c"), "(?, ?, ?)");
        assert_eq!(holes(3), "?, ?, ?");
    }
}
