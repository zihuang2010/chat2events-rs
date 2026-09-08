//! ⑦ 落库 store —— **MySQL 唯一写入方。所有写库 SQL 都在这个文件里，一条都不许外流。**
//!
//! 不是「存储层抽象接口」—— MySQL 是当前唯一目标，这里只保证写库代码集中在一处。
//! 建表走手写的 `schema.sql`，人工执行一次；本模块**不碰 DDL**。
//!
//! **一个群一次抽取的事实写入 = 一个事务**（承重不变量 2）。标签由独立阶段按批更新。
//! `occurred_on = date(first_msg_time)`，而「首条来源消息是哪条」由模型判断 ——
//! 同一个 event 会在 `T-3` / `T-2` 两个分片之间移动。分两个事务提交，中间失败就会造成
//! 它**一个分片都不在**，或者**两个分片都在**。
//!
//! 失败隔离粒度 = 群 × 本次窗口，三种失败语义不同（承重不变量 3 / 4 / 5）：
//!
//! | 失败在哪 | `event` / `agent` 表 | `group` 表 | `run_failure` |
//! |---|---|---|---|
//! | **拉取** | 不写 | **不写**（连消息都不全，写 0 就是拿 0 冒充「没算出来」） | 写 |
//! | **抽取** | 不写 | 写，`extraction_status='failed'`，事件级 NULL | 写 |
//! | **打标** | 保留已存事实和成功批次标签，不发布客服分类指标 | `classification_status='failed'` | 写 |
//! | **没轮到**（整轮预算用完） | 不写 | **不写**（同拉取：消息根本没读过） | 写 |
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
use futures_util::TryStreamExt;
use sqlx::{MySql, MySqlPool, Row, Transaction};
use std::collections::BTreeMap;

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
    extraction_status, classification_status, agent_accounts, fact_completed_time";
const AGENT_COLS: &str =
    "corpid, room, agent, dt, event_type, taxonomy_version, event_count, official_user_id";
/// `run_failure` 的列。四组列名里它曾经是唯一没有常量的一组 —— INSERT 语句里写一遍、
/// [`check_schema`] 的清单里再写一遍，也没进那条占位符个数的测试。
const FAILURE_COLS: &str = "run_date, corpid, roomid, reason, stage";
/// [`EVENT_COLS`] 的前 12 个 —— **事实列**。末尾三个 `event_type` / `event_types` /
/// `taxonomy_version` 是标注列，不在这里：[`read_events`] 还原的是 `Event`，而 `Event`
/// 只装事实列（标签不刻在它上面，是每次算出来的）。下面那条测试钉住「它必须是
/// EVENT_COLS 的前缀」。
const EVENT_FACT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary";
/// `IN (...)` 的集合上限（`docs/database-conventions.md`：控制在 1000 以内）。
const IN_MAX: usize = 1000;

/// 从群日状态发现待恢复项，零事件同样在内；流式读取避免历史待办全部驻留。
pub fn unfinished_days(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> impl futures_util::Stream<Item = Result<(String, String, NaiveDate, u32), sqlx::Error>> + '_ {
    sqlx::query_as("SELECT corpid, roomid, dt, event_count FROM b_merchant_group_metric_daily \
        WHERE extraction_status = 'ok' AND classification_status IN ('pending','failed') AND dt BETWEEN ? AND ? \
        ORDER BY corpid, roomid, dt")
        .bind(since).bind(until).fetch(pool)
}

pub async fn read_event_labels(
    pool: &MySqlPool,
    shard: Shard<'_>,
    classifier: &crate::classify::Classifier,
) -> Result<BTreeMap<u64, Option<Labels>>, BoxError> {
    let (corp, room, since, until) = shard.parts();
    let mut rows = sqlx::query(
        "SELECT id, event_type, CAST(event_types AS CHAR), taxonomy_version \
        FROM b_merchant_group_event WHERE corpid=? AND roomid=? AND occurred_on BETWEEN ? AND ?",
    )
    .bind(corp)
    .bind(room)
    .bind(since)
    .bind(until)
    .fetch(pool);
    let mut labels = BTreeMap::new();
    while let Some(row) = rows.try_next().await? {
        labels.insert(
            row.try_get(0)?,
            classifier.saved_labels(row.try_get(1)?, row.try_get(2)?, row.try_get(3)?)?,
        );
    }
    Ok(labels)
}

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

/// **失败隔离粒度 —— 群 × 本次窗口。** 承重不变量 2 / 3 / 5 全都以它为单位。
///
/// 这三样永远一起出现、永远是同一个意思，而 `corp` 和 `room` **都是 `&str`** ——
/// 按位置传的时候递反了**编译通过、测试也可能通过**（同一个 corp 下尤其），
/// 上线后的表现是「事件写到别的群名下」。`read_events` 那条注释已经论证过同一种
/// 静默错法（8 列都是字符串、错位类型兼容、编译通过），只是当时没往函数签名上推。
///
/// `days` 是 `&Window` 而不是一对裸日期：[`write_room`] 要拿整个窗口喂
/// [`stray_days`]（承重不变量 1 的守卫），那是对 `Window` 的真实语义依赖，
/// 不只是取两个端点。`Window` 的「非空、连续、升序」由它自己的构造保证。
#[derive(Clone, Copy)]
pub struct Shard<'a> {
    pub corp: &'a str,
    pub room: &'a str,
    pub days: &'a Window,
}

impl<'a> Shard<'a> {
    pub fn new(corp: &'a str, room: &'a str, days: &'a Window) -> Self {
        Self { corp, room, days }
    }

    /// 摊平成四个绑定值 —— 纯事件区间操作只用得上两个端点，不用整个窗口。
    fn parts(&self) -> (&'a str, &'a str, NaiveDate, NaiveDate) {
        (self.corp, self.room, self.days.since(), self.days.until())
    }
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

/// 独立保存一个群的事实与抽取指标；标签初始为 NULL，分类指标等待打标完成。
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
/// `accounts` 保存消息元信息里的账号映射，独立打标阶段不必重读原文。
pub async fn write_room(
    pool: &MySqlPool,
    run_date: NaiveDate,
    shard: Shard<'_>,
    events: Option<&[Event]>,
    reason: Option<&str>,
    group: &[GroupRow],
    accounts: &BTreeMap<String, String>,
) -> Result<(), BoxError> {
    let Shard { corp, room, days } = shard;
    if let Some(evs) = events {
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
                .bind("extract")
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

            for chunk in evs.chunks(BATCH) {
                let sql = format!(
                    "INSERT INTO {T_EVENT} ({EVENT_FACT_COLS}) VALUES {}",
                    vec![values(EVENT_FACT_COLS); chunk.len()].join(", ")
                );
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                for e in chunk {
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
                        .bind(&e.summary);
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
    }

    // 事实完成凭据与标签修改时间分开；失败或旧库未知数据不能用通用修改时间冒充。
    let fact_completed_time: Option<NaiveDateTime> = if events.is_some() {
        Some(
            sqlx::query_scalar("SELECT CURRENT_TIMESTAMP(6)")
                .fetch_one(&mut *tx)
                .await?,
        )
    } else {
        None
    };
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
                .bind(r.status.as_str())
                .bind(if events.is_some() {
                    "pending"
                } else {
                    "failed"
                })
                .bind(serde_json::to_string(accounts)?)
                .bind(fact_completed_time);
        }
        q.execute(&mut *tx).await?;
    }

    // 提交之前任何一步 `?` 早退，`tx` 被 drop 时 sqlx 自动回滚 —— 不需要手写 rollback。
    tx.commit().await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 读 —— 三个读者：每轮开跑取一次词表 · `taxonomy` 归纳 · `recompute` 重打标。
// 独立打标按群读取已保存事件；channel 不搬运正文。
// ─────────────────────────────────────────────────────────────────────────────

/// 取指定版本的词表；版本和内容由 Classifier 统一校验。
/// 只有显式 v0 允许空表，正式版本查不到行在构造时失败。
///
/// `ORDER BY type_id` 不是装饰：词表会被渲染成 system prompt，顺序变了 prompt 就变了，
/// 而 ⑤ 的确定性要求同样的词表给出同样的答案。库里的行序没有保证，这里钉死它。
pub async fn read_taxonomy<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::MySql>,
    version: &str,
) -> Result<Vec<TaxonomyType>, BoxError> {
    let sql =
        format!("SELECT {TAXONOMY_COLS} FROM {T_TAXONOMY} WHERE version = ? ORDER BY type_id");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(version)
        .fetch_all(executor)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(TaxonomyType {
                type_id: r.try_get(0)?,
                parent_name: r.try_get(1)?,
                name: r.try_get(2)?,
                description: r.try_get(3)?,
            })
        })
        .collect()
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
    shard: Shard<'_>,
) -> Result<(Vec<u64>, Vec<Event>), BoxError> {
    let (corp, room, since, until) = shard.parts();
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
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(corp)
        .bind(room)
        .bind(since)
        .bind(until)
        .fetch(pool);
    // 逐行还原，避免 SQL 原始结果、配对临时集合与 Event 全量同时驻留。
    let (mut ids, mut events) = (Vec::new(), Vec::new());
    let mut bytes = 0usize;
    while let Some(r) = rows.try_next().await? {
        let role: String = r.get(8);
        let id: u64 = r.get(12);
        let event = Event {
            corpid: r.get(0),
            roomid: r.get(1),
            source_msg_ids: serde_json::from_str(&r.get::<String, _>(2))?,
            first_msg_time: r.get::<NaiveDateTime, _>(3),
            last_msg_time: r.get::<NaiveDateTime, _>(4),
            first_agent_reply_time: r.get(5),
            occurred_on: r.get(6),
            asker: r.get(7),
            asker_role: Role::parse(&role)
                .ok_or_else(|| format!("库里的 asker_role「{role}」不是 INTERNAL/EXTERNAL"))?,
            agents: serde_json::from_str(&r.get::<String, _>(9))?,
            first_responder: r.get(10),
            summary: r.get(11),
        };
        bytes += std::mem::size_of::<Event>()
            + event.summary.len()
            + event.corpid.len()
            + event.roomid.len()
            + event.asker.len()
            + event.first_responder.as_ref().map_or(0, String::len)
            + event
                .source_msg_ids
                .iter()
                .chain(&event.agents)
                .map(|s| std::mem::size_of::<String>() + s.len())
                .sum::<usize>();
        if bytes > 32 * 1024 * 1024 {
            return Err("单群事件超过 32 MiB 读取预算，请缩小重打标日期范围".into());
        }
        ids.push(id);
        events.push(event);
    }
    Ok((ids, events))
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
pub async fn retag_room(
    pool: &MySqlPool,
    shard: Shard<'_>,
    ids: &[u64],
    types: &[Labels],
    taxonomy_version: &str,
    agent: &[AgentRow],
) -> Result<u64, BoxError> {
    let mut tx = pool.begin().await?;
    let changed = write_labels(&mut tx, shard, ids, types, taxonomy_version).await?;
    publish_classification(&mut tx, shard, agent).await?;
    tx.commit().await?;
    Ok(changed)
}

/// 一批标签独立提交，不等待同群其他批次，也不修改事件事实。
pub async fn update_event_labels(
    pool: &MySqlPool,
    shard: Shard<'_>,
    ids: &[u64],
    types: &[Labels],
    taxonomy_version: &str,
) -> Result<(), BoxError> {
    let mut tx = pool.begin().await?;
    write_labels(&mut tx, shard, ids, types, taxonomy_version).await?;
    tx.commit().await?;
    Ok(())
}

/// 所有批次成功后发布本群客服指标，完成状态与指标一起提交。
pub async fn finish_classification(
    pool: &MySqlPool,
    shard: Shard<'_>,
    agent: &[AgentRow],
) -> Result<(), BoxError> {
    let mut tx = pool.begin().await?;
    publish_classification(&mut tx, shard, agent).await?;
    tx.commit().await?;
    Ok(())
}

/// 打标失败只改变打标状态并记账，已保存事实与成功批次标签均保留。
pub async fn fail_classification(
    pool: &MySqlPool,
    run_date: NaiveDate,
    shard: Shard<'_>,
    reason: &str,
) -> Result<(), BoxError> {
    let (corp, room, since, until) = shard.parts();
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {T_GROUP} SET classification_status = 'failed' WHERE corpid = ? AND roomid = ? AND dt BETWEEN ? AND ?"
    )))
    .bind(corp).bind(room).bind(since).bind(until)
    .execute(&mut *tx).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {T_FAILURE} ({FAILURE_COLS}) VALUES {}",
        values(FAILURE_COLS)
    )))
    .bind(run_date)
    .bind(corp)
    .bind(room)
    .bind(reason)
    .bind("classify")
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn write_labels(
    tx: &mut Transaction<'_, MySql>,
    shard: Shard<'_>,
    ids: &[u64],
    types: &[Labels],
    taxonomy_version: &str,
) -> Result<u64, BoxError> {
    let (corp, room, since, until) = shard.parts();
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
            changed += q.execute(&mut **tx).await?.rows_affected();
        }
    }

    Ok(changed)
}

async fn publish_classification(
    tx: &mut Transaction<'_, MySql>,
    shard: Shard<'_>,
    agent: &[AgentRow],
) -> Result<(), BoxError> {
    let (corp, room, since, until) = shard.parts();
    // 客服日指标整段删重写 —— 语义键含 event_type + taxonomy_version，重打标必然改它。
    // 键含 room，所以这里仍然嵌在「群 × 日」的粒度里（承重不变量 5 的结构保证不变）。
    // 账号来自消息元信息，事件事实无法还原；删除前在同一事务内保留各员工各日的值。
    let sql = format!(
        "SELECT DISTINCT agent, dt, official_user_id FROM {T_AGENT} \
         WHERE corpid = ? AND room = ? AND dt BETWEEN ? AND ? AND official_user_id IS NOT NULL"
    );
    let accounts: Vec<(String, NaiveDate, String)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(corp)
        .bind(room)
        .bind(since)
        .bind(until)
        .fetch_all(&mut **tx)
        .await?;
    let mut accounts: BTreeMap<_, _> = accounts
        .into_iter()
        .map(|(agent, dt, account)| ((agent, dt), account))
        .collect();
    let saved: Vec<(NaiveDate, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT dt, CAST(agent_accounts AS CHAR) FROM {T_GROUP} \
         WHERE corpid = ? AND roomid = ? AND dt BETWEEN ? AND ? AND agent_accounts IS NOT NULL"
    )))
    .bind(corp)
    .bind(room)
    .bind(since)
    .bind(until)
    .fetch_all(&mut **tx)
    .await?;
    for (dt, saved) in saved {
        for (agent, account) in serde_json::from_str::<BTreeMap<String, String>>(&saved)? {
            accounts.insert((agent, dt), account);
        }
    }
    let sql = format!("DELETE FROM {T_AGENT} WHERE corpid = ? AND room = ? AND dt BETWEEN ? AND ?");
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(corp)
        .bind(room)
        .bind(since)
        .bind(until)
        .execute(&mut **tx)
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
                .bind(r.event_count)
                .bind(
                    r.official_user_id
                        .as_ref()
                        .or_else(|| accounts.get(&(r.agent.clone(), r.dt))),
                );
        }
        q.execute(&mut **tx).await?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {T_GROUP} SET classification_status = 'ok' WHERE corpid = ? AND roomid = ? AND dt BETWEEN ? AND ?"
    )))
    .bind(corp).bind(room).bind(since).bind(until)
    .execute(&mut **tx).await?;
    Ok(())
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
            "SELECT column_name, is_nullable, datetime_precision FROM information_schema.columns \
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
        for row in &rows {
            let name: String = row.get(0);
            if ((table == T_GROUP && name == "fact_completed_time")
                || (table == T_FAILURE && name == "gmt_created_time"))
                && row.get::<Option<u32>, _>(2) != Some(6)
            {
                return Err(
                    format!("{table}.{name} 必须使用微秒精度，请执行事实新鲜度升级").into(),
                );
            }
        }
        if table == T_EVENT {
            for row in &rows {
                let name: String = row.get(0);
                if ["event_type", "event_types", "taxonomy_version"].contains(&name.as_str())
                    && row.get::<String, _>(1) != "YES"
                {
                    return Err(format!(
                        "{table}.{name} 必须允许 NULL，先执行独立打标的表结构升级"
                    )
                    .into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extract::Event, ingest::Role};

    /// 为既有事务、冻结和重打标用例准备已完成两阶段处理的记录。
    #[allow(clippy::too_many_arguments)]
    async fn write_room(
        pool: &MySqlPool,
        run_date: NaiveDate,
        corp: &str,
        room: &str,
        days: &Window,
        events: Option<&[Event]>,
        labels: &[Labels],
        version: &str,
        reason: Option<&str>,
        group: &[GroupRow],
        agent: &[AgentRow],
    ) -> Result<(), BoxError> {
        let shard = Shard::new(corp, room, days);
        super::write_room(
            pool,
            run_date,
            shard,
            events,
            reason,
            group,
            &BTreeMap::new(),
        )
        .await?;
        if let Some(events) = events {
            let (ids, saved) = read_events(pool, shard).await?;
            let aligned: Vec<_> = saved
                .iter()
                .map(|event| {
                    labels[events
                        .iter()
                        .position(|source| {
                            source.summary == event.summary
                                && source.first_msg_time == event.first_msg_time
                        })
                        .unwrap()]
                    .clone()
                })
                .collect();
            update_event_labels(pool, shard, &ids, &aligned, version).await?;
            finish_classification(pool, shard, agent).await?;
        }
        Ok(())
    }

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

    #[tokio::test]
    #[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
    async fn mysql_pipeline_upgrade_matches_the_documented_schema() {
        let pool = crate::testutil::mysql_pool("pipeline_upgrade").await;
        sqlx::raw_sql(
            "ALTER TABLE b_merchant_group_event MODIFY event_type VARCHAR(64) NOT NULL, \
             MODIFY event_types JSON NOT NULL, MODIFY taxonomy_version VARCHAR(16) NOT NULL; \
             ALTER TABLE b_merchant_group_metric_daily DROP COLUMN classification_status, DROP COLUMN agent_accounts; \
             ALTER TABLE b_merchant_group_run_failure DROP COLUMN stage; \
             INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason) VALUES ('2026-08-28','C','R','历史失败');"
        ).execute(&pool).await.unwrap();
        assert!(check_schema(&pool).await.is_err());
        let section = include_str!("../docs/deploy.md")
            .split_once("### 独立打标流水线升级")
            .unwrap()
            .1;
        let sql = section
            .split_once("```sql\n")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0;
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        check_schema(&pool).await.unwrap();
        let (stage,): (String,) = sqlx::query_as("SELECT stage FROM b_merchant_group_run_failure")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stage, "extract", "历史失败保留原先的事实完整性语义");
        crate::testutil::drop_mysql_database(pool).await;
    }

    #[tokio::test]
    #[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
    async fn mysql_freshness_upgrade_keeps_legacy_evidence_unknown() {
        let pool = crate::testutil::mysql_pool("freshness_upgrade").await;
        sqlx::raw_sql("ALTER TABLE b_merchant_group_metric_daily DROP COLUMN fact_completed_time, DROP INDEX idx_corp_day; \
            ALTER TABLE b_merchant_group_event DROP INDEX idx_corp_day; \
            ALTER TABLE b_merchant_group_run_failure DROP INDEX idx_room_stage_time, MODIFY gmt_created_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, \
            MODIFY gmt_modified_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP; \
            INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,extraction_status) VALUES ('C','R','2026-08-25',0,0,'ok');")
            .execute(&pool).await.unwrap();
        assert!(check_schema(&pool).await.is_err());
        let sql = include_str!("../docs/deploy.md")
            .split_once("### 事实新鲜度与查询预算升级")
            .unwrap()
            .1
            .split_once("```sql\n")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0;
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        check_schema(&pool).await.unwrap();
        let evidence: Option<NaiveDateTime> =
            sqlx::query_scalar("SELECT fact_completed_time FROM b_merchant_group_metric_daily")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(evidence.is_none(), "不能用通用更新时间伪造历史事实凭据");
        let precision: u32 = sqlx::query_scalar("SELECT datetime_precision FROM information_schema.columns WHERE table_schema=DATABASE() AND table_name='b_merchant_group_run_failure' AND column_name='gmt_created_time'").fetch_one(&pool).await.unwrap();
        assert_eq!(precision, 6);
        crate::testutil::drop_mysql_database(pool).await;
    }

    #[tokio::test]
    #[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
    async fn mysql_room_writes_preserve_failure_atomicity_and_frozen_facts() {
        use crate::{
            classify::Classifier,
            config::Config,
            llm::Llm,
            metrics::{self, Attribution, Status},
            testutil,
        };
        use std::collections::BTreeMap;
        let pool = testutil::mysql_pool("store").await;
        check_schema(&pool).await.unwrap();
        let cfg: Config = toml::from_str(include_str!("../config.toml")).unwrap();
        let classifier = Classifier::new(
            "v0",
            vec![],
            Llm::new(&cfg.llm, &cfg.llm.classify, "unused-test-key".into()).unwrap(),
            &testutil::fresh_root("store", "cache"),
        )
        .unwrap();
        let labels = classifier.classify(&["旧事实", "新事实"]).await.unwrap();
        let w = Window::span(d(25), d(26));
        let frozen = ev(24);
        write_room(
            &pool,
            d(27),
            "C",
            "R",
            &Window::span(d(24), d(24)),
            Some(std::slice::from_ref(&frozen)),
            &labels[..1],
            "v0",
            None,
            &[],
            &[],
        )
        .await
        .unwrap();
        let mut events = vec![ev(25), ev(26)];
        for (i, e) in events.iter_mut().enumerate() {
            e.source_msg_ids = vec![format!("message-{i}"), format!("reply-{i}")];
            e.asker = "merchant-0000001".into();
            e.agents = vec!["agent-0000000001".into(), "agent-0000000002".into()];
            e.first_responder = Some(e.agents[0].clone());
            e.last_msg_time = e.first_msg_time + chrono::Duration::minutes(5);
            e.first_agent_reply_time = Some(e.last_msg_time);
        }
        let counts = BTreeMap::from([(d(25), (2, 2)), (d(26), (2, 2))]);
        let group = metrics::group_rows("C", "R", &w, &counts, Some(&events), Status::Ok);
        let mut agent = metrics::agent_rows(
            "C",
            "R",
            &events,
            &["__untyped__"; 2],
            "v0",
            Attribution::default(),
        );
        agent[0].official_user_id = Some("13523611718".into());
        write_room(
            &pool,
            d(27),
            "C",
            "R",
            &w,
            Some(&events),
            &labels,
            "v0",
            None,
            &group,
            &agent,
        )
        .await
        .unwrap();
        let initial = read_events(&pool, Shard::new("C", "R", &w)).await.unwrap();
        assert_eq!(initial.1, events, "所有事实列读写往返必须一致");
        let accounts: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT agent, official_user_id FROM b_merchant_group_agent_metric_daily ORDER BY dt",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            accounts,
            vec![
                (agent[0].agent.clone(), Some("13523611718".into())),
                (agent[1].agent.clone(), None),
            ]
        );

        let failed = metrics::group_rows("C", "R", &w, &counts, None, Status::Failed);
        write_room(
            &pool,
            d(27),
            "C",
            "R",
            &w,
            None,
            &[],
            "v0",
            Some("合成抽取失败"),
            &failed,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(
            read_events(&pool, Shard::new("C", "R", &w)).await.unwrap(),
            initial
        );
        let (agent_rows,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_agent_metric_daily")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(agent_rows, 2, "失败保留旧客服事实，由群日状态揭示残缺");
        let status: Vec<(String, Option<u32>)> = sqlx::query_as(
            "SELECT extraction_status, event_count FROM b_merchant_group_metric_daily ORDER BY dt",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(status, vec![("failed".into(), None); 2]);
        let (failures,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_run_failure")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(failures, 1);

        let empty = metrics::group_rows("C", "R", &w, &counts, Some(&[]), Status::Ok);
        write_room(
            &pool,
            d(27),
            "C",
            "R",
            &w,
            Some(&[]),
            &[],
            "v0",
            None,
            &empty,
            &[],
        )
        .await
        .unwrap();
        assert!(
            read_events(&pool, Shard::new("C", "R", &w))
                .await
                .unwrap()
                .1
                .is_empty()
        );
        let (remaining,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_agent_metric_daily")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
        let status: Vec<(String, Option<u32>)> = sqlx::query_as(
            "SELECT extraction_status, event_count FROM b_merchant_group_metric_daily ORDER BY dt",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(status, vec![("ok".into(), Some(0)); 2]);

        write_room(
            &pool,
            d(27),
            "C",
            "R",
            &w,
            Some(&events),
            &labels,
            "v0",
            None,
            &group,
            &agent,
        )
        .await
        .unwrap();
        let before = read_events(&pool, Shard::new("C", "R", &w)).await.unwrap();
        let mut broken = group.clone();
        broken[1].room = "R".repeat(65);
        assert!(
            write_room(
                &pool,
                d(27),
                "C",
                "R",
                &w,
                Some(&events[..1]),
                &labels[..1],
                "v0",
                None,
                &broken,
                &agent[..1]
            )
            .await
            .is_err()
        );
        assert_eq!(
            read_events(&pool, Shard::new("C", "R", &w)).await.unwrap(),
            before,
            "第二日写入失败应回滚全部事实和指标"
        );
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_agent_metric_daily")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 2);

        // 同一来源事件移到另一天，两个分片必须整体替换。
        let mut moved = events[0].clone();
        moved.occurred_on = d(26);
        moved.first_msg_time += chrono::Duration::days(1);
        moved.last_msg_time += chrono::Duration::days(1);
        moved.first_agent_reply_time = Some(moved.last_msg_time);
        let moved_group = metrics::group_rows(
            "C",
            "R",
            &w,
            &counts,
            Some(std::slice::from_ref(&moved)),
            Status::Ok,
        );
        let mut moved_agent = metrics::agent_rows(
            "C",
            "R",
            std::slice::from_ref(&moved),
            &["__untyped__"],
            "v0",
            Attribution::default(),
        );
        moved_agent[0].official_user_id = Some("staff.a".into());
        write_room(
            &pool,
            d(27),
            "C",
            "R",
            &w,
            Some(std::slice::from_ref(&moved)),
            &labels[..1],
            "v0",
            None,
            &moved_group,
            &moved_agent,
        )
        .await
        .unwrap();
        let before_retag = read_events(&pool, Shard::new("C", "R", &w)).await.unwrap();
        assert_eq!(before_retag.1, vec![moved.clone()]);
        let new_agent = metrics::agent_rows(
            "C",
            "R",
            &[moved],
            &["__untyped__"],
            "v1",
            Attribution::default(),
        );
        retag_room(
            &pool,
            Shard::new("C", "R", &w),
            &before_retag.0,
            &labels[..1],
            "v1",
            &new_agent,
        )
        .await
        .unwrap();
        assert_eq!(
            read_events(&pool, Shard::new("C", "R", &w)).await.unwrap(),
            before_retag,
            "重打标不改事实与行 id"
        );
        let tagged: (String, String) = sqlx::query_as("SELECT taxonomy_version, CAST(event_types AS CHAR) FROM b_merchant_group_event WHERE occurred_on='2026-08-26'").fetch_one(&pool).await.unwrap();
        assert_eq!(tagged.0, "v1");
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&tagged.1).unwrap(),
            labels[0].all()
        );
        assert_eq!(
            read_events(&pool, Shard::new("C", "R", &Window::span(d(24), d(24))))
                .await
                .unwrap()
                .1,
            vec![frozen]
        );
        let versions: Vec<(String,)> =
            sqlx::query_as("SELECT taxonomy_version FROM b_merchant_group_agent_metric_daily")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(versions, vec![("v1".into(),)]);
        let (account,): (Option<String>,) =
            sqlx::query_as("SELECT official_user_id FROM b_merchant_group_agent_metric_daily")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(account.as_deref(), Some("staff.a"), "重打标保留已有账号");
        testutil::drop_mysql_database(pool).await;
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
        assert_eq!(GROUP_COLS.split(',').count(), 14);
        assert_eq!(AGENT_COLS.split(',').count(), 8);
        assert_eq!(FAILURE_COLS.split(',').count(), 5);
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
