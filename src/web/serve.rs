//! HTTP 入口 —— 应用状态、路由与四个 handler。
//!
//! 全部 `GET`、无登录版（内网可达即可看）。限额与错误映射在 `budget`，
//! 取数与 SQL 在 `query`；这里只负责把两边接起来。

use super::{
    budget::{ReadBudget, WebError, admit, bounded_json, too_large},
    query::{EVENT_SELECT, Period, read_meta, snapshot},
};
use crate::{classify::CURRENT_VERSION, config::WebLimits, ingest, store, window::Window};
use axum::{
    Router,
    extract::{Path as Id, Query, State},
    http::{StatusCode, header},
    middleware,
    response::Response,
    routing::get,
};
use chrono::{NaiveDate, NaiveDateTime};
use futures_util::TryStreamExt;
use serde_json::{Value, json};
use sqlx::{MySqlPool, Row};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub(super) struct WebState {
    pub pool: MySqlPool,
    pub raw_root: PathBuf,
    pub corp: String,
    pub limits: WebLimits,
    pub requests: Arc<Semaphore>,
    pub scans: Arc<Semaphore>,
}

pub async fn serve(
    pool: MySqlPool,
    raw_root: PathBuf,
    corp: String,
    address: SocketAddr,
    limits: WebLimits,
) -> crate::Result<()> {
    assert!(!corp.is_empty(), "corpid 不可为空");
    store::check_schema(&pool).await?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(address = %listener.local_addr()?, corp, "只读工作台启动");
    axum::serve(
        listener,
        router(WebState {
            pool,
            raw_root,
            corp,
            requests: Arc::new(Semaphore::new(limits.concurrency)),
            scans: Arc::new(Semaphore::new(limits.scan_concurrency)),
            limits,
        }),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

pub(super) fn router(state: WebState) -> Router {
    Router::new()
        .route("/api/meta", get(meta))
        .route("/api/dataset", get(dataset))
        .route("/api/event/{id}", get(event))
        .route("/api/event/{id}/messages", get(messages))
        .layer(middleware::from_fn_with_state(state.clone(), admit))
        .layer(middleware::map_response(
            |mut response: Response| async move {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                response
            },
        ))
        .with_state(state)
}

async fn meta(State(state): State<WebState>) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    let meta = read_meta(&mut tx, &state.corp, &state.limits).await?;
    tx.commit().await?;
    bounded_json(&meta, state.limits.max_response_bytes)
}

async fn dataset(
    State(state): State<WebState>,
    Query(period): Query<Period>,
) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    let meta = read_meta(&mut tx, &state.corp, &state.limits).await?;
    let mut budget = ReadBudget::new(&state.limits);
    let (since, until) = period.bounds(&meta)?;
    // JSON 由 MySQL 结构化构造；日期显式格式化，保留 DATETIME 的 UTC+8 墙钟含义。
    let sql = format!(
        "{EVENT_SELECT} WHERE e.corpid = ? AND e.occurred_on BETWEEN ? AND ? ORDER BY e.occurred_on, e.id"
    );
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&state.corp)
        .bind(since)
        .bind(until)
        .fetch(&mut *tx);
    let mut events = Vec::new();
    while let Some(row) = rows.try_next().await? {
        events.push(budget.document(&row.try_get::<String, _>("document")?)?);
    }
    drop(rows);
    if events
        .iter()
        .any(|e| !e["taxonomy_version"].is_null() && e["taxonomy_version"] != meta.taxonomy_version)
    {
        return Err(WebError(
            StatusCode::CONFLICT,
            "事件与当前词表版本不一致，请完成重打标".into(),
        ));
    }
    let mut rows = sqlx::query(
        "SELECT CAST(JSON_OBJECT('corpid', g.corpid, 'roomid', g.roomid, 'dt', DATE_FORMAT(g.dt, '%Y-%m-%d'), \
         'msg_count', g.msg_count, 'sender_count', g.sender_count, 'event_count', g.event_count, \
         'merchant_event_count', g.merchant_event_count, 'unreplied_count', g.unreplied_count, \
         'first_reply_p50_sec', g.first_reply_p50_sec, 'first_reply_p90_sec', g.first_reply_p90_sec, \
         'extraction_status', g.extraction_status, 'classification_status', g.classification_status, \
         'freshness', IF(g.fact_completed_time IS NULL OR \
         (SELECT MAX(f.gmt_created_time) FROM b_merchant_group_run_failure f WHERE f.corpid=g.corpid AND f.roomid=g.roomid AND f.stage='extract') \
         >= g.fact_completed_time, 'unknown', 'known')) AS CHAR) AS document \
         FROM b_merchant_group_metric_daily g WHERE g.corpid = ? AND g.dt BETWEEN ? AND ? ORDER BY g.dt, g.roomid"
    ).bind(&state.corp).bind(since).bind(until).fetch(&mut *tx);
    let mut group = Vec::new();
    while let Some(row) = rows.try_next().await? {
        group.push(budget.document(&row.try_get::<String, _>("document")?)?);
    }
    drop(rows);
    tx.commit().await?;
    bounded_json(
        &json!({"meta":meta, "events":events, "groupDaily":group}),
        state.limits.max_response_bytes,
    )
}

async fn event(State(state): State<WebState>, Id(id): Id<u64>) -> Result<Response, WebError> {
    let sql = format!("{EVENT_SELECT} WHERE e.corpid = ? AND e.id = ?");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&state.corp)
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    let row =
        row.ok_or_else(|| WebError(StatusCode::NOT_FOUND, "事件不存在或已被重新抽取".into()))?;
    let event = ReadBudget::new(&state.limits).document(&row.try_get::<String, _>("document")?)?;
    if !event["taxonomy_version"].is_null() && event["taxonomy_version"] != CURRENT_VERSION {
        return Err(WebError(
            StatusCode::CONFLICT,
            "事件与当前词表版本不一致，请完成重打标".into(),
        ));
    }
    bounded_json(&event, state.limits.max_response_bytes)
}

pub(super) async fn messages(
    State(state): State<WebState>,
    Id(id): Id<u64>,
) -> Result<Response, WebError> {
    let permit = state.scans.clone().try_acquire_owned().map_err(|_| {
        WebError(
            StatusCode::SERVICE_UNAVAILABLE,
            "原文读取繁忙，请稍后重试".into(),
        )
    })?;
    let row: Option<(String, NaiveDate, NaiveDateTime, String)> = sqlx::query_as(
        "SELECT roomid, occurred_on, last_msg_time, CAST(source_msg_ids AS CHAR) FROM b_merchant_group_event WHERE corpid = ? AND id = ?"
    ).bind(&state.corp).bind(id).fetch_optional(&state.pool).await?;
    let Some((room, since, last, ids)) = row else {
        return Err(WebError(
            StatusCode::NOT_FOUND,
            "事件不存在或已被重新抽取".into(),
        ));
    };
    if ids.len() > state.limits.max_response_bytes {
        return Err(too_large());
    }
    let ids: Vec<String> = serde_json::from_str(&ids)?;
    if ids.len() > state.limits.max_rows {
        return Err(too_large());
    }
    if since > last.date() {
        return Err(WebError(StatusCode::CONFLICT, "事件时间顺序不一致".into()));
    }
    let w = Window::span(since, last.date());
    let result = tokio::task::spawn_blocking(move || {
        // HTTP 取消后扫描仍可能继续，名额跟随阻塞任务释放，防止取消请求绕过限流。
        let _permit = permit;
        ingest::read_by_ids(&state.raw_root, &state.corp, &room, &w, &ids)
    })
    .await?;
    let messages = match result {
        Ok(messages) => messages,
        Err(ingest::IngestError::Missing(_)) => {
            return Err(WebError(
                StatusCode::GONE,
                "原文不完整或已超过保留期".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    bounded_json(&Value::Array(
        messages
            .into_iter()
            .map(|m| {
                json!({
                    "msg_id":m.msg_id, "at":m.at.format("%Y-%m-%d %H:%M:%S").to_string(),
                    "sender_id":m.sender_id, "sender_role":m.sender_role.as_str(), "text":m.text,
                })
            })
            .collect(),
    ), state.limits.max_response_bytes)
}
