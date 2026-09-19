//! **事实列的唯一写入方** —— 承重不变量 1（冻结）与 2（两分片同事务）的守卫。
//!
//! 一个群一次抽取的事实写入 = **一个事务**。`occurred_on = date(first_msg_time)`，
//! 而「首条来源消息是哪条」由模型判断 —— 同一个 event 会在 `T-3` / `T-2` 两个分片
//! 之间移动。分两个事务提交，中间失败就会造成它**一个分片都不在**，或者**两个分片都在**。
//!
//! 标注列（`event_type` / `taxonomy_version`）不在这里写，
//! 它们是 `labels` 的事：初始 NULL，等打标阶段独立提交。

use super::sql::{
    AGENT_MSG_COLS, BATCH, EVENT_FACT_COLS, FAILURE_COLS, GROUP_COLS, Shard, T_AGENT, T_AGENT_MSG,
    T_EVENT, T_FAILURE, T_GROUP, T_REWRITE, values,
};
use crate::{
    BoxError,
    stage::extract::Event,
    stage::metrics::{AgentMsgRow, GroupRow},
    window::Window,
};
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

/// **只记一行账，一张指标表都不碰** —— 拉取失败与「整轮预算用完、这个群没轮到」走这里。
///
/// 这两种情况**连消息都没读过**：`group` 表写 0 就是拿 0 冒充「没算出来」（不变量 4），
/// 所以一行不写；`event` / `agent` 表同理不动（不变量 5）。剩下要做的只有留一行账 ——
/// 少这一行，库里对这批群是**整行缺失**，几个月后报表上的洞没有任何东西解释得了，
/// 因为 `Tally` 只活在那一轮的内存里。
///
/// 一条 `INSERT`，**不需要事务**（不变量 2 管的是事实的两个分片，这里一个事实都不写）。
///
/// ⚠️ **三种失败仍共用 `stage = "extract"`，而这已经不再是数据问题。**
///
/// 曾经是：`OK_DAYS` 按 `stage='extract'` 判事实新鲜度，子查询**不带日期条件**，
/// 于是一次「没轮到」会把这个群**全部历史**标成 `unknown` —— 含冻结区里早已成功的天，
/// 而 `OK_DAYS` 是四个聚合接口的**分母边界**、冻结区又不会再被重抽，那个 unknown
/// 是永久的。数字偏小，且看起来正常。
///
/// 修的**不是 `stage`，是影响面**：`window_since` / `window_until` 现在跟着每一行
/// `run_failure` 落库（见 `FAILURE_COLS`），两处读新鲜度的查询都按它把失败夹在
/// 自己那个窗口里。三种原因对新鲜度的后果本来就相同 ——「这个群的窗口本轮没被
/// 重新处理，事实可能落后」—— 所以让它们共用一个 `stage` 是**对的**，
/// 给拉取失败单独一个 stage 反而等于断言「它不影响新鲜度」，而那是假的。
///
/// `stage` 保持参数：将来想让运维能 `GROUP BY stage` 回答「这个群为什么没数据」时，
/// 给 `schema.sql` 的枚举加值、改这里的调用点一个字符串即可。那是可诊断性，不是正确性。
/// 把过了保留期的 `source_messages` 置 NULL —— **原文的留存期，和 raw 镜像同一个**。
///
/// ⚠️ **它写的是展示列，不是事实列。** `source_messages` 不在 `EVENT_FACT_COLS` 里
/// （`sql.rs` 有 `assert!` 钉着），所以这一条**不碰承重不变量 1**：冻结区的事实列照旧
/// 不可写，被清掉的只是给人看的原文快照。`web::query::read_source_messages` 对 NULL
/// 已经回 410「该事件早于原文留存」，语义现成。
///
/// **为什么非做不可**：同一批未脱敏的客户正文（实测 1850 条里 193 条手机号、88 条
/// 门牌址、101 处真名）有两个副本 —— 镜像区那份受 `raw_retention_months` 清理，
/// 而这一列**此前永生**，且看板无登录、谁都能下钻。留存期分家是顺手改出来的，不是决定。
///
/// ⚠️ **只清「刚过期的那一个月」，不是「所有比保留期旧的」。** `occurred_on < 边界`
/// 走 `idx_day` 是范围扫，但 `source_messages IS NOT NULL` 只能回表才判得了 ——
/// 稳态下那些行早就清干净了，于是每晚白扫整段历史、一行都清不到，
/// 而那个代价只跟「库里攒了多久」有关。夹一个下界，每轮正好清掉刚滑出保留期的那个月。
/// **代价**：跑批连续多天没跑会漏掉中间的月份，追平要人工执行一次（见 `docs/deploy.md`）。
///
/// 分批是为了**别长时间持锁**（跟 `BATCH` 在 `INSERT` 那边的占位符理由不是一回事，
/// 改一个不等于能改另一个）。返回清掉的行数。
pub async fn prune_source_messages(
    pool: &MySqlPool,
    since: NaiveDate,
    before: NaiveDate,
) -> Result<u64, BoxError> {
    let mut pruned = 0u64;
    loop {
        let n = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {T_EVENT} SET source_messages = NULL \
             WHERE occurred_on >= ? AND occurred_on < ? AND source_messages IS NOT NULL \
             LIMIT {BATCH}"
        )))
        .bind(since)
        .bind(before)
        .execute(pool)
        .await?
        .rows_affected();
        pruned += n;
        if (n as usize) < BATCH {
            return Ok(pruned);
        }
    }
}

/// `[since, before)` 里还有没有没清掉的原文快照 —— 返回最老的那一天。
///
/// ⚠️ **区间必须小**。`source_messages IS NOT NULL` 只能回表判，所以这条的代价
/// 正比于区间内的行数。[`prune_source_messages`] 夹一个月下界就是为了躲开
/// 「每晚白扫整段历史」，这里不能把那个代价换个地方加回来。
///
/// 调用方只拿它看**紧挨着清理窗口的那一个月** —— 跑批停摆后漏掉的月份必然紧贴着
/// 新的清理窗口下沿，看那一个月就够触发告警；真要找出全部漏月是人工排查的活，
/// SQL 在 `docs/deploy.md`。
pub async fn oldest_unpruned_source_messages(
    pool: &MySqlPool,
    since: NaiveDate,
    before: NaiveDate,
) -> Result<Option<NaiveDate>, BoxError> {
    let sql = format!(
        "SELECT MIN(occurred_on) FROM {T_EVENT} \
         WHERE occurred_on >= ? AND occurred_on < ? AND source_messages IS NOT NULL"
    );
    let (oldest,): (Option<NaiveDate>,) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(since)
        .bind(before)
        .fetch_one(pool)
        .await?;
    Ok(oldest)
}

/// 记一笔「这次窗口写穿了冻结区」。**只在 `window_since < frozen_before` 时调用**，
/// 判断在 `daily::run_span` 里做（那是三个调用方的共同必经之路，理由见 `check_window`）。
///
/// `rooms` 为空表示没挑群 —— 存 `NULL` 不存 `[]`，两者语义不同：
/// 「窗口内全部群」和「一个群都没挑中」在这张表上必须分得开。
///
/// **失败不掀翻整轮**由调用方决定（它是审计不是事实）；这里只管写。
pub async fn record_rewrite(
    pool: &MySqlPool,
    run_date: NaiveDate,
    days: &Window,
    frozen_before: NaiveDate,
    rooms: &[String],
) -> Result<(), BoxError> {
    let sql = format!(
        "INSERT INTO {T_REWRITE} (run_date, window_since, window_until, frozen_before, rooms) \
         VALUES (?, ?, ?, ?, ?)"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_date)
        .bind(days.since())
        .bind(days.until())
        .bind(frozen_before)
        // JSON 列走 `to_string` 绑字符串，和 `source_msg_ids` / `agents` 一个写法
        // （sqlx 没开 json feature）。`None` 进去就是 SQL NULL。
        .bind(
            (!rooms.is_empty())
                .then(|| serde_json::to_string(rooms))
                .transpose()?,
        )
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn record_failure(
    pool: &MySqlPool,
    run_date: NaiveDate,
    shard: Shard<'_>,
    stage: &str,
    reason: &str,
) -> Result<(), BoxError> {
    let (corp, room, since, until) = shard.parts();
    let sql = format!(
        "INSERT INTO {T_FAILURE} ({FAILURE_COLS}) VALUES {}",
        values(FAILURE_COLS)
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_date)
        .bind(corp)
        .bind(room)
        .bind(reason)
        .bind(stage)
        // 影响面 = 本次窗口，不是全历史。理由见 `FAILURE_COLS`。
        .bind(since)
        .bind(until)
        .execute(pool)
        .await?;
    Ok(())
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
///
/// `agent_msg` 是客服日消息量，**跟 `group` 同一个待遇、和 `events` 无关** ——
/// 消息数不依赖抽取，抽取失败的群照写；拉取失败时调用方传空切片，于是一行不写。
/// 打标和重打标都不碰这张表（那两条路只经过 `labels.rs`）。
///
/// 八个参数。**唯一一组会静默递反的（corp / room / days）已经收进 [`Shard`]**，
/// 剩下的每个都是不同类型，递反是编译错误 —— 再包一个 `struct` 只是把同样的字段
/// 换个地方写一遍。
#[allow(clippy::too_many_arguments)]
pub async fn write_room(
    pool: &MySqlPool,
    run_date: NaiveDate,
    shard: Shard<'_>,
    events: Option<&[Event]>,
    reason: Option<&str>,
    group: &[GroupRow],
    agent_msg: &[AgentMsgRow],
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
                // 影响面 = 本次窗口，不是全历史。理由见 `FAILURE_COLS`。
                .bind(days.since())
                .bind(days.until())
                .execute(&mut *tx)
                .await?;
        }
        Some(evs) => {
            // **`BETWEEN` 而不是逐日 `IN`。** `Window` 的「非空、连续、升序」由构造
            // 保证（`window.rs`），所以两者选中的行**一字不差**；而 `IN` 的占位符
            // 个数 = 窗口天数，日常 2 天无所谓，`src/bin/backfill.rs` 给一个上千天的
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

            // 事实列 ＋ 展示列。`source_messages` **不在 `EVENT_FACT_COLS` 里**
            // （理由见那个常量的注释：进去了 `read_events` 就会跟着读正文），
            // 所以写入侧在这里显式拼上，占位符个数照旧从列名串自己数。
            let cols = format!("{EVENT_FACT_COLS}, source_messages");
            for chunk in evs.chunks(BATCH) {
                let sql = format!(
                    "INSERT INTO {T_EVENT} ({cols}) VALUES {}",
                    vec![values(&cols); chunk.len()].join(", ")
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
                        .bind(&e.summary)
                        .bind(e.last_msg_role.map(|r| r.as_str()))
                        .bind(e.followup_wait_max_sec)
                        .bind(serde_json::to_string(&e.source_messages)?);
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

    // metric_agent_msg_daily 也用 REPLACE，语义键是 uk_agent_msg_daily。
    // **不做删重写**：raw 只增不删，所以同一个「群 × 日」的客服集合只会变大不会变小，
    // 没有需要清掉的陈旧行。空切片（拉取失败）自然一行不写，跟 `group` 一个道理。
    // ⚠️ 这一段**不判 `events`** —— 消息数不依赖抽取，那正是这张表存在的理由：
    //    模型挂了，「谁在群里说了多少」仍然是已知的。
    for chunk in agent_msg.chunks(BATCH) {
        let sql = format!(
            "REPLACE INTO {T_AGENT_MSG} ({AGENT_MSG_COLS}) VALUES {}",
            vec![values(AGENT_MSG_COLS); chunk.len()].join(", ")
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = q
                .bind(&r.corp)
                .bind(&r.room)
                .bind(&r.agent)
                .bind(r.dt)
                .bind(r.msg_count);
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
