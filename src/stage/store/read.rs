//! **只读取数** —— 四个读者：每轮开跑取一次词表 · `taxonomy` 归纳 ·
//! `recompute` 重打标 · `daily::recover` 补齐未完成分类。
//!
//! 独立打标按群读取已保存事件；channel 不搬运正文。
//! 全部是 `SELECT`，一条写语句都不许出现在这个文件里。

use super::sql::{EVENT_FACT_COLS, Shard, T_EVENT, T_GROUP, T_TAXONOMY, TAXONOMY_COLS};
use crate::{
    BoxError,
    stage::classify::{Labels, TaxonomyType},
    stage::extract::Event,
    stage::ingest::Role,
};
use chrono::{NaiveDate, NaiveDateTime};
use futures_util::TryStreamExt;
use sqlx::{MySqlPool, Row};
use std::collections::BTreeMap;

/// 从群日状态发现待恢复项，零事件同样在内；流式读取避免历史待办全部驻留。
pub fn unfinished_days(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> impl futures_util::Stream<Item = Result<(String, String, NaiveDate, u32), sqlx::Error>> + '_ {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT corpid, roomid, dt, event_count FROM {T_GROUP} \
        WHERE extraction_status = 'ok' AND classification_status IN ('pending','failed') AND dt BETWEEN ? AND ? \
        ORDER BY corpid, roomid, dt"
    )))
    .bind(since)
    .bind(until)
    .fetch(pool)
}

pub async fn read_event_labels(
    pool: &MySqlPool,
    shard: Shard<'_>,
    classifier: &crate::stage::classify::Classifier,
) -> Result<BTreeMap<u64, Option<Labels>>, BoxError> {
    let (corp, room, since, until) = shard.parts();
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, event_type, CAST(event_types AS CHAR), taxonomy_version \
        FROM {T_EVENT} WHERE corpid=? AND roomid=? AND occurred_on BETWEEN ? AND ?"
    )))
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
    // `id` **接在末尾**，不放开头：前 13 个下标是按 `EVENT_FACT_COLS` 的顺序取的，
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
        let last_role: Option<String> = r.get(12);
        let id: u64 = r.get(14);
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
            // 与 `asker_role` 同一条契约、同一个解析点。`None` 只可能是加这一列之前
            // 抽取的历史行；有值却认不出，说明有人手改过库，该显式报错。
            last_msg_role: last_role
                .map(|s| {
                    Role::parse(&s)
                        .ok_or_else(|| format!("库里的 last_msg_role「{s}」不是 INTERNAL/EXTERNAL"))
                })
                .transpose()?,
            followup_wait_max_sec: r.get(13),
            // **故意不读。** 这条路径服务的是 ⑤ 打标和 `recompute` 重打标，两者只用
            // `summary`；把正文一起读回来，`recompute` 的全历史扫描就会把整个正文
            // 语料拉进内存。`source_messages` 因此不在 `EVENT_FACT_COLS` 里 ——
            // 上面那个 `cols` 正是从它派生的，所以这里少一列不是漏，是设计。
            // 空 `Vec` 在这里只意味着「没读」（见 `Event::source_messages` 的注释）。
            source_messages: Vec::new(),
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
