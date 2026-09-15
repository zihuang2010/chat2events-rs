//! `daily` 的测试 —— `daily` 的子模块，私有项照常可见。
//!
//! 覆盖的是 [`run_rooms`] 那个循环的**全部职责**：背压 · 预算 · 记账 · 失败分流。
//! 生产的 [`run_room`] 在读完之后才接上抽取与落库（要真端点真库），
//! 所以这里传一个只读的「一个群干什么」进去。

use super::{labeling::*, run::*, tally::*};
use crate::{
    stage::ingest::{self, IngestError},
    testutil,
    window::Window,
};
use chrono::NaiveDate;
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};

/// 不设限的预算 —— 只有那条专测 deadline 的用例才给已经到点的值。
fn forever() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 8, d).unwrap()
}

fn ms(d: u32) -> i64 {
    testutil::upstream_ms(day(d).and_hms_opt(9, 0, 0).unwrap())
}

/// 打标排空的 deadline —— 测试里几乎都不该撞上，给一个远得不可能到的点。
fn far() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(3600)
}

/// **过点不开新批次，但队列仍然排空并记账。**
///
/// 打标独立成队列时丢掉了「过点不开新群」那道保护（此前它在 `run_room` 里），于是
/// 分类端点夜里退化能让进程跑过 24 小时，下一轮在 `classify::Cache` 的文件锁上失败
/// —— 整轮零事实、零指标、零 `run_failure`。
///
/// 到点之后**直接 drop 接收端**是另一种错法：那批群事实在、标签是 NULL，而库里
/// 没有任何东西解释为什么。名单必须出得来，由调用方记 `run_failure(stage='classify')`。
#[tokio::test]
async fn classification_stops_starting_batches_after_the_deadline_but_still_accounts_for_them() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    let (tx, rx) = mpsc::channel(4);
    for index in 0..3 {
        tx.send(ClassifyTask {
            corpid: "C".into(),
            roomid: format!("R{index}"),
            event_count: index,
        })
        .await
        .unwrap();
    }
    drop(tx);
    let ran = AtomicUsize::new(0);
    // deadline 已经过了 —— 一个批次都不该开。
    let tally = run_classification(rx, 2, std::time::Instant::now(), |_task| {
        ran.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(()))
    })
    .await;
    assert_eq!(ran.load(Ordering::SeqCst), 0, "过点之后不该再开新批次");
    assert_eq!((tally.ok, tally.failed), (0, 0));
    assert_eq!(
        tally.skipped,
        vec![
            ("C".to_owned(), "R0".to_owned()),
            ("C".to_owned(), "R1".to_owned()),
            ("C".to_owned(), "R2".to_owned()),
        ],
        "队列必须排空，名单交给调用方记 run_failure(stage='classify')"
    );
}

#[tokio::test]
async fn classification_queue_backpressures_drains_and_isolates_failures() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::{Semaphore, mpsc};
    let (tx, rx) = mpsc::channel(1);
    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let worker = tokio::spawn({
        let (started, finished, gate) = (started.clone(), finished.clone(), gate.clone());
        async move {
            run_classification(rx, 2, far(), |task| {
                let (started, finished, gate) = (started.clone(), finished.clone(), gate.clone());
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    gate.acquire().await.unwrap().forget();
                    finished.fetch_add(1, Ordering::SeqCst);
                    if task.roomid == "R1" {
                        Err("合成打标失败".into())
                    } else {
                        Ok(())
                    }
                }
            })
            .await
        }
    });
    for index in 0..3 {
        tx.send(ClassifyTask {
            corpid: "C".into(),
            roomid: format!("R{index}"),
            event_count: index,
        })
        .await
        .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while started.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(finished.load(Ordering::SeqCst), 0);
    assert!(matches!(
        tx.try_send(ClassifyTask {
            corpid: "C".into(),
            roomid: "R3".into(),
            event_count: 0
        }),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    drop(tx);
    gate.add_permits(3);
    let tally = tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((tally.ok, tally.failed), (2, 1));
    assert_eq!(finished.load(Ordering::SeqCst), 3);
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_label_batches_share_eight_slots_and_preserve_saved_events_on_failure() {
    use crate::{
        stage::classify::{Classifier, TaxonomyType},
        stage::extract::Event,
        stage::ingest::Role,
        stage::metrics::{self, Status},
        stage::store,
    };
    use axum::{Json, Router, http::StatusCode, routing::post};
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio::sync::{Semaphore, mpsc};

    let pool = testutil::mysql_pool("label_pipeline").await;
    let gate = Arc::new(Semaphore::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new().route("/v1/chat/completions", post({
        let (gate, requests, active, maximum) = (gate.clone(), requests.clone(), active.clone(), maximum.clone());
        move |Json(body): Json<serde_json::Value>| {
            let (gate, requests, active, maximum) = (gate.clone(), requests.clone(), active.clone(), maximum.clone());
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                gate.acquire().await.unwrap().forget();
                let listing = body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap();
                assert!(!listing.contains("private-account"));
                let count = listing.lines().filter(|line| line.starts_with('#')).count();
                assert!((1..=50).contains(&count));
                active.fetch_sub(1, Ordering::SeqCst);
                if listing.contains("FAIL") {
                    (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"合成打标失败","type":"invalid_request_error","code":"invalid_request"}})))
                } else {
                    let output = json!({"assignments":(1..=count).map(|index| json!({"index":index,"type_id":"a"})).collect::<Vec<_>>()});
                    (StatusCode::OK, Json(testutil::completion(&output.to_string(), "stop")))
                }
            }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = testutil::fresh_root("daily", "batch-pipeline");
    let classifier = Arc::new(
        Classifier::new(
            "v1",
            vec![TaxonomyType {
                type_id: "a".into(),
                parent_name: "服务".into(),
                name: "改期".into(),
                description: "商家要求改期".into(),
            }],
            testutil::test_classify_llm(&base, "test"),
            &root,
        )
        .unwrap(),
    );
    let w = Window::span(day(25), day(26));
    let (tx, rx) = mpsc::channel(8);
    for (room, count) in [("R0", 401), ("R1", 50), ("EMPTY", 0)] {
        let events: Vec<_> = (0..count)
            .map(|index| Event {
                corpid: "C".into(),
                roomid: room.into(),
                source_msg_ids: vec![format!("m{index}")],
                first_msg_time: day(25).and_hms_opt(9, 0, 0).unwrap(),
                last_msg_time: day(25).and_hms_opt(9, 1, 0).unwrap(),
                first_agent_reply_time: Some(day(25).and_hms_opt(9, 1, 0).unwrap()),
                occurred_on: day(25),
                asker: "external".into(),
                asker_role: Role::External,
                agents: vec!["agent".into()],
                first_responder: Some("agent".into()),
                summary: if room == "R0" && index == 400 {
                    "ZZFAIL 商家要求改期".into()
                } else {
                    format!("{room}-{index:04} 商家要求改期")
                },
                // 首响时间 = 末条消息时间，收尾的是客服。
                last_msg_role: Some(Role::Internal),
                followup_wait_max_sec: Some(0),
                source_messages: vec![],
            })
            .collect();
        let group = metrics::group_rows("C", room, &w, &BTreeMap::new(), Some(&events), Status::Ok);
        store::write_room(
            &pool,
            day(28),
            store::Shard::new("C", room, &w),
            Some(&events),
            None,
            &group,
            &[],
            &BTreeMap::from([("agent".into(), "private-account".into())]),
        )
        .await
        .unwrap();
        tx.send(ClassifyTask {
            corpid: "C".into(),
            roomid: room.into(),
            event_count: count,
        })
        .await
        .unwrap();
    }
    drop(tx);
    let worker = async {
        let slots = Semaphore::new(8);
        run_classification(rx, 8, far(), |task| {
            classify_room(&pool, &classifier, &slots, 8, day(28), &w, task)
        })
        .await
    };
    let observe = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while requests.load(Ordering::SeqCst) < 8 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let saved: (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COUNT(event_type) FROM b_merchant_group_event")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(saved, (451, 0), "模型阻塞时事件已经独立保存");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            8,
            "多个群合计只占八个模型名额"
        );
        gate.add_permits(20);
    };
    let (tally, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(worker, observe)
    })
    .await
    .unwrap();
    assert_eq!(
        (tally.ok, tally.failed),
        (2, 1),
        "失败群不影响其他群，零事件也完成"
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 8);
    let saved: (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COUNT(event_type) FROM b_merchant_group_event")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved, (451, 450), "失败群已成功的批次标签也保留");
    let metrics: (u64,) = sqlx::query_as(
        "SELECT CAST(SUM(event_count) AS UNSIGNED) FROM b_merchant_group_agent_metric_daily",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(metrics.0, 50, "失败群不能发布残缺分类指标");
    let failure: (String, String, String) = sqlx::query_as("SELECT g.extraction_status, g.classification_status, f.stage FROM b_merchant_group_metric_daily g JOIN b_merchant_group_run_failure f USING(corpid,roomid) WHERE g.roomid='R0' LIMIT 1")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(failure, ("ok".into(), "failed".into(), "classify".into()));
    let accounts: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_agent_metric_daily WHERE official_user_id='private-account'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(accounts.0, 1);
    drop(classifier);
    server.abort();
    let _ = server.await;
    testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_daily_persists_latest_internal_account_without_sending_it_to_the_model() {
    let root = testutil::fresh_root("daily", "official-account");
    let rows: Vec<_> = [
        (25, "EXTERNAL", "external-id", json!("external-account")),
        (26, "INTERNAL", "1688857091747413", json!("old-account")),
        (27, "INTERNAL", "1688857091747413", json!("13523611718")),
        (28, "INTERNAL", "1688857091747413", serde_json::Value::Null),
    ]
    .into_iter()
    .map(|(d, role, sender, account)| {
        json!({
            "schemaVersion": 1, "parserVersion": 1, "corpId": "C", "officialRoomId": "R",
            "sourceMessageId": format!("m{d}"), "standardType": "TEXT", "messageTime": ms(d),
            "sender": {"easyUserId": sender, "officialUserId": account, "identityType": role},
            "content": "商家要求改期", "analysisText": "商家要求改期",
            "semanticPayload": {"replyTo": null}
        })
    })
    .collect();
    testutil::write_month(&root, "202608", "C", "R", &rows);
    let pool = testutil::mysql_pool("daily_account").await;
    let output = json!({"events":[{"ref":null,"msg_indexes":[1,2,3,4],"summary":"商家要求改期","still_open":false}]}).to_string();
    let (base, server) =
        testutil::http_model(vec![(200, testutil::completion(&output, "stop"))], false);
    // 两队分开取 —— 抽取那份喂 LiveModel，打标那份喂 Classifier，跟生产同一个形状。
    let model = crate::stage::extract::LiveModel::new(testutil::test_llm(&base, "test"));
    let classifier = crate::stage::classify::Classifier::new(
        "v0",
        vec![],
        testutil::test_classify_llm(&base, "test"),
        &root.join("cache"),
    )
    .unwrap();
    let outcome = run_room(
        &root,
        &pool,
        &model,
        day(30),
        "C",
        "R",
        &Window::span(day(25), day(29)),
        &["202608".into()],
        100,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, Outcome::Ok { events: 1, .. }));
    let pending: (Option<String>,) =
        sqlx::query_as("SELECT event_type FROM b_merchant_group_event")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending.0, None, "事件保存无需等待打标");
    classify_room(
        &pool,
        &classifier,
        &tokio::sync::Semaphore::new(8),
        8,
        day(30),
        &Window::span(day(25), day(29)),
        ClassifyTask {
            corpid: "C".into(),
            roomid: "R".into(),
            event_count: 1,
        },
    )
    .await
    .unwrap();
    let stored: Vec<(String, Option<String>, u32)> = sqlx::query_as(
        "SELECT agent, official_user_id, event_count FROM b_merchant_group_agent_metric_daily",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        vec![("1688857091747413".into(), Some("13523611718".into()), 1)]
    );
    let requests = server.join().unwrap();
    let prompt = requests[0]["messages"].to_string();
    for private in [
        "1688857091747413",
        "13523611718",
        "old-account",
        "external-account",
    ] {
        assert!(!prompt.contains(private), "元信息账号不得进入模型请求");
    }
    testutil::drop_mysql_database(pool).await;
}

/// `bad` 时把 `sourceMessageId` 置空 —— 缺必填字段 = 该群失败（群级，非整轮）。
fn write_room(root: &Path, room: &str, bad: bool) {
    let rows: Vec<_> = (25..30)
        .map(|d| {
            json!({
                "schemaVersion": 1, "parserVersion": 1,
                "corpId": "C", "officialRoomId": room,
                "sourceMessageId": if bad { String::new() } else { format!("{room}-{d}") },
                "standardType": "TEXT", "messageTime": ms(d),
                "sender": {"easyUserId": "u1", "officialUserId": null,
                           "identityType": "INTERNAL"},
                "content": "原文", "analysisText": "正文",
                "semanticPayload": {"replyTo": null}
            })
        })
        .collect();
    testutil::write_month(root, "202608", "C", room, &rows);
}

/// 测试用的「一个群干什么」：只走 ①② 读取，不碰 ③④⑤⑥⑦（那几步要真端点真库）。
///
/// 生产的 [`run_room`] 在读完之后才接上抽取与落库，所以这里覆盖到的
/// **正是 [`run_rooms`] 那个循环的全部职责**：背压、预算、记账、失败分流。
fn read_only(
    root: &Path,
    w: &Window,
) -> impl Fn(
    String,
    String,
)
    -> std::pin::Pin<Box<dyn Future<Output = std::result::Result<Outcome, IngestError>> + Send>> {
    let (root, w) = (root.to_path_buf(), w.clone());
    move |corp, room| {
        let (root, w) = (root.clone(), w.clone());
        Box::pin(async move {
            let conv =
                tokio::task::spawn_blocking(move || ingest::read_room(&root, &corp, &room, &w))
                    .await
                    .unwrap()?;
            Ok(if conv.msgs.is_empty() {
                Outcome::Empty
            } else {
                Outcome::Ok {
                    msgs: conv.msgs.len(),
                    events: 0,
                }
            })
        })
    }
}

/// 背压那条 `if set.len() >= concurrency` 分两个排空点，最容易漏掉或重复计一个群。
/// 20 个群 / 并发 3：既跑满背压分支，也跑到循环后的收尾分支。
#[tokio::test]
async fn every_room_is_counted_exactly_once_and_failures_stay_isolated() {
    let root = testutil::fresh_root("daily", "concurrent");
    // 每 5 个坏一个 → 4 个失败、16 个成功
    let rooms: Vec<_> = (0..20)
        .map(|i| {
            let room = format!("R{i:02}");
            write_room(&root, &room, i % 5 == 0);
            ("C".to_string(), room)
        })
        .collect();
    let w = Window::span(day(25), day(29));

    let mut t = Tally::default();
    run_rooms(&rooms, 3, forever(), &mut t, read_only(&root, &w))
        .await
        .unwrap();

    assert_eq!((t.ok, t.failed), (16, 4), "坏群整体跳过，好群一个不漏");
    assert_eq!(t.msgs, 16 * 5, "每个成功的群 5 条，不重不漏");
}

/// 预算已经到点：一个群都不该开，且**不能报成 `failed`** —— 「没轮到」和
/// 「跑了但坏了」下一轮处置一样，但看日志时的诊断完全不同。
#[tokio::test]
async fn an_exhausted_budget_starts_no_room_and_is_not_counted_as_failure() {
    let root = testutil::fresh_root("daily", "deadline");
    let rooms: Vec<_> = (0..5)
        .map(|i| {
            let room = format!("R{i}");
            write_room(&root, &room, false);
            ("C".to_string(), room)
        })
        .collect();
    let w = Window::span(day(25), day(29));

    let mut t = Tally::default();
    run_rooms(&rooms, 3, Instant::now(), &mut t, read_only(&root, &w))
        .await
        .unwrap();

    assert_eq!((t.ok, t.failed, t.skipped.len()), (0, 0, 5));
    assert_eq!(t.msgs, 0);
    // **名单本身是承重的**：`run_span` 靠它给每个群补一行 `run_failure`。
    // 只有计数时，库里对这批群是整行缺失 —— 报表上的洞查不出任何原因。
    assert_eq!(t.skipped, rooms, "没轮到的群必须留下名单，顺序即队列顺序");
}

/// 窗口上界越过 T-2 的一律拒绝 —— 越界行写进 group 表就再也删不掉（`REPLACE` 覆盖
/// 不到的行没有任何东西会清）。日常跑批的窗口恰好贴着 T-2，不能被这道闸误伤。
#[test]
fn a_window_reaching_past_two_days_ago_is_refused() {
    let run_date = day(15);
    // 日常跑批：`[T-3, T-2]`，上界正好等于 T-2，必须放行
    assert!(check_window(run_date, &Window::new(run_date, 2)).is_ok());
    // 补跑历史同样放行
    assert!(check_window(run_date, &Window::span(day(1), day(13))).is_ok());
    // 昨天和今天都不行 —— 会话存档 T+2 才到齐，这两天的数据不全
    for until in [day(14), day(15)] {
        let err = check_window(run_date, &Window::span(day(1), until))
            .expect_err("窗口越过 T-2 必须拒绝");
        assert!(
            err.to_string().contains("越过 T-2"),
            "错误要说清是窗口上界的问题：{err}"
        );
    }
}

/// 队首每天挪一格：连着 N 天，**每个群都轮到过队首**，被砍的不会总是同一批。
///
/// 这条在生产上是静默的 —— 偏移恒 0 时字母序末尾那批群会继续天天被预算砍掉，
/// 日志一个字都不变，只是报表上那几个群一直没数据。
#[test]
fn the_queue_head_rotates_so_no_room_is_always_last() {
    let rooms: Vec<_> = (0..7).map(|i| ("C".to_string(), format!("R{i}"))).collect();
    // 假设预算只跑得完队首 3 个
    let mut head_over_a_week: BTreeSet<String> = BTreeSet::new();
    for d in 1..=7 {
        let mut today = rooms.clone();
        rotate_daily(&mut today, day(d));
        head_over_a_week.extend(today[..3].iter().map(|(_, room)| room.clone()));
    }
    assert_eq!(
        head_over_a_week.len(),
        rooms.len(),
        "一周之内每个群都该轮到队首，否则被砍的永远是同一批"
    );
}

/// 任何一个群的**连续**空档都有上界 —— 上面那条只保证「终究会轮到」，不保证「多久轮到」。
///
/// ⚠️ **群数必须显著大于观察天数，否则这条测不出任何东西。** 上面那条用 7 个群跑 7 天，
/// 观察窗口正好是一整圈 —— 在那个规模上，每天挪 1 格和每天挪 [`STRIDE`] 格
/// **表现完全一样**，于是它对步长是盲的。挪 1 格的真实后果只在 n ≫ 观察天数时才显形：
/// 被处理的那一段每天只滑动一位，一个群掉出去就要等队伍绕完整圈才回来，
/// 空档 = n - k 天且**连续**。n=1000 / k=400 时那是 601 天。
///
/// 7 天这个上界不是随便取的：`lookback_days ≥ 2` 时，隔一天没跑到还能靠下一轮的
/// 窗口重叠补回来；连续一周都没轮到就补不回来了。
#[test]
fn no_room_goes_dark_for_a_long_stretch() {
    const N: usize = 1000;
    const K: usize = 400; // 假设预算只跑得完队首 400 个
    const DAYS: i64 = 1200;
    const MAX_GAP: i64 = 7;

    let rooms: Vec<_> = (0..N)
        .map(|i| ("C".to_string(), format!("R{i:04}")))
        .collect();
    // `day(d)` 只变「日」、最多到 31，跑不了 1200 天 —— 从它起步再加天数。
    let start = day(1);
    let mut last_seen: Vec<Option<i64>> = vec![None; N];
    let mut worst = (0i64, String::new());

    for d in 0..DAYS {
        let mut today = rooms.clone();
        rotate_daily(&mut today, start + chrono::Duration::days(d));
        for (_, room) in &today[..K] {
            let i: usize = room[1..].parse().expect("房间名就是下标");
            if let Some(prev) = last_seen[i] {
                let gap = d - prev - 1;
                if gap > worst.0 {
                    worst = (gap, room.clone());
                }
            }
            last_seen[i] = Some(d);
        }
    }

    assert!(
        worst.0 <= MAX_GAP,
        "{} 连续 {} 天没轮到（上界 {MAX_GAP}）—— 步长太小，被砍的群一掉队就是几百天，\
         而 run_failure 表里每天照常记着「本轮没轮到」，报表上那个洞查不出任何原因",
        worst.1,
        worst.0
    );
}

/// 等待并发空位跨过截止点时，不能再开下一个群。
#[tokio::test]
async fn waiting_for_capacity_does_not_start_a_room_after_deadline() {
    let rooms = vec![("C".into(), "R1".into()), ("C".into(), "R2".into())];
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut t = Tally::default();
    run_rooms(&rooms, 1, deadline, &mut t, |_, _| async move {
        tokio::time::sleep_until((deadline + Duration::from_millis(20)).into()).await;
        Ok(Outcome::Empty)
    })
    .await
    .unwrap();
    assert_eq!((t.empty, t.skipped.len()), (1, 1));
    assert_eq!(t.skipped, [("C".to_string(), "R2".to_string())]);
}

/// 并发之后 `Upstream` 仍然是**整轮**失败，不会被降级成某个群的事。
#[tokio::test]
async fn upstream_version_mismatch_fails_the_whole_round() {
    let root = testutil::fresh_root("daily", "upstream");
    write_room(&root, "R0", false);
    testutil::write_month(
        &root,
        "202608",
        "C",
        "R1",
        &[json!({
            "schemaVersion": 99, "parserVersion": 1,
            "corpId": "C", "officialRoomId": "R1",
            "sourceMessageId": "x", "standardType": "TEXT", "messageTime": ms(25),
            "sender": {"easyUserId": "u1", "officialUserId": null,
                       "identityType": "INTERNAL"},
            "content": "原文", "analysisText": "正文",
            "semanticPayload": {"replyTo": null}
        })],
    );
    let rooms = [
        ("C".to_string(), "R0".to_string()),
        ("C".to_string(), "R1".to_string()),
    ];
    let w = Window::span(day(25), day(29));

    let mut t = Tally::default();
    let e = run_rooms(&rooms, 2, forever(), &mut t, read_only(&root, &w))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("不做兼容层"), "{e}");
}

/// 窗口内没有消息 —— **既不是成功也不是失败，一行都不写**（生产路径上
/// `run_room` 在这一步直接返回，`store::write_room` 根本不会被调到）。
#[tokio::test]
async fn a_room_with_no_messages_in_the_window_writes_nothing() {
    let root = testutil::fresh_root("daily", "empty");
    write_room(&root, "R0", false); // 消息在 08-25 ~ 08-29
    let rooms = [("C".to_string(), "R0".to_string())];
    // 窗口挪到消息之后：文件在、消息不在
    let w = Window::span(day(30), day(31));

    let mut t = Tally::default();
    run_rooms(&rooms, 2, forever(), &mut t, read_only(&root, &w))
        .await
        .unwrap();
    assert_eq!((t.ok, t.failed, t.empty), (0, 0, 1));
}

/// 关闭的连接池能确定性地证明读取失败会尝试记账，并保留两次失败的原因。
#[tokio::test]
async fn a_read_failure_attempts_to_record_the_failure_without_calling_the_model() {
    let root = testutil::fresh_root("daily", "record-read-failure");
    write_room(&root, "R", true);
    let cfg: crate::config::Config = toml::from_str(include_str!("../../../config.toml")).unwrap();
    let llm = crate::llm::Llm::new(&cfg.llm, &cfg.llm.extract, "unused-test-key".into()).unwrap();
    let model = crate::stage::extract::LiveModel::new(llm.clone());
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .connect_lazy("mysql://test:test@127.0.0.1:1/test")
        .unwrap();
    pool.close().await;
    let err = run_room(
        &root,
        &pool,
        &model,
        day(30),
        "C",
        "R",
        &Window::span(day(25), day(29)),
        &["202608".into()],
        100,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("缺必填字段"), "{err}");
    assert!(err.contains("记录 run_failure 失败"), "{err}");
}

// ─────────────────────────────────────────────────────────────────────────────
// 收尾段 —— `finish` 与它的产能账
//
// 这一段此前是 `run_span` 里唯一没测试的 99 行，而三条退出码规则全压在里面。
// **下面钉的是现状，不是主张。** 尤其「零群 → 退出码 0」那条：`run.rs` 的注释
// 已经写明它是个已知的空转路径，改不改是产品决定，这里只保证改的时候有对照物。
// ─────────────────────────────────────────────────────────────────────────────

/// 跑批配置直接读仓库里那份 —— 跟 `config/tests.rs`、`llm/tests.rs` 同一个做法，
/// 免得为两个字段手搓一个会跟 config.toml 漂开的 fixture。
fn cfg() -> crate::config::Config {
    toml::from_str(include_str!("../../../config.toml")).unwrap()
}

fn usage(secs: f64) -> crate::stage::extract::Usage {
    crate::stage::extract::Usage { calls: 1, secs }
}

fn window() -> Window {
    Window::span(day(1), day(2))
}

#[test]
fn finish_warns_but_succeeds_when_the_index_gave_no_rooms() {
    // ⚠️ 钉的是**现状**：一个群都没查到时整轮仍以退出码 0 结束（见 `run.rs` 里
    // 那条注释）。要改成 `return Err` 是产品决定 —— 改的那天这条会红，那正是它的用处。
    let out = finish(
        &Tally::default(),
        &ClassifyTally::default(),
        0,
        &cfg(),
        &window(),
        usage(0.0),
        Duration::from_secs(1),
    );
    assert!(out.is_ok(), "零群不改退出码");
}

#[test]
fn finish_warns_but_succeeds_when_every_room_was_empty() {
    let t = Tally {
        empty: 3,
        ..Tally::default()
    };
    let out = finish(
        &t,
        &ClassifyTally::default(),
        3,
        &cfg(),
        &window(),
        usage(0.0),
        Duration::from_secs(1),
    );
    assert!(out.is_ok(), "全空窗口不是失败");
}

#[test]
fn finish_fails_the_round_when_rooms_never_got_their_turn() {
    let t = Tally {
        ok: 1,
        skipped: vec![("C".into(), "R".into())],
        ..Tally::default()
    };
    let err = finish(
        &t,
        &ClassifyTally::default(),
        2,
        &cfg(),
        &window(),
        usage(10.0),
        Duration::from_secs(1),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("没轮到"), "{err}");
}

#[test]
fn finish_fails_the_round_on_extract_classify_or_sync_failures() {
    // 三条失败通道各自都要能单独把整轮拉成非零码 —— 少任何一条，那一批群
    // 会从指标里静默消失而监控看到成功。
    for (extract_failed, classify_failed, unsynced) in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
        let t = Tally {
            ok: 1,
            failed: extract_failed,
            unsynced,
            ..Tally::default()
        };
        let classified = ClassifyTally {
            ok: 1,
            failed: classify_failed,
            ..ClassifyTally::default()
        };
        let err = finish(
            &t,
            &classified,
            2,
            &cfg(),
            &window(),
            usage(10.0),
            Duration::from_secs(1),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("run_failure"),
            "({extract_failed},{classify_failed},{unsynced}) 应该拉成非零码：{err}"
        );
    }
}

#[test]
fn finish_succeeds_when_every_room_went_through() {
    let t = Tally {
        ok: 2,
        msgs: 10,
        events: 3,
        ..Tally::default()
    };
    let classified = ClassifyTally {
        ok: 2,
        ..ClassifyTally::default()
    };
    let out = finish(
        &t,
        &classified,
        2,
        &cfg(),
        &window(),
        usage(20.0),
        Duration::from_secs(1),
    );
    assert!(out.is_ok());
}

#[test]
fn projected_hours_divides_by_extracted_rooms_not_by_total_rooms() {
    // 10 个群里只有 2 个真跑完，烧掉 100 模型秒 → 每群 50 秒 × 10 个群 ÷ 并发 5 ÷ 3600。
    // 拿 `rooms` 当分母会得到 100/10*10/5/3600，恰好小 5 倍，且不会报任何错。
    let t = Tally {
        ok: 2,
        ..Tally::default()
    };
    let got = projected_extract_hours(&t, 10, usage(100.0), 5).unwrap();
    assert!(
        (got - (100.0 / 2.0 * 10.0 / 5.0 / 3600.0)).abs() < 1e-12,
        "{got}"
    );

    // 一个群都没跑完 → None，不是 0（承重不变量 4）。
    assert!(projected_extract_hours(&Tally::default(), 10, usage(0.0), 5).is_none());
}
