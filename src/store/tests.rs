//! `store` 的**跨文件**测试与共享 fixture —— 三条 `mysql_` 用例在隔离 MySQL 上
//! 验证事务边界、冻结与打标状态，CI 显式执行（见 `docs/deploy.md`）。
//!
//! 单个文件的单元测试住在各自文件底部：`facts` 的冻结守卫、`sql` 的列数与占位符。

use super::{labels::*, read::*, schema::*, sql::*};
use crate::{
    BoxError,
    classify::Labels,
    extract::Event,
    ingest::Role,
    metrics::{AgentRow, GroupRow},
    window::Window,
};
use chrono::{NaiveDate, NaiveDateTime};
use sqlx::MySqlPool;
use std::collections::BTreeMap;

/// 为既有事务、冻结和重打标用例准备已完成两阶段处理的记录。
#[allow(clippy::too_many_arguments)]
async fn write_room(
    pool: &MySqlPool,
    run_date: NaiveDate,
    corp: &str,
    room: &str,
    days: &Window,
    events: Option<&[Event]>,
    labels: &[Labels],
    version: &str,
    reason: Option<&str>,
    group: &[GroupRow],
    agent: &[AgentRow],
) -> Result<(), BoxError> {
    let shard = Shard::new(corp, room, days);
    super::write_room(
        pool,
        run_date,
        shard,
        events,
        reason,
        group,
        &BTreeMap::new(),
    )
    .await?;
    if let Some(events) = events {
        let (ids, saved) = read_events(pool, shard).await?;
        let aligned: Vec<_> = saved
            .iter()
            .map(|event| {
                labels[events
                    .iter()
                    .position(|source| {
                        source.summary == event.summary
                            && source.first_msg_time == event.first_msg_time
                    })
                    .unwrap()]
                .clone()
            })
            .collect();
        update_event_labels(pool, shard, &ids, &aligned, version).await?;
        finish_classification(pool, shard, agent).await?;
    }
    Ok(())
}

pub(super) fn d(day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 8, day).unwrap()
}

pub(super) fn ev(day: u32) -> Event {
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

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_pipeline_upgrade_matches_the_documented_schema() {
    let pool = crate::testutil::mysql_pool("pipeline_upgrade").await;
    sqlx::raw_sql(
        "ALTER TABLE b_merchant_group_event MODIFY event_type VARCHAR(64) NOT NULL, \
         MODIFY event_types JSON NOT NULL, MODIFY taxonomy_version VARCHAR(16) NOT NULL; \
         ALTER TABLE b_merchant_group_metric_daily DROP COLUMN classification_status, DROP COLUMN agent_accounts; \
         ALTER TABLE b_merchant_group_run_failure DROP COLUMN stage; \
         INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason) VALUES ('2026-08-28','C','R','历史失败');"
    ).execute(&pool).await.unwrap();
    assert!(check_schema(&pool).await.is_err());
    let section = include_str!("../../docs/deploy.md")
        .split_once("### 独立打标流水线升级")
        .unwrap()
        .1;
    let sql = section
        .split_once("```sql\n")
        .unwrap()
        .1
        .split_once("```")
        .unwrap()
        .0;
    sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    check_schema(&pool).await.unwrap();
    let (stage,): (String,) = sqlx::query_as("SELECT stage FROM b_merchant_group_run_failure")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stage, "extract", "历史失败保留原先的事实完整性语义");
    crate::testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_freshness_upgrade_keeps_legacy_evidence_unknown() {
    let pool = crate::testutil::mysql_pool("freshness_upgrade").await;
    sqlx::raw_sql("ALTER TABLE b_merchant_group_metric_daily DROP COLUMN fact_completed_time, DROP INDEX idx_corp_day; \
        ALTER TABLE b_merchant_group_event DROP INDEX idx_corp_day; \
        ALTER TABLE b_merchant_group_run_failure DROP INDEX idx_room_stage_time, MODIFY gmt_created_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, \
        MODIFY gmt_modified_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP; \
        INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,extraction_status) VALUES ('C','R','2026-08-25',0,0,'ok');")
        .execute(&pool).await.unwrap();
    assert!(check_schema(&pool).await.is_err());
    let sql = include_str!("../../docs/deploy.md")
        .split_once("### 事实新鲜度与查询预算升级")
        .unwrap()
        .1
        .split_once("```sql\n")
        .unwrap()
        .1
        .split_once("```")
        .unwrap()
        .0;
    sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    check_schema(&pool).await.unwrap();
    let evidence: Option<NaiveDateTime> =
        sqlx::query_scalar("SELECT fact_completed_time FROM b_merchant_group_metric_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(evidence.is_none(), "不能用通用更新时间伪造历史事实凭据");
    let precision: u32 = sqlx::query_scalar("SELECT datetime_precision FROM information_schema.columns WHERE table_schema=DATABASE() AND table_name='b_merchant_group_run_failure' AND column_name='gmt_created_time'").fetch_one(&pool).await.unwrap();
    assert_eq!(precision, 6);
    crate::testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_room_writes_preserve_failure_atomicity_and_frozen_facts() {
    use crate::{
        classify::Classifier,
        config::Config,
        llm::Llm,
        metrics::{self, Attribution, Status},
        testutil,
    };
    use std::collections::BTreeMap;
    let pool = testutil::mysql_pool("store").await;
    check_schema(&pool).await.unwrap();
    let cfg: Config = toml::from_str(include_str!("../../config.toml")).unwrap();
    let classifier = Classifier::new(
        "v0",
        vec![],
        Llm::new(&cfg.llm, &cfg.llm.classify, "unused-test-key".into()).unwrap(),
        &testutil::fresh_root("store", "cache"),
    )
    .unwrap();
    let labels = classifier.classify(&["旧事实", "新事实"]).await.unwrap();
    let w = Window::span(d(25), d(26));
    let frozen = ev(24);
    write_room(
        &pool,
        d(27),
        "C",
        "R",
        &Window::span(d(24), d(24)),
        Some(std::slice::from_ref(&frozen)),
        &labels[..1],
        "v0",
        None,
        &[],
        &[],
    )
    .await
    .unwrap();
    let mut events = vec![ev(25), ev(26)];
    for (i, e) in events.iter_mut().enumerate() {
        e.source_msg_ids = vec![format!("message-{i}"), format!("reply-{i}")];
        e.asker = "merchant-0000001".into();
        e.agents = vec!["agent-0000000001".into(), "agent-0000000002".into()];
        e.first_responder = Some(e.agents[0].clone());
        e.last_msg_time = e.first_msg_time + chrono::Duration::minutes(5);
        e.first_agent_reply_time = Some(e.last_msg_time);
    }
    let counts = BTreeMap::from([(d(25), (2, 2)), (d(26), (2, 2))]);
    let group = metrics::group_rows("C", "R", &w, &counts, Some(&events), Status::Ok);
    let mut agent = metrics::agent_rows(
        "C",
        "R",
        &events,
        &["__untyped__"; 2],
        "v0",
        Attribution::default(),
    );
    agent[0].official_user_id = Some("13523611718".into());
    write_room(
        &pool,
        d(27),
        "C",
        "R",
        &w,
        Some(&events),
        &labels,
        "v0",
        None,
        &group,
        &agent,
    )
    .await
    .unwrap();
    let initial = read_events(&pool, Shard::new("C", "R", &w)).await.unwrap();
    assert_eq!(initial.1, events, "所有事实列读写往返必须一致");
    let accounts: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT agent, official_user_id FROM b_merchant_group_agent_metric_daily ORDER BY dt",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        accounts,
        vec![
            (agent[0].agent.clone(), Some("13523611718".into())),
            (agent[1].agent.clone(), None),
        ]
    );

    let failed = metrics::group_rows("C", "R", &w, &counts, None, Status::Failed);
    write_room(
        &pool,
        d(27),
        "C",
        "R",
        &w,
        None,
        &[],
        "v0",
        Some("合成抽取失败"),
        &failed,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        read_events(&pool, Shard::new("C", "R", &w)).await.unwrap(),
        initial
    );
    let (agent_rows,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_agent_metric_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(agent_rows, 2, "失败保留旧客服事实，由群日状态揭示残缺");
    let status: Vec<(String, Option<u32>)> = sqlx::query_as(
        "SELECT extraction_status, event_count FROM b_merchant_group_metric_daily ORDER BY dt",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(status, vec![("failed".into(), None); 2]);
    let (failures,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_run_failure")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(failures, 1);

    let empty = metrics::group_rows("C", "R", &w, &counts, Some(&[]), Status::Ok);
    write_room(
        &pool,
        d(27),
        "C",
        "R",
        &w,
        Some(&[]),
        &[],
        "v0",
        None,
        &empty,
        &[],
    )
    .await
    .unwrap();
    assert!(
        read_events(&pool, Shard::new("C", "R", &w))
            .await
            .unwrap()
            .1
            .is_empty()
    );
    let (remaining,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_agent_metric_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
    let status: Vec<(String, Option<u32>)> = sqlx::query_as(
        "SELECT extraction_status, event_count FROM b_merchant_group_metric_daily ORDER BY dt",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(status, vec![("ok".into(), Some(0)); 2]);

    write_room(
        &pool,
        d(27),
        "C",
        "R",
        &w,
        Some(&events),
        &labels,
        "v0",
        None,
        &group,
        &agent,
    )
    .await
    .unwrap();
    let before = read_events(&pool, Shard::new("C", "R", &w)).await.unwrap();
    let mut broken = group.clone();
    broken[1].room = "R".repeat(65);
    assert!(
        write_room(
            &pool,
            d(27),
            "C",
            "R",
            &w,
            Some(&events[..1]),
            &labels[..1],
            "v0",
            None,
            &broken,
            &agent[..1]
        )
        .await
        .is_err()
    );
    assert_eq!(
        read_events(&pool, Shard::new("C", "R", &w)).await.unwrap(),
        before,
        "第二日写入失败应回滚全部事实和指标"
    );
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_agent_metric_daily")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);

    // 同一来源事件移到另一天，两个分片必须整体替换。
    let mut moved = events[0].clone();
    moved.occurred_on = d(26);
    moved.first_msg_time += chrono::Duration::days(1);
    moved.last_msg_time += chrono::Duration::days(1);
    moved.first_agent_reply_time = Some(moved.last_msg_time);
    let moved_group = metrics::group_rows(
        "C",
        "R",
        &w,
        &counts,
        Some(std::slice::from_ref(&moved)),
        Status::Ok,
    );
    let mut moved_agent = metrics::agent_rows(
        "C",
        "R",
        std::slice::from_ref(&moved),
        &["__untyped__"],
        "v0",
        Attribution::default(),
    );
    moved_agent[0].official_user_id = Some("staff.a".into());
    write_room(
        &pool,
        d(27),
        "C",
        "R",
        &w,
        Some(std::slice::from_ref(&moved)),
        &labels[..1],
        "v0",
        None,
        &moved_group,
        &moved_agent,
    )
    .await
    .unwrap();
    let before_retag = read_events(&pool, Shard::new("C", "R", &w)).await.unwrap();
    assert_eq!(before_retag.1, vec![moved.clone()]);
    let new_agent = metrics::agent_rows(
        "C",
        "R",
        &[moved],
        &["__untyped__"],
        "v1",
        Attribution::default(),
    );
    retag_room(
        &pool,
        Shard::new("C", "R", &w),
        &before_retag.0,
        &labels[..1],
        "v1",
        &new_agent,
    )
    .await
    .unwrap();
    assert_eq!(
        read_events(&pool, Shard::new("C", "R", &w)).await.unwrap(),
        before_retag,
        "重打标不改事实与行 id"
    );
    let tagged: (String, String) = sqlx::query_as("SELECT taxonomy_version, CAST(event_types AS CHAR) FROM b_merchant_group_event WHERE occurred_on='2026-08-26'").fetch_one(&pool).await.unwrap();
    assert_eq!(tagged.0, "v1");
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&tagged.1).unwrap(),
        labels[0].all()
    );
    assert_eq!(
        read_events(&pool, Shard::new("C", "R", &Window::span(d(24), d(24))))
            .await
            .unwrap()
            .1,
        vec![frozen]
    );
    let versions: Vec<(String,)> =
        sqlx::query_as("SELECT taxonomy_version FROM b_merchant_group_agent_metric_daily")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(versions, vec![("v1".into(),)]);
    let (account,): (Option<String>,) =
        sqlx::query_as("SELECT official_user_id FROM b_merchant_group_agent_metric_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(account.as_deref(), Some("staff.a"), "重打标保留已有账号");
    testutil::drop_mysql_database(pool).await;
}
