//! **事实列的唯一写入方** —— 承重不变量 1（冻结）与 2（两分片同事务）的守卫。
//!
//! 一个群一次抽取的事实写入 = **一个事务**。`occurred_on = date(first_msg_time)`，
//! 而「首条来源消息是哪条」由模型判断 —— 同一个 event 会在 `T-3` / `T-2` 两个分片
//! 之间移动。分两个事务提交，中间失败就会造成它**一个分片都不在**，或者**两个分片都在**。
//!
//! 标注列（`event_type` / `event_types` / `taxonomy_version`）不在这里写，
//! 它们是 `labels` 的事：初始 NULL，等打标阶段独立提交。

use super::sql::{
    BATCH, EVENT_FACT_COLS, FAILURE_COLS, GROUP_COLS, Shard, T_AGENT, T_EVENT, T_FAILURE, T_GROUP,
    values,
};
use crate::{BoxError, extract::Event, metrics::GroupRow, window::Window};
use chrono::{NaiveDate, NaiveDateTime};
use sqlx::MySqlPool;
use std::collections::BTreeMap;

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

#[cfg(test)]
mod tests {
    use super::super::tests::{d, ev};
    use super::*;

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
}
