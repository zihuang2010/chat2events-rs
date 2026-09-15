//! **标注列与分类指标的唯一写入方** —— 承重不变量 5（客服分类指标整群发布）的守卫。
//!
//! 标注列「任何时候可写，但只有词表升版这一个原因」，所以这里**不受冻结约束**
//! （不变量 1 管的是事实列），也**不需要两分片同事务**（不变量 2 管的是 event 在
//! 分片之间移动，而更新标注列不移动任何行）。
//!
//! 本群全部批次成功之后才发布 `metric_agent_daily`；失败保留已存事实与已完成批次的
//! 标签，分类指标不发布，`classification_status='failed'`。

use super::sql::{
    AGENT_COLS, BATCH, FAILURE_COLS, IN_MAX, Shard, T_AGENT, T_EVENT, T_FAILURE, T_GROUP, holes,
    values,
};
use crate::{BoxError, stage::classify::Label, stage::metrics::AgentRow};
use chrono::NaiveDate;
use sqlx::{MySql, MySqlPool, Transaction};
use std::collections::BTreeMap;

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
    types: &[Label],
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
    types: &[Label],
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
    // 影响面 = 本次窗口。打标失败不使事实失效（`OK_DAYS` 只看 `stage='extract'`），
    // 但窗口照记 —— 运维查「这个群哪几天没打上标」要它。
    .bind(since)
    .bind(until)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn write_labels(
    tx: &mut Transaction<'_, MySql>,
    shard: Shard<'_>,
    ids: &[u64],
    types: &[Label],
    taxonomy_version: &str,
) -> Result<u64, BoxError> {
    let (corp, room, since, until) = shard.parts();
    assert_eq!(
        ids.len(),
        types.len(),
        "types 必须与 events 一一对应（构造保证）"
    );
    // 同一个类的行并成一条 UPDATE。
    let mut by_type: std::collections::BTreeMap<&Label, Vec<u64>> =
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
                "UPDATE {T_EVENT} SET event_type = ?, taxonomy_version = ? \
                 WHERE corpid = ? AND roomid = ? AND occurred_on BETWEEN ? AND ? \
                 AND id IN ({})",
                holes(chunk.len())
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(t.type_id())
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
