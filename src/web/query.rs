//! 只读取数 —— 一致快照、meta、日期区间与 event 的 SELECT。
//!
//! **全部是 `SELECT`，且事务显式声明 `READ ONLY`**：webUI 是只读旁路，
//! 写库 SQL 一条都不许出现在这个文件里（`store/` 才是 MySQL 的唯一写入方）。

use super::budget::{WebError, too_large};
use crate::{
    classify::{self, CURRENT_VERSION, TaxonomyType},
    config::WebLimits,
    store,
};
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Connection, MySql, MySqlConnection, Transaction};

/// 显式钉住隔离级别，部署机改变默认值也不影响跨表快照；数据库拒绝事务内写入。
pub(super) async fn snapshot(
    connection: &mut MySqlConnection,
) -> Result<Transaction<'_, MySql>, sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *connection)
        .await?;
    connection
        .begin_with("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
        .await
}

#[derive(Serialize)]
pub(super) struct Meta {
    corpid: String,
    pub(super) days: Vec<String>,
    rooms: Vec<Value>,
    agents: Vec<Value>,
    taxonomy: Vec<TaxonomyType>,
    pub(super) taxonomy_version: &'static str,
    alias_is_authoritative: bool,
}

pub(super) async fn read_meta(
    connection: &mut MySqlConnection,
    corp: &str,
    limits: &WebLimits,
) -> Result<Meta, WebError> {
    let (since, until): (Option<NaiveDate>, Option<NaiveDate>) = sqlx::query_as(
        "SELECT MIN(since), MAX(until) FROM (SELECT MIN(dt) since, MAX(dt) until FROM b_merchant_group_metric_daily WHERE corpid = ? \
         UNION ALL SELECT MIN(occurred_on), MAX(occurred_on) FROM b_merchant_group_event WHERE corpid = ?) dates"
    ).bind(corp).bind(corp).fetch_one(&mut *connection).await?;
    let (Some(since), Some(until)) = (since, until) else {
        return Err(WebError(
            StatusCode::CONFLICT,
            "该企业尚无已落库的群日或事件".into(),
        ));
    };
    // 历史群仍读取已删除配置；商家 ID 转字符串，避免前端丢失 BIGINT 精度。
    let rooms: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT r.roomid, NULLIF(g.group_name, ''), CAST(g.merchant_id AS CHAR) FROM (\
         SELECT roomid FROM b_merchant_group_metric_daily WHERE corpid = ? \
         UNION SELECT roomid FROM b_merchant_group_event WHERE corpid = ? \
         UNION SELECT roomid FROM b_merchant_group_run_failure WHERE corpid = ?) r \
         LEFT JOIN b_wecom_merchant_group g ON g.official_room_id = r.roomid AND g.corp_id = ? \
         ORDER BY r.roomid LIMIT ?",
    )
    .bind(corp)
    .bind(corp)
    .bind(corp)
    .bind(corp)
    .bind(limits.max_rows as u64 + 1)
    .fetch_all(&mut *connection)
    .await?;
    if rooms.len() > limits.max_rows || (until - since).num_days() as usize >= limits.max_rows {
        return Err(too_large());
    }
    let agents: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT a.agent FROM b_merchant_group_event e \
         JOIN JSON_TABLE(e.agents, '$[*]' COLUMNS(agent VARCHAR(16) PATH '$')) a \
         WHERE e.corpid = ? ORDER BY a.agent LIMIT ?",
    )
    .bind(corp)
    .bind(limits.max_rows as u64 + 1)
    .fetch_all(&mut *connection)
    .await?;
    if agents.len() > limits.max_rows {
        return Err(too_large());
    }
    let taxonomy = store::read_taxonomy(&mut *connection, CURRENT_VERSION).await?;
    if CURRENT_VERSION != "v0" {
        classify::check_types(&taxonomy)?;
    }
    Ok(Meta {
        corpid: corp.into(),
        days: since
            .iter_days()
            .take_while(|day| *day <= until)
            .map(|d| d.to_string())
            .collect(),
        rooms: rooms
            .into_iter()
            .map(|(roomid, alias, merchant_id)| {
                json!({"roomid": roomid, "alias_is_authoritative": alias.is_some(),
                       "alias": alias, "merchant_id": merchant_id})
            })
            .collect(),
        agents: agents
            .into_iter()
            .map(|(agent,)| json!({"agent": agent, "alias": null}))
            .collect(),
        taxonomy,
        taxonomy_version: CURRENT_VERSION,
        alias_is_authoritative: false,
    })
}

#[derive(Default, Deserialize)]
pub(super) struct Period {
    from: Option<String>,
    to: Option<String>,
}

impl Period {
    pub(super) fn bounds(&self, meta: &Meta) -> Result<(NaiveDate, NaiveDate), WebError> {
        let parse = |value: &str| {
            if value.len() != 10 {
                return Err(WebError(
                    StatusCode::BAD_REQUEST,
                    "日期须为有效的 YYYY-MM-DD".into(),
                ));
            }
            value
                .parse::<NaiveDate>()
                .map_err(|_| WebError(StatusCode::BAD_REQUEST, "日期须为有效的 YYYY-MM-DD".into()))
        };
        let until = parse(self.to.as_deref().unwrap_or(meta.days.last().unwrap()))?;
        let since = match self.from.as_deref() {
            Some(value) => parse(value)?,
            None => (until - chrono::Duration::days(6)).max(parse(&meta.days[0])?),
        };
        if since > until {
            return Err(WebError(StatusCode::BAD_REQUEST, "日期范围倒挂".into()));
        }
        Ok((since, until))
    }
}

// 批次已更新但本群尚未完成时，事实照常可见，标签等到分类指标发布后一起展示。
pub(super) const EVENT_SELECT: &str = "SELECT CAST(JSON_OBJECT('id', e.id, 'corpid', e.corpid, 'roomid', e.roomid, \
         'source_msg_ids', e.source_msg_ids, 'first_msg_time', DATE_FORMAT(e.first_msg_time, '%Y-%m-%d %H:%i:%s'), \
         'last_msg_time', DATE_FORMAT(e.last_msg_time, '%Y-%m-%d %H:%i:%s'), \
         'first_agent_reply_time', DATE_FORMAT(e.first_agent_reply_time, '%Y-%m-%d %H:%i:%s'), \
         'occurred_on', DATE_FORMAT(e.occurred_on, '%Y-%m-%d'), 'asker', e.asker, 'asker_role', e.asker_role, \
         'agents', e.agents, 'first_responder', e.first_responder, 'summary', e.summary, \
         'event_type', IF(g.classification_status IN ('pending','failed'), NULL, e.event_type), \
         'event_types', IF(g.classification_status IN ('pending','failed'), NULL, e.event_types), \
         'taxonomy_version', IF(g.classification_status IN ('pending','failed'), NULL, e.taxonomy_version)) AS CHAR) AS document \
         FROM b_merchant_group_event e LEFT JOIN b_merchant_group_metric_daily g \
         ON g.corpid = e.corpid AND g.roomid = e.roomid AND g.dt = e.occurred_on";
