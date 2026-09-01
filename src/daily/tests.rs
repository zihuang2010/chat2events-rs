//! `daily` 的测试 —— `daily` 的子模块，私有项照常可见。
//!
//! 覆盖的是 [`run_rooms`] 那个循环的**全部职责**：背压 · 预算 · 记账 · 失败分流。
//! 生产的 [`run_room`] 在读完之后才接上抽取与落库（要真端点真库），
//! 所以这里传一个只读的「一个群干什么」进去。

use super::*;
use crate::testutil;
use chrono::NaiveDate;
use serde_json::json;
use std::path::Path;

/// 不设限的预算 —— 只有那条专测 deadline 的用例才给已经到点的值。
fn forever() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 8, d).unwrap()
}

fn ms(d: u32) -> i64 {
    (day(d).and_hms_opt(9, 0, 0).unwrap() - chrono::Duration::hours(8))
        .and_utc()
        .timestamp_millis()
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
) -> impl Fn(String, String) -> std::pin::Pin<Box<dyn Future<Output = std::result::Result<Outcome, IngestError>> + Send>>
{
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
                Outcome::Ok { msgs: conv.msgs.len(), events: 0 }
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

    assert_eq!((t.ok, t.failed, t.over_budget), (0, 0, 5));
    assert_eq!(t.msgs, 0);
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
    let rooms = [("C".to_string(), "R0".to_string()), ("C".to_string(), "R1".to_string())];
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
