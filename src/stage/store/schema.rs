//! 启动期自检 —— DDL 漂移在第一秒暴露，不在烧完一轮 token 之后。

use super::sql::{
    AGENT_COLS, AGENT_MSG_COLS, BATCH, EVENT_COLS, FAILURE_COLS, GROUP_COLS, REWRITE_COLS, T_AGENT,
    T_AGENT_MSG, T_EVENT, T_FAILURE, T_GROUP, T_REWRITE, T_TAXONOMY, TAXONOMY_COLS,
};
use crate::BoxError;
use sqlx::{MySqlPool, Row};

/// 启动期自检：跑批读写的**六张**表在不在，列对不对。
///
/// `b_merchant_group_taxonomy` 从 ⑤ 的 v1 起进来 —— 每轮开跑都要读它取词表。
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
        (T_AGENT_MSG, AGENT_MSG_COLS),
        (T_FAILURE, FAILURE_COLS),
        (T_TAXONOMY, TAXONOMY_COLS),
        // 冻结区重写的账。**查表不查行** —— 从没人工补跑过的库里它一行都没有，
        // 那是正常状态。进这份清单是因为漏建它会让 backfill / retry 在写账那一刻
        // 才报错，而那时事实列已经开始删重写了。
        (T_REWRITE, REWRITE_COLS),
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
            return Err(format!(
                "表 {table} 缺列 {missing:?} —— schema.sql 漂移了，\
                 去 docs/deploy.md 找带这几列的升级章节，照抄 ALTER 执行"
            )
            .into());
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
                if ["event_type", "taxonomy_version"].contains(&name.as_str())
                    && row.get::<String, _>(1) != "YES"
                {
                    return Err(format!(
                        "{table}.{name} 必须允许 NULL，先执行独立打标的表结构升级"
                    )
                    .into());
                }
                // 加这一列之前抽取的历史行没有值，而冻结区不可回填 —— 建成
                // `NOT NULL DEFAULT 'EXTERNAL'` 会给它们编造一个角色，那是承重不变量 4
                // 说的「用某一边表示没算出来」。ALTER 抄错了要在第一秒拦住。
                if ["last_msg_role", "followup_wait_max_sec"].contains(&name.as_str())
                    && row.get::<String, _>(1) != "YES"
                {
                    return Err(format!(
                        "{table}.{name} 必须允许 NULL —— 历史行没有值，不能默认成某一边或 0"
                    )
                    .into());
                }
            }
        }
    }
    packet_limit(pool).await?;
    Ok(())
}

/// `max_allowed_packet` 的下限 —— **护栏，不是容量规划**。
///
/// 事件 `INSERT` 一次带 [`BATCH`] = 500 行，其中 `source_messages` 是装全文的
/// `MEDIUMTEXT`，单条语句轻易到数 MB。撞上这个限额的失败发生在**模型 token 已经烧完
/// 之后**，而且重试一次仍以同样方式失败 —— 典型的「该在第一秒暴露」。
///
/// ⚠️ **16 MiB 是一个没有实测支撑的保守数**，不是量出来的：MySQL 8 的默认值是 64 MiB，
/// 这里打了四折还留着余量。真有了一次单语句字节数的实测，再决定是抬这个数、
/// 还是把 [`BATCH`] 改成按字节切块。别把它当成「测出来 16 MiB 就够」。
const MIN_PACKET_BYTES: u64 = 16 * 1024 * 1024;

async fn packet_limit(pool: &MySqlPool) -> Result<(), BoxError> {
    let (packet,): (u64,) = sqlx::query_as("SELECT @@max_allowed_packet")
        .fetch_one(pool)
        .await?;
    if packet < MIN_PACKET_BYTES {
        return Err(format!(
            "max_allowed_packet = {packet} 字节，低于 {MIN_PACKET_BYTES} —— \
             事件 INSERT 一次带 {BATCH} 行、其中 source_messages 装的是全文，\
             单条语句会撞上它，而那时这一轮的模型 token 已经烧完了。\
             改数据库配置，或降低 store::sql::BATCH"
        )
        .into());
    }
    Ok(())
}

/// 跑批收尾刷新索引统计。**不是优化，是防"某天突然变慢"。**
///
/// 分片删重写每天把 `T-3` / `T-2` 的全部事件删掉再写一遍。InnoDB 的索引统计是
/// **采样**得来的，天天大改会让「这个条件大概命中多少行」失准 —— 而优化器就靠这个
/// 数在 `idx_shard` 和 `idx_overview` 之间选。选错一次，只读工作台的概览查询慢十倍，
/// 而且没有任何报错，只是变慢了。
///
/// 放在跑批收尾是因为**位置天然合适**：写刚刚结束、下一次读还没开始，而且跑批本来
/// 每天就跑完一次。`ANALYZE TABLE` 只重算采样页，不重建表，代价是毫秒到秒级。
///
/// **失败不掀翻整轮** —— 统计没刷新只是查询计划可能不是最优，不是数据错。
/// 记 warn，让它在日志里看得见，不改退出码。
pub async fn refresh_statistics(pool: &MySqlPool) {
    for table in [T_EVENT, T_GROUP, T_AGENT, T_AGENT_MSG] {
        // 表名是本文件的常量，不是外部输入 —— ANALYZE TABLE 不接受 `?` 绑定表名。
        let sql = format!("ANALYZE TABLE {table}");
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
            tracing::warn!(table, "刷新索引统计失败，查询计划可能不是最优：{e}");
        }
    }
}
