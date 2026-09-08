//! 启动期自检 —— DDL 漂移在第一秒暴露，不在烧完一轮 token 之后。

use super::sql::{
    AGENT_COLS, EVENT_COLS, FAILURE_COLS, GROUP_COLS, T_AGENT, T_EVENT, T_FAILURE, T_GROUP,
    T_TAXONOMY, TAXONOMY_COLS,
};
use crate::BoxError;
use sqlx::{MySqlPool, Row};

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
