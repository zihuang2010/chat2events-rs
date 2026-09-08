//! 人工恢复已保存事实的未完成分类；按群日补齐，不重新抽取冻结事实。

use super::labeling::{ClassifyTask, classify_room};
use crate::{
    Result,
    classify::{CURRENT_VERSION, Classifier},
    config::Config,
    llm::Llm,
    store,
    window::Window,
};
use chrono::{Local, NaiveDate};
use futures_util::StreamExt;
use sqlx::MySqlPool;
use tokio::sync::Semaphore;

pub async fn recover(
    config: &Config,
    llm: &Llm,
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<()> {
    if since > until {
        return Err("补标日期范围倒挂".into());
    }
    store::check_schema(pool).await?;
    let types = store::read_taxonomy(pool, CURRENT_VERSION).await?;
    let (cache_dir, llm) = (config.classify.cache_dir.clone(), llm.clone());
    let classifier = tokio::task::spawn_blocking(move || {
        Classifier::new(CURRENT_VERSION, types, llm, &cache_dir)
    })
    .await??;
    let concurrency = config.classify.concurrency;
    let slots = Semaphore::new(concurrency);
    let run_date = Local::now().date_naive();
    // 单日是精确的恢复范围，不把中间已完成的日期纳入重写；零事件也走发布收尾。
    let work = store::unfinished_days(pool, since, until)
        .map(|row| {
            let classifier = &classifier;
            let slots = &slots;
            async move {
                let (corpid, roomid, day, event_count) = row?;
                classify_room(
                    pool,
                    classifier,
                    slots,
                    concurrency,
                    run_date,
                    &Window::span(day, day),
                    ClassifyTask {
                        corpid,
                        roomid,
                        event_count: event_count as usize,
                    },
                )
                .await
            }
        })
        .buffer_unordered(concurrency);
    tokio::pin!(work);
    let (mut ok, mut failed) = (0, 0);
    while let Some(result) = work.next().await {
        match result {
            Ok(()) => ok += 1,
            Err(error) => {
                failed += 1;
                tracing::error!("补标失败：{error}");
            }
        }
    }
    tracing::info!(ok, failed, %since, %until, "未完成分类恢复结束");
    if failed > 0 {
        return Err(format!("{failed} 个群日补标失败").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        extract::Event,
        ingest::Role,
        metrics::{self, Status},
        testutil,
    };
    use std::collections::BTreeMap;

    #[tokio::test]
    #[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
    async fn mysql_recovery_keeps_facts_and_labels_and_completes_empty_days() {
        let pool = testutil::mysql_pool("recover").await;
        sqlx::query("INSERT INTO b_merchant_group_taxonomy (version,type_id,parent_name,name,description) VALUES ('v1','a','订单变更','改期','修改服务日期')").execute(&pool).await.unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 8, 25).unwrap();
        let window = Window::span(day, day);
        let events: Vec<_> = (0..3)
            .map(|i| Event {
                corpid: "C".into(),
                roomid: "R".into(),
                source_msg_ids: vec![format!("m{i}")],
                first_msg_time: day.and_hms_opt(10, i, 0).unwrap(),
                last_msg_time: day.and_hms_opt(10, i, 0).unwrap(),
                first_agent_reply_time: None,
                occurred_on: day,
                asker: "merchant00000001".into(),
                asker_role: Role::External,
                agents: vec![],
                first_responder: None,
                summary: format!("商家提出第{}次改期", i % 2),
            })
            .collect();
        for (room, facts) in [("R", events.as_slice()), ("empty", &[][..])] {
            let group = metrics::group_rows(
                "C",
                room,
                &window,
                &BTreeMap::new(),
                Some(facts),
                Status::Ok,
            );
            store::write_room(
                &pool,
                day,
                store::Shard::new("C", room, &window),
                Some(facts),
                None,
                &group,
                &BTreeMap::new(),
            )
            .await
            .unwrap();
        }
        let before = store::read_events(&pool, store::Shard::new("C", "R", &window))
            .await
            .unwrap();
        sqlx::query("UPDATE b_merchant_group_event SET event_type='__untyped__',event_types=JSON_ARRAY('__untyped__'),taxonomy_version='v1' WHERE id=?")
            .bind(before.0[0]).execute(&pool).await.unwrap();
        let time_before: Vec<(String, Option<chrono::NaiveDateTime>)> = sqlx::query_as(
            "SELECT roomid,fact_completed_time FROM b_merchant_group_metric_daily ORDER BY roomid",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let (base, server) = testutil::http_model(
            vec![
                (
                    200,
                    testutil::completion(
                        r#"{"assignments":[{"index":1,"type_ids":["__untyped__"]}]}"#,
                        "stop",
                    ),
                ),
                (
                    200,
                    testutil::completion(
                        r#"{"assignments":[{"index":1,"type_ids":["a"]}]}"#,
                        "stop",
                    ),
                ),
            ],
            false,
        );
        let mut config: Config = toml::from_str(include_str!("../../config.toml")).unwrap();
        config.classify.cache_dir = testutil::fresh_root("recover", "cache");
        let llm = testutil::test_classify_llm(&base, "test");
        // 模拟前一进程已把首批答案持久化，再由新进程恢复剩余批次。
        let previous = Classifier::new(
            CURRENT_VERSION,
            store::read_taxonomy(&pool, CURRENT_VERSION).await.unwrap(),
            llm.clone(),
            &config.classify.cache_dir,
        )
        .unwrap();
        previous.classify(&[&events[0].summary]).await.unwrap();
        drop(previous);
        recover(&config, &llm, &pool, day, day).await.unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        let prompt = requests[1]["messages"][1]["content"].as_str().unwrap();
        assert!(
            prompt.contains(&events[1].summary) && !prompt.contains(&events[0].summary),
            "已完成批次不重新问模型"
        );
        assert_eq!(
            store::read_events(&pool, store::Shard::new("C", "R", &window))
                .await
                .unwrap(),
            before
        );
        let labels: Vec<(String,)> =
            sqlx::query_as("SELECT event_type FROM b_merchant_group_event ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            labels,
            vec![
                ("__untyped__".into(),),
                ("a".into(),),
                ("__untyped__".into(),)
            ]
        );
        let states: Vec<(String,)> =
            sqlx::query_as("SELECT classification_status FROM b_merchant_group_metric_daily")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(states, vec![("ok".into(),); 2], "零事件也必须完成发布");
        let time_after: Vec<(String, Option<chrono::NaiveDateTime>)> = sqlx::query_as(
            "SELECT roomid,fact_completed_time FROM b_merchant_group_metric_daily ORDER BY roomid",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(time_after, time_before);
        // 已完成群日不再被选中，重跑不依赖模型在线。
        recover(&config, &llm, &pool, day, day).await.unwrap();
        testutil::drop_mysql_database(pool).await;
    }
}
