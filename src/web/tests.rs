use super::{budget::*, query::*, serve::*};
use crate::{config::WebLimits, testutil};
use axum::{
    Router,
    extract::{Path as Id, State},
    http::StatusCode,
    middleware,
    routing::get,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_http_dataset_and_evidence_obey_the_read_contract() {
    use serde_json::{Value, json};
    let _log = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::ERROR)
            .finish(),
    );
    let pool = testutil::mysql_pool("web").await;
    sqlx::raw_sql(
        "CREATE TABLE b_wecom_merchant_group (\
         corp_id VARCHAR(64) NOT NULL, official_room_id VARCHAR(128) NOT NULL, \
         group_name VARCHAR(255) NOT NULL DEFAULT '', merchant_id BIGINT UNSIGNED NULL, \
         is_deleted TINYINT UNSIGNED NOT NULL DEFAULT 0, UNIQUE KEY uk_corp_room (corp_id, official_room_id)\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci; \
         INSERT INTO b_wecom_merchant_group (corp_id,official_room_id,group_name,merchant_id,is_deleted) VALUES \
         ('C','R','商家服务群',18446744073709551615,1), \
         ('other','R','其他企业群',2,0), ('C','empty-name','',NULL,0), \
         ('C','unused-room','未产生记录的群',3,0); \
         INSERT INTO b_merchant_group_event \
         (corpid,roomid,source_msg_ids,first_msg_time,last_msg_time,first_agent_reply_time,occurred_on,asker,asker_role,agents,first_responder,summary,event_type,event_types,taxonomy_version) \
         VALUES ('C','R',JSON_ARRAY('m1','m2'),'2026-08-25 23:55:00','2026-08-26 00:05:00','2026-08-26 00:05:00','2026-08-25', \
         'merchant00000001','EXTERNAL',JSON_ARRAY('agent00000000001'),'agent00000000001','商家要求改期，平台已受理','reschedule',JSON_ARRAY('reschedule'),'v1'); \
         INSERT INTO b_merchant_group_metric_daily \
         (corpid,roomid,dt,msg_count,sender_count,event_count,merchant_event_count,unreplied_count,first_reply_p50_sec,first_reply_p90_sec,extraction_status,gmt_modified_time,fact_completed_time) \
         VALUES ('C','R','2026-08-25',1,1,1,1,0,600,600,'ok','2026-08-30 10:00:00','2026-08-30 10:00:00'), \
         ('C','R','2026-08-26',1,1,0,0,0,NULL,NULL,'ok','2026-08-30 10:00:00','2026-08-30 10:00:00'); \
         INSERT INTO b_merchant_group_taxonomy (version,type_id,parent_name,name,description) \
         VALUES ('v1','reschedule','订单变更','改期','修改上门服务日期');"
    ).execute(&pool).await.unwrap();
    let root = testutil::fresh_root("web", "raw");
    let at = |d, h, m| {
        chrono::NaiveDate::from_ymd_opt(2026, 8, d)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    };
    let row = |id, time, sender, role, text| {
        json!({
            "schemaVersion":1,"parserVersion":1,"corpId":"C","officialRoomId":"R",
            "sourceMessageId":id,"standardType":"TEXT","messageTime":testutil::upstream_ms(time),
            "sender":{"easyUserId":sender,"identityType":role},"analysisText":text,"content":text,
            "semanticPayload":{"replyTo":null}
        })
    };
    testutil::write_month(
        &root,
        "202608",
        "C",
        "R",
        &[
            row(
                "m1",
                at(25, 23, 55),
                "merchant00000001",
                "EXTERNAL",
                "请改期，原文保留",
            ),
            row(
                "m2",
                at(26, 0, 5),
                "agent00000000001",
                "INTERNAL",
                "稍等，已受理",
            ),
        ],
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let app = router(WebState {
        pool: pool.clone(),
        raw_root: root.clone(),
        corp: "C".into(),
        limits: toml::from_str::<crate::config::WebConfig>(include_str!("../../config.toml"))
            .unwrap()
            .web,
        requests: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
        scans: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = signal.await;
            })
            .await
            .unwrap();
    });
    let http = reqwest::Client::new();
    let response = http
        .get(format!("{base}/api/dataset?from=2026-08-25&to=2026-08-26"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let data: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(data["meta"]["days"], json!(["2026-08-25", "2026-08-26"]));
    assert_eq!(data["meta"]["agents"][0]["agent"], "agent00000000001");
    assert_eq!(
        data["meta"]["rooms"],
        json!([{
            "roomid": "R", "alias": "商家服务群", "merchant_id": "18446744073709551615",
            "alias_is_authoritative": true
        }])
    );
    assert_eq!(data["meta"]["alias_is_authoritative"], false);
    let meta: Value = http
        .get(format!("{base}/api/meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(meta, data["meta"]);
    assert_eq!(data["events"][0]["source_msg_ids"], json!(["m1", "m2"]));
    assert_eq!(data["events"][0]["first_msg_time"], "2026-08-25 23:55:00");
    assert_eq!(data["groupDaily"][0]["freshness"], "known");
    let id = data["events"][0]["id"].as_u64().unwrap();
    let detail: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/event/{id}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(detail, data["events"][0]);
    let response = http
        .get(format!("{base}/api/event/{id}/messages"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let messages: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(messages[0]["text"], "请改期，原文保留");
    assert_eq!(messages[1]["at"], "2026-08-26 00:05:00");
    // 可选导出实际 HTTP 响应，让前端 zod 对拍同一份跨语言契约。
    if let Ok(dir) = std::env::var("CHAT2EVENTS_WEB_FIXTURE_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join("dataset.json"),
            data.to_string(),
        )
        .unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join("messages.json"),
            messages.to_string(),
        )
        .unwrap();
    }
    for (path, status) in [
        ("/api/dataset?from=2026-08-27&to=2026-08-25", 400),
        ("/api/dataset?from=broken", 400),
        ("/api/event/999999/messages", 404),
        ("/api/event/999999", 404),
    ] {
        assert_eq!(
            http.get(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            status
        );
    }
    assert_eq!(
        http.post(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    let narrow: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset?from=2026-08-26&to=2026-08-26"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(narrow["events"], json!([]));
    assert_eq!(narrow["groupDaily"].as_array().unwrap().len(), 1);

    sqlx::query("INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,event_count,merchant_event_count,unreplied_count,extraction_status) VALUES ('C','R','2026-08-01',0,0,0,0,0,'ok')").execute(&pool).await.unwrap();
    let recent: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        recent["meta"]["days"][0], "2026-08-01",
        "可用历史范围保持完整"
    );
    assert_eq!(
        recent["groupDaily"].as_array().unwrap().len(),
        2,
        "默认只加载最近七天的记录"
    );
    let history: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset?from=2026-08-01&to=2026-08-26"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        history["groupDaily"].as_array().unwrap().len(),
        3,
        "用户仍可显式读取全部历史"
    );

    sqlx::query("INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason,gmt_created_time) VALUES ('2026-08-30','C','R','合成同步失败','2026-08-30 11:00:00'),('2026-08-30','C','missing-room','合成读取失败','2026-08-30 11:00:00'),('2026-08-30','C','empty-name','合成读取失败','2026-08-30 11:00:00')").execute(&pool).await.unwrap();
    let uncertain: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(uncertain["groupDaily"][0]["freshness"], "unknown");
    // 标签状态更新不能替一次失败的抽取证明事实新鲜，必须穿过真实写入与读取链路。
    let window = crate::window::Window::span(at(25, 0, 0).date(), at(26, 0, 0).date());
    let shard = crate::store::Shard::new("C", "R", &window);
    crate::store::fail_classification(&pool, at(30, 0, 0).date(), shard, "测试分类失败")
        .await
        .unwrap();
    crate::store::finish_classification(&pool, shard, &[])
        .await
        .unwrap();
    let after_retag: Value = http
        .get(format!("{base}/api/dataset"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_retag["groupDaily"][0]["freshness"], "unknown",
        "标签更新不能推进事实完成凭据"
    );
    assert_eq!(
        uncertain["groupDaily"][0]["event_count"], 1,
        "保留旧值，但必须标记未知"
    );
    for roomid in ["missing-room", "empty-name"] {
        let room = uncertain["meta"]["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["roomid"] == roomid)
            .unwrap();
        assert_eq!(
            room,
            &json!({"roomid": roomid, "alias": null,
            "merchant_id": null, "alias_is_authoritative": false})
        );
    }
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(
        http.get(format!("{base}/api/event/{id}/messages"))
            .send()
            .await
            .unwrap()
            .status(),
        410
    );
    for status in ["pending", "failed", "ok"] {
        sqlx::query(
            "UPDATE b_merchant_group_metric_daily SET classification_status=? WHERE roomid='R'",
        )
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
        let response = http
            .get(format!("{base}/api/dataset?from=2026-08-25&to=2026-08-26"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "打标未完成也能读取事实");
        let data: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(data["events"].as_array().unwrap().len(), 1);
        assert_eq!(data["groupDaily"][0]["classification_status"], status);
        assert_eq!(data["events"][0]["event_type"].is_null(), status != "ok");
        let response = http
            .get(format!("{base}/api/event/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let detail: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(detail["taxonomy_version"].is_null(), status != "ok");
    }
    sqlx::query("UPDATE b_merchant_group_event SET taxonomy_version='v0'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        http.get(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let (_, facts) = crate::store::read_events(&pool, shard).await.unwrap();
    let counts =
        std::collections::BTreeMap::from([(window.since(), (1, 1)), (window.until(), (1, 1))]);
    let group = crate::metrics::group_rows(
        "C",
        "R",
        &window,
        &counts,
        Some(&facts),
        crate::metrics::Status::Ok,
    );
    crate::store::write_room(
        &pool,
        at(30, 0, 0).date(),
        shard,
        Some(&facts),
        None,
        &group,
        &std::collections::BTreeMap::new(),
    )
    .await
    .unwrap();
    let refreshed: Value = http
        .get(format!("{base}/api/dataset"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        refreshed["groupDaily"][0]["freshness"], "known",
        "只有真实保存事实才能恢复新鲜度"
    );
    assert!(
        refreshed["events"][0]["event_type"].is_null(),
        "事实新鲜不意味着分类已完成"
    );
    shutdown.send(()).unwrap();
    server.await.unwrap();
    testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_read_snapshot_is_read_only_and_stable_across_tables() {
    let pool = testutil::mysql_pool("snapshot").await;
    let mut connection = pool.acquire().await.unwrap();
    let mut tx = snapshot(&mut connection).await.unwrap();
    let (before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_run_failure")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO b_merchant_group_run_failure (run_date, corpid, roomid, reason) VALUES ('2026-08-30','C','R','test')").execute(&pool).await.unwrap();
    let (after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_run_failure")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(before, after, "同一快照不应读到后续提交");
    assert!(
        sqlx::query("DELETE FROM b_merchant_group_run_failure")
            .execute(&mut *tx)
            .await
            .is_err(),
        "只读事务必须由数据库阻止写入"
    );
    tx.commit().await.unwrap();
    drop(connection);
    testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_filtered_reads_use_date_and_failure_indexes() {
    use sqlx::Row;
    let pool = testutil::mysql_pool("read_indexes").await;
    sqlx::raw_sql("INSERT INTO b_merchant_group_event (corpid,roomid,source_msg_ids,first_msg_time,last_msg_time,occurred_on,asker,asker_role,agents,summary) \
        WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM seq WHERE n<999) \
        SELECT CONCAT('C',MOD(n,10)),CONCAT('R',MOD(n,100)),JSON_ARRAY(CONCAT('m',n)),'2026-01-01','2026-01-01', \
        DATE_ADD('2026-01-01',INTERVAL (n DIV 10) DAY),'merchant00000001','EXTERNAL',JSON_ARRAY('agent00000000001'),'合成事件' FROM seq; \
        INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,event_count,extraction_status,fact_completed_time) \
        SELECT corpid,roomid,occurred_on,1,1,1,'ok','2026-08-30' FROM b_merchant_group_event; \
        INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason,stage) SELECT '2026-08-31',corpid,roomid,'合成失败','extract' FROM b_merchant_group_event; \
        ANALYZE TABLE b_merchant_group_event, b_merchant_group_metric_daily, b_merchant_group_run_failure;")
        .execute(&pool).await.unwrap();
    for (sql, expected) in [
        (
            "SELECT summary FROM b_merchant_group_event WHERE corpid='C1' AND occurred_on='2026-01-05'",
            "idx_corp_day",
        ),
        (
            "SELECT msg_count FROM b_merchant_group_metric_daily WHERE corpid='C1' AND dt='2026-01-05'",
            "idx_corp_day",
        ),
        (
            "SELECT gmt_created_time FROM b_merchant_group_run_failure WHERE corpid='C1' AND roomid='R1' AND stage='extract' ORDER BY gmt_created_time DESC LIMIT 1",
            "idx_room_stage_time",
        ),
    ] {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
            .fetch_one(&pool)
            .await
            .unwrap();
        let key: Option<String> = row.try_get("key").unwrap();
        assert_eq!(key.as_deref(), Some(expected));
        let rows: u64 = row.try_get("rows").unwrap();
        assert!(rows <= 10, "索引不应扫描整份1000行历史：{rows}");
        eprintln!("{expected}: estimated_rows={rows}");
    }
    for (name, query) in [
        (
            "old_meta_dates",
            "SELECT MIN(dt), MAX(dt) FROM (SELECT dt FROM b_merchant_group_metric_daily WHERE corpid='C1' UNION ALL SELECT occurred_on FROM b_merchant_group_event WHERE corpid='C1') dates",
        ),
        (
            "new_meta_dates",
            "SELECT MIN(since), MAX(until) FROM (SELECT MIN(dt) since,MAX(dt) until FROM b_merchant_group_metric_daily WHERE corpid='C1' UNION ALL SELECT MIN(occurred_on),MAX(occurred_on) FROM b_merchant_group_event WHERE corpid='C1') dates",
        ),
    ] {
        let plan: String =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN ANALYZE {query}")))
                .fetch_one(&pool)
                .await
                .unwrap();
        eprintln!("{name}: {plan}");
    }
    testutil::drop_mysql_database(pool).await;
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
