//! `store` 的**跨文件**测试与共享 fixture —— 三条 `mysql_` 用例在隔离 MySQL 上
//! 验证事务边界、冻结与打标状态，CI 显式执行（见 `docs/deploy.md`）。
//!
//! 单个文件的单元测试住在各自文件底部：`facts` 的冻结守卫、`sql` 的列数与占位符。

use super::{facts::*, labels::*, read::*, schema::*, sql::*};
use crate::{
    BoxError,
    stage::classify::Label,
    stage::extract::Event,
    stage::ingest::Role,
    stage::metrics::{AgentRow, GroupRow},
    window::Window,
};
use chrono::{NaiveDate, NaiveDateTime};
use sqlx::MySqlPool;
use std::collections::BTreeMap;

/// `read_events` **按契约不读展示列** `source_messages`（理由见 `store::read`：
/// 那条路服务打标与重打标，读回正文会让全历史重打标把整个语料拉进内存）。
/// 所以往返比对一律拿这个摘掉展示列的副本比，别改 `read_events` 去迁就测试。
fn facts_only(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .cloned()
        .map(|mut e| {
            e.source_messages.clear();
            e
        })
        .collect()
}

/// 为既有事务、冻结和重打标用例准备已完成两阶段处理的记录。
#[allow(clippy::too_many_arguments)]
async fn write_room(
    pool: &MySqlPool,
    run_date: NaiveDate,
    corp: &str,
    room: &str,
    days: &Window,
    events: Option<&[Event]>,
    labels: &[Label],
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
        // 客服日消息量由下面 `mysql_agent_msg_daily_*` 单独验，这里的用例只关心事实与标签。
        &[],
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
        last_msg_role: Some(Role::External),
        followup_wait_max_sec: Some(0),
        // 与 source_msg_ids 等长同序 —— 这个夹具会真的写进库，形状要和 assemble 产的一致。
        source_messages: vec![crate::stage::extract::SourceMessage {
            msg_id: "m1".into(),
            at: format!("{}-{day:02} 09:00:00", "2026-08"),
            sender_id: "EXT".into(),
            sender_role: Role::External.as_str(),
            text: "商家要求加单".into(),
        }],
    }
}

/// **原文快照有保留期，而且清的是展示列不是事实列。**
///
/// 未脱敏的客户正文有两个副本：镜像区那份受 `raw_retention_months` 清理，
/// `source_messages` 这一列此前**永生**，且看板无登录谁都能下钻 —— 留存期分家是顺手
/// 改出来的，不是决定。这条钉住三件事：只清夹在 `[since, before)` 里的、事实列一个字
/// 不动（承重不变量 1）、以及清完之后原文取不到是 NULL 不是空串（410 靠它区分）。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_source_messages_expire_without_touching_facts() {
    let pool = crate::testutil::mysql_pool("source_retention").await;
    sqlx::raw_sql(
        "INSERT INTO b_merchant_group_event \
         (corpid,roomid,source_msg_ids,first_msg_time,last_msg_time,occurred_on,asker,asker_role,agents,summary,source_messages) VALUES \
         ('C','R',JSON_ARRAY('m1'),'2026-05-10 09:00:00','2026-05-10 09:01:00','2026-05-10','merchant00000001','EXTERNAL',JSON_ARRAY(),'更早的一个月','[{\"text\":\"更早\"}]'), \
         ('C','R',JSON_ARRAY('m2'),'2026-06-10 09:00:00','2026-06-10 09:01:00','2026-06-10','merchant00000001','EXTERNAL',JSON_ARRAY(),'刚过期的那个月','[{\"text\":\"过期\"}]'), \
         ('C','R',JSON_ARRAY('m3'),'2026-07-10 09:00:00','2026-07-10 09:01:00','2026-07-10','merchant00000001','EXTERNAL',JSON_ARRAY(),'还在保留期内','[{\"text\":\"保留\"}]');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let day = |m: u32, d: u32| chrono::NaiveDate::from_ymd_opt(2026, m, d).unwrap();
    let pruned = prune_source_messages(&pool, day(6, 1), day(7, 1))
        .await
        .unwrap();
    assert_eq!(pruned, 1, "只清刚滑出保留期的那个月");

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT summary, source_messages FROM b_merchant_group_event ORDER BY occurred_on",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // 下界之外的那个月**没被碰**：稳态下它早清完了，每晚重扫整段历史才是那个代价。
    assert!(rows[0].1.is_some(), "下界之外的行不在本轮范围内");
    // NULL 而不是空串 —— `read_source_messages` 靠它区分「永久取不到」（410）。
    assert_eq!(rows[1].1, None, "刚过期的那个月必须清成 NULL");
    assert!(rows[2].1.is_some(), "保留期内的原文必须还在");
    // 事实列一个字都不能动（承重不变量 1：展示列有保留期，事实列冻结）。
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["更早的一个月", "刚过期的那个月", "还在保留期内"]
    );
    let facts: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM b_merchant_group_event WHERE asker_role = 'EXTERNAL' AND first_msg_time IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(facts.0, 3);

    crate::testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_pipeline_upgrade_matches_the_documented_schema() {
    let pool = crate::testutil::mysql_pool("pipeline_upgrade").await;
    sqlx::raw_sql(
        "ALTER TABLE b_merchant_group_event MODIFY event_type VARCHAR(64) NOT NULL, \
         MODIFY taxonomy_version VARCHAR(16) NOT NULL; \
         ALTER TABLE b_merchant_group_metric_daily DROP COLUMN classification_status, DROP COLUMN agent_accounts; \
         ALTER TABLE b_merchant_group_run_failure DROP COLUMN stage; \
         INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason) VALUES ('2026-08-28','C','R','历史失败');"
    ).execute(&pool).await.unwrap();
    assert!(check_schema(&pool).await.is_err());
    let section = include_str!("../../../docs/deploy.md")
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
        ALTER TABLE b_merchant_group_event DROP INDEX idx_overview; \
        ALTER TABLE b_merchant_group_run_failure DROP INDEX idx_room_stage_time, MODIFY gmt_created_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, \
        MODIFY gmt_modified_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP; \
        INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,extraction_status) VALUES ('C','R','2026-08-25',0,0,'ok');")
        .execute(&pool).await.unwrap();
    assert!(check_schema(&pool).await.is_err());
    let sql = include_str!("../../../docs/deploy.md")
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
        config::Config,
        llm::Llm,
        stage::classify::Classifier,
        stage::metrics::{self, Attribution, Status},
        testutil,
    };
    use std::collections::BTreeMap;
    let pool = testutil::mysql_pool("store").await;
    check_schema(&pool).await.unwrap();
    let cfg: Config = toml::from_str(include_str!("../../../config.toml")).unwrap();
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
    // ⚠️ `read_events` **故意不读 `source_messages`**（见 `store::read` 里那段注释：
    // 这条路服务 ⑤ 打标和 `recompute` 重打标，两者只用 `summary`；把正文一起读回来，
    // 全历史重打标会把整个正文语料拉进内存）。所以往返比对要把**展示列**摘掉 ——
    // 而「它确实没被读回来」本身就是承重的，单独钉一条。
    assert!(
        initial.1.iter().all(|e| e.source_messages.is_empty()),
        "read_events 不该把展示列 source_messages 读回来 —— 那会让重打标拉进整个语料"
    );
    assert_eq!(initial.1, facts_only(&events), "所有事实列读写往返必须一致");
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
    assert_eq!(before_retag.1, facts_only(std::slice::from_ref(&moved)));
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
    let tagged: (String, String) = sqlx::query_as(
        "SELECT taxonomy_version, event_type FROM b_merchant_group_event WHERE occurred_on='2026-08-26'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tagged.0, "v1");
    assert_eq!(tagged.1, labels[0].type_id());
    assert_eq!(
        read_events(&pool, Shard::new("C", "R", &Window::span(d(24), d(24))))
            .await
            .unwrap()
            .1,
        facts_only(std::slice::from_ref(&frozen))
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

/// `b_merchant_group_agent_msg_daily` 的生命周期：**跟着事实阶段走，不跟着抽取和词表走。**
///
/// 三件事一起钉住，它们正是「为什么它不是 `b_merchant_group_agent_metric_daily`
/// 上的一列」的三条理由：
///   1. **抽取失败照写** —— 那张表上失败的群是整行缺失（承重不变量 5），这张表不是。
///   2. **打标不碰它** —— `publish_classification` 整段删重写那张表，这张表一行不动。
///   3. **REPLACE 幂等** —— 同窗口重跑不产生重复行，也不需要删重写。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_agent_msg_daily_survives_extraction_failure_and_relabeling() {
    use crate::{
        stage::metrics::{self, Status},
        testutil,
    };
    let pool = testutil::mysql_pool("agent_msg").await;
    check_schema(&pool).await.unwrap();

    let w = Window::span(d(25), d(26));
    let shard = Shard::new("C", "R", &w);
    let counts: BTreeMap<NaiveDate, (usize, usize)> = [(d(25), (7, 3)), (d(26), (2, 1))].into();
    let agent_msg = metrics::agent_msg_rows(
        "C",
        "R",
        &[
            ((d(25), "agent00000000001".to_string()), 4),
            ((d(25), "agent00000000002".to_string()), 1),
            ((d(26), "agent00000000001".to_string()), 2),
        ]
        .into(),
    );
    let read = |pool: MySqlPool| async move {
        sqlx::query_as::<_, (String, NaiveDate, u32)>(
            "SELECT agent, dt, msg_count FROM b_merchant_group_agent_msg_daily \
             ORDER BY dt, agent",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };

    // ① 抽取失败：事件表和客服事件表一行不写，消息量表照写。
    let failed = metrics::group_rows("C", "R", &w, &counts, None, Status::Failed);
    super::write_room(
        &pool,
        d(27),
        shard,
        None,
        Some("合成抽取失败"),
        &failed,
        &agent_msg,
        &BTreeMap::new(),
    )
    .await
    .unwrap();
    let saved = read(pool.clone()).await;
    assert_eq!(
        saved,
        [
            ("agent00000000001".into(), d(25), 4),
            ("agent00000000002".into(), d(25), 1),
            ("agent00000000001".into(), d(26), 2),
        ],
        "抽取失败不影响消息量 —— 模型挂了，「谁说了多少」仍然是已知的"
    );

    // ② 打标发布分类指标：整段删重写的是客服事件表，这张表一行不动。
    finish_classification(&pool, shard, &[]).await.unwrap();
    assert_eq!(read(pool.clone()).await, saved, "打标不该碰消息量表");

    // ③ 同窗口重跑：REPLACE 靠 uk_agent_msg_daily 覆盖，不产生重复行。
    super::write_room(
        &pool,
        d(27),
        shard,
        Some(&[]),
        None,
        &metrics::group_rows("C", "R", &w, &counts, Some(&[]), Status::Ok),
        &agent_msg,
        &BTreeMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(read(pool).await, saved, "REPLACE 覆盖写必须幂等");
}

/// **`retry` 的挑活判据 —— 已经修好的群绝不能再重抽一遍。**
///
/// `run_failure` 只增不改，重跑成功不删旧失败行（靠 `fact_completed_time` 晚于
/// `gmt_created_time` 让它自然失效）。所以「拿整张表重跑」和「按判据重跑」在
/// 日志上长得一模一样，区别只在烧掉多少 token、以及有没有把冻结区里早已成功的天
/// 重抽一遍（承重不变量 1）—— 静默得没有任何东西会报错，只能靠这条钉住。
///
/// 五种行各造一个群，覆盖的是四条判据线：凭据晚于失败 · 抽取失败没凭据 ·
/// 连群日行都没有 · 凭据**早于**失败（拉取失败那条路不写群日行，旧的 `ok` 还在）。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_retry_picks_only_rooms_that_are_still_broken() {
    let pool = crate::testutil::mysql_pool("retry_picks").await;
    sqlx::raw_sql(
        "INSERT INTO b_merchant_group_run_failure \
         (run_date,corpid,roomid,reason,stage,window_since,window_until,gmt_created_time) VALUES \
         ('2026-08-30','C','R_FIXED','抽取失败','extract','2026-08-27','2026-08-28','2026-08-30 03:00:00.000000'), \
         ('2026-08-30','C','R_FAILED','抽取失败','extract','2026-08-27','2026-08-28','2026-08-30 03:00:00.000000'), \
         ('2026-08-30','C','R_NOROW','拉取失败，本轮不参与跑批','extract','2026-08-27','2026-08-28','2026-08-30 03:00:00.000000'), \
         ('2026-08-30','C','R_STALE','拉取失败，本轮不参与跑批','extract','2026-08-27','2026-08-28','2026-08-30 03:00:00.000000'), \
         ('2026-08-30','C','R_LEGACY','升级前写的行','extract',NULL,NULL,'2026-08-30 03:00:00.000000'), \
         ('2026-08-30','C','R_LABEL','打标失败','classify','2026-08-27','2026-08-28','2026-08-30 03:00:00.000000'), \
         ('2026-08-20','C','R_OLD','跑批日在 since 之前','extract','2026-08-17','2026-08-18','2026-08-20 03:00:00.000000'); \
         INSERT INTO b_merchant_group_metric_daily \
         (corpid,roomid,dt,msg_count,sender_count,extraction_status,fact_completed_time) VALUES \
         ('C','R_FIXED','2026-08-28',10,2,'ok','2026-08-30 04:00:00.000000'), \
         ('C','R_FAILED','2026-08-28',10,2,'failed',NULL), \
         ('C','R_STALE','2026-08-28',10,2,'ok','2026-08-29 04:00:00.000000'), \
         ('C','R_OLD','2026-08-18',10,2,'ok','2026-08-20 04:00:00.000000');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (rows, legacy) = unrepaired_extract_failures(&pool, d(25)).await.unwrap();
    assert_eq!(
        rows.iter().map(|(r, ..)| r.as_str()).collect::<Vec<_>>(),
        ["R_FAILED", "R_NOROW", "R_STALE"],
        "R_FIXED 的事实凭据晚于失败 = 已经修好，重抽它是白烧 token；\
         R_LABEL 是打标失败（事实是好的，归 recover）；R_OLD 的跑批日在 since 之前"
    );
    // 窗口原样带出来 —— 它是 `run_span` 的**删重写范围**，放宽一天就是多抽一天。
    assert!(rows.iter().all(|(_, s, u)| (*s, *u) == (d(27), d(28))));
    assert_eq!(legacy, 1, "没有窗口的历史行要数出来报警，不能静默吞掉");

    // 打标那支只要并集端点，具体哪些群日由 recover 的 unfinished_days 定。
    assert_eq!(
        classify_failure_span(&pool, d(25)).await.unwrap(),
        Some((d(27), d(28)))
    );
    assert_eq!(
        classify_failure_span(&pool, d(31)).await.unwrap(),
        None,
        "这段跑批日里没有打标失败时是 None，不是一对空日期"
    );

    crate::testutil::drop_mysql_database(pool).await;
}
