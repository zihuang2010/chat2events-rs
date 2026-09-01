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
    extract::Event,
    metrics::{AgentRow, GroupRow},
    window::Window,
};
use chrono::NaiveDate;
use sqlx::{MySqlPool, Row};

/// 一次 `INSERT` 最多带几行。MySQL 的预处理占位符上限是 65535，`event` 表 14 列 ——
/// 500 行 = 7000 个，留着一个数量级的余量。跑批一个群一天几百个事件，正常撞不到。
const BATCH: usize = 500;

/// 表名 —— DELETE / INSERT / [`check_schema`] 三处引用**同一个常量**。
/// `b_merchant_group_agent_metric_daily` 这个 35 字符的名字曾经写了 4 遍，
/// 打错一个字母是运行期的 `Table doesn't exist`，跟 `COL_*` 同一个理由。
///
/// `schema.sql` 里还有第五张 `b_merchant_group_taxonomy`（⑤ 的 v1 词表表），
/// 跑批一个读写点都没有，所以这里没有它。
const T_EVENT: &str = "b_merchant_group_event";
const T_GROUP: &str = "b_merchant_group_metric_daily";
const T_AGENT: &str = "b_merchant_group_agent_metric_daily";
const T_FAILURE: &str = "b_merchant_group_run_failure";

const EVENT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary, \
    event_type, taxonomy_version";
const GROUP_COLS: &str = "corpid, roomid, dt, msg_count, sender_count, event_count, \
    merchant_event_count, unreplied_count, first_reply_p50_sec, first_reply_p90_sec, \
    extraction_status";
const AGENT_COLS: &str = "corpid, room, agent, dt, event_type, taxonomy_version, event_count";
/// `run_failure` 的列。四组列名里它曾经是唯一没有常量的一组 —— INSERT 语句里写一遍、
/// [`check_schema`] 的清单里再写一遍，也没进那条占位符个数的测试。
const FAILURE_COLS: &str = "run_date, corpid, roomid, reason";

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
    let mut out: Vec<NaiveDate> = events
        .iter()
        .map(|e| e.occurred_on)
        .filter(|d| !days.days().contains(d))
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
    types: &[&str],
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

    let ds = days.days();
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
            let sql = format!(
                "DELETE FROM {T_EVENT} WHERE corpid = ? AND roomid = ? \
                 AND occurred_on IN ({})",
                holes(ds.len())
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(corp).bind(room);
            for d in ds {
                q = q.bind(*d);
            }
            q.execute(&mut *tx).await?;

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
                        .bind(*t)
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
        let sql = format!(
            "DELETE FROM {T_AGENT} WHERE corpid = ? AND room = ? AND dt IN ({})",
            holes(ds.len())
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(corp).bind(room);
        for d in ds {
            q = q.bind(*d);
        }
        q.execute(&mut *tx).await?;

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

/// 启动期自检：跑批会写的**四张**表在不在，列对不对。
///
/// `schema.sql` 里有五张 —— 第五张 `b_merchant_group_taxonomy` 是 ⑤ 的 v1 词表表，
/// 今天没有任何读写点（`classify` 只有两个常量），所以不查：查一张跑批用不到的表，
/// 只会让「还没上词表」的正常状态变成启动失败。
///
/// 存在的理由是一条真实事故：Python 版改了 `schema.sql` 但 dev 库没迁移，
/// **抽取跑完 23 分钟才在落库那步炸掉**（`1054 Unknown column`）。跑批是无人值守的，
/// 让 DDL 漂移在第一秒暴露，而不是在烧完一轮 token 之后。
pub async fn check_schema(pool: &MySqlPool) -> Result<(), BoxError> {
    for (table, cols) in [
        (T_EVENT, EVENT_COLS),
        (T_GROUP, GROUP_COLS),
        (T_AGENT, AGENT_COLS),
        (T_FAILURE, FAILURE_COLS),
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
        assert_eq!(EVENT_COLS.split(',').count(), 14);
        assert_eq!(GROUP_COLS.split(',').count(), 11);
        assert_eq!(AGENT_COLS.split(',').count(), 7);
        assert_eq!(FAILURE_COLS.split(',').count(), 4);
        assert_eq!(values("a, b, c"), "(?, ?, ?)");
        assert_eq!(holes(3), "?, ?, ?");
    }
}
