use crate::{
    BoxError,
    classify::{self, CURRENT_VERSION, TaxonomyType},
    config::WebLimits,
    ingest, store,
    window::Window,
};
use axum::{
    Json, Router,
    extract::{Path as Id, Query, Request, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{NaiveDate, NaiveDateTime};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Connection, MySql, MySqlConnection, MySqlPool, Row, Transaction};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
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

async fn admit(
    State(state): State<WebState>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let Ok(_permit) = state.requests.try_acquire() else {
        return WebError(
            StatusCode::SERVICE_UNAVAILABLE,
            "查询繁忙，请稍后重试".into(),
        )
        .into_response();
    };
    // 名额覆盖取数与响应序列化；等待超时取消数据库读取，原文阻塞任务另持真实扫描名额。
    match tokio::time::timeout(
        Duration::from_secs(state.limits.query_timeout_secs),
        next.run(request),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => WebError(
            StatusCode::GATEWAY_TIMEOUT,
            "查询超时，请缩小日期范围".into(),
        )
        .into_response(),
    }
}

fn too_large() -> WebError {
    WebError(
        StatusCode::PAYLOAD_TOO_LARGE,
        "结果超出读取预算，请缩小日期范围；未返回部分统计".into(),
    )
}

/// 同时限制行数和结构化文档字节，不能读完大集合再发现超限。
struct ReadBudget {
    rows: usize,
    bytes: usize,
}

impl ReadBudget {
    fn new(limits: &WebLimits) -> Self {
        Self {
            rows: limits.max_rows,
            bytes: limits.max_response_bytes,
        }
    }

    fn document(&mut self, text: &str) -> Result<Value, WebError> {
        self.rows = self.rows.checked_sub(1).ok_or_else(too_large)?;
        self.bytes = self.bytes.checked_sub(text.len()).ok_or_else(too_large)?;
        Ok(serde_json::from_str(text)?)
    }
}

struct ResponseBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for ResponseBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("响应超过预算"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Response, WebError> {
    let mut buffer = ResponseBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value)
        .map_err(|e| if e.is_io() { too_large() } else { e.into() })?;
    Ok(([(header::CONTENT_TYPE, "application/json")], buffer.bytes).into_response())
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn row_and_byte_budgets_reject_instead_of_truncating() {
        let limits = WebLimits {
            concurrency: 1,
            scan_concurrency: 1,
            max_response_bytes: 6,
            max_rows: 1,
            query_timeout_secs: 1,
        };
        let mut budget = ReadBudget::new(&limits);
        assert_eq!(budget.document("{}").unwrap(), json!({}));
        assert_eq!(
            budget.document("{}").unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            ReadBudget::new(&limits).document("{invalid").unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        // 预算按 UTF-8 字节而不是中文字数，也包括 JSON 引号。
        assert!(bounded_json(&"中文", 8).is_ok());
        assert_eq!(
            bounded_json(&"中文", 7).unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn admission_rejects_excess_work_and_releases_timed_out_slots() {
        let state = WebState {
            pool: sqlx::mysql::MySqlPoolOptions::new()
                .connect_lazy("mysql://localhost/test")
                .unwrap(),
            raw_root: PathBuf::new(),
            corp: "C".into(),
            limits: WebLimits {
                concurrency: 1,
                scan_concurrency: 1,
                max_response_bytes: 1000,
                max_rows: 10,
                query_timeout_secs: 1,
            },
            requests: Arc::new(Semaphore::new(1)),
            scans: Arc::new(Semaphore::new(1)),
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let ready = entered.clone();
        let app = Router::new()
            .route(
                "/slow",
                get(move || {
                    let ready = ready.clone();
                    async move {
                        ready.notify_one();
                        std::future::pending::<&'static str>().await
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(state.clone(), admit));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/slow", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::new();
        let first = tokio::spawn(http.get(&url).send());
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            http.get(&url).send().await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            first.await.unwrap().unwrap().status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(state.requests.available_permits(), 1);
        let _held_scan = state.scans.acquire().await.unwrap();
        assert_eq!(
            messages(State(state.clone()), Id(1)).await.unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        server.abort();
        let _ = server.await;
    }
}

#[derive(Debug)]
pub(super) struct WebError(StatusCode, String);

impl<E: Into<BoxError>> From<E> for WebError {
    fn from(error: E) -> Self {
        let error = error.into();
        tracing::error!("只读取数失败：{error}");
        if let Some(sqlx::Error::Database(db)) = error.downcast_ref::<sqlx::Error>()
            && db.code().is_some_and(|code| code == "3024")
        {
            return Self(
                StatusCode::GATEWAY_TIMEOUT,
                "数据库查询超时，请缩小日期范围".into(),
            );
        }
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "读取失败，请查看后端日志".into(),
        )
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

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
struct Meta {
    corpid: String,
    days: Vec<String>,
    rooms: Vec<Value>,
    agents: Vec<Value>,
    taxonomy: Vec<TaxonomyType>,
    taxonomy_version: &'static str,
    alias_is_authoritative: bool,
}

async fn read_meta(
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

async fn meta(State(state): State<WebState>) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    let meta = read_meta(&mut tx, &state.corp, &state.limits).await?;
    tx.commit().await?;
    bounded_json(&meta, state.limits.max_response_bytes)
}

#[derive(Default, Deserialize)]
struct Period {
    from: Option<String>,
    to: Option<String>,
}

impl Period {
    fn bounds(&self, meta: &Meta) -> Result<(NaiveDate, NaiveDate), WebError> {
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
const EVENT_SELECT: &str = "SELECT CAST(JSON_OBJECT('id', e.id, 'corpid', e.corpid, 'roomid', e.roomid, \
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

async fn messages(State(state): State<WebState>, Id(id): Id<u64>) -> Result<Response, WebError> {
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
