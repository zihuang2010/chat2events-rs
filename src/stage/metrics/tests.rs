//! ⑥ 的测试。
//!
//! 承重不变量 4（`Ok([])` 与 `Failed` 绝不混淆）和 5（失败的群在 agent 表上整行缺失）
//! 就靠这几条守着 —— 错了不会报错，只会让报表安静地偏小。

use super::*;
use crate::{
    stage::classify::{CURRENT_VERSION, UNTYPED},
    stage::extract::Event,
    stage::ingest::Role,
    window::Window,
};
use chrono::NaiveDate;
use std::collections::BTreeMap;

fn d(day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 8, day).unwrap()
}

fn days() -> Window {
    Window::span(d(25), d(26))
}

fn counts() -> BTreeMap<NaiveDate, (usize, usize)> {
    [(d(25), (100usize, 5usize)), (d(26), (40, 3))].into()
}

/// `reply_h = None` 表示未回复。`role` 默认商家发起。
fn ev(
    day: u32,
    h: u32,
    reply_h: Option<u32>,
    responder: Option<&str>,
    agents: &[&str],
    role: Role,
) -> Event {
    Event {
        corpid: "C".into(),
        roomid: "R".into(),
        source_msg_ids: vec!["m1".into()],
        first_msg_time: d(day).and_hms_opt(h, 0, 0).unwrap(),
        last_msg_time: d(day).and_hms_opt(h, 0, 0).unwrap(),
        first_agent_reply_time: reply_h.map(|r| d(day).and_hms_opt(r, 0, 0).unwrap()),
        occurred_on: d(day),
        asker: "EXT".into(),
        asker_role: role,
        agents: agents.iter().map(|s| s.to_string()).collect(),
        first_responder: responder.map(str::to_string),
        summary: "商家要求加单，平台已受理".into(),
        // 只有一条来源消息，首尾同一条。
        last_msg_role: Some(role),
        followup_wait_max_sec: Some(0),
        // ⑥ 指标一个字都不看展示列。
        source_messages: vec![],
    }
}

fn sample() -> Vec<Event> {
    vec![
        ev(25, 9, Some(10), Some("a1"), &["a1", "a2"], Role::External), // 3600s
        ev(25, 9, Some(11), Some("a2"), &["a2"], Role::External),       // 7200s
        ev(25, 9, None, None, &[], Role::External),                     // 未回复
        ev(26, 9, Some(9), Some("a1"), &["a1"], Role::External),        // 0s
        // 平台发起的工单推送：首响恒 0 秒、且永远算「已回复」。
        // 它必须进 event_count，但绝不能进 merchant_event_count / 未回复 / 分位数。
        ev(25, 9, Some(9), Some("a3"), &["a3"], Role::Internal),
    ]
}

fn types(n: usize) -> Vec<&'static str> {
    vec![UNTYPED; n]
}

#[test]
fn failed_leaves_every_event_level_column_null_never_zero() {
    let rows = group_rows("C", "R", &days(), &counts(), None, Status::Failed);
    assert_eq!(
        rows.iter()
            .map(|r| (r.msg_count, r.sender_count))
            .collect::<Vec<_>>(),
        [(100, 5), (40, 3)],
        "失败的群丢了消息级指标 —— 那两个不依赖抽取"
    );
    assert!(
        rows.iter().all(|r| r.event_count.is_none()
            && r.merchant_event_count.is_none()
            && r.unreplied_count.is_none()
            && r.first_reply_p50_sec.is_none()
            && r.first_reply_p90_sec.is_none()),
        "Failed 用了 0 冒充 NULL（承重不变量 4）"
    );
    assert!(rows.iter().all(|r| r.status == Status::Failed));
    // 承重不变量 5：失败的群在 agent 表上整行缺失，不是 0
    assert!(agent_rows("C", "R", &[], &[], CURRENT_VERSION, Attribution::default()).is_empty());
}

#[test]
fn ok_with_no_events_is_zero_not_null() {
    let rows = group_rows("C", "R", &days(), &counts(), Some(&[]), Status::Ok);
    assert!(
        rows.iter().all(
            |r| (r.event_count, r.merchant_event_count, r.unreplied_count)
                == (Some(0), Some(0), Some(0))
        ),
        "Ok([]) 该是 0 —— 这天确实没有业务事件，是正常状态"
    );
    assert!(
        rows.iter()
            .all(|r| r.first_reply_p50_sec.is_none() && r.first_reply_p90_sec.is_none()),
        "没有已回复事件时分位数只能是 NULL"
    );
}

#[test]
fn first_response_stats_count_merchant_started_events_only() {
    let evs = sample();
    let rows = group_rows("C", "R", &days(), &counts(), Some(&evs), Status::Ok);
    let by: BTreeMap<NaiveDate, &GroupRow> = rows.iter().map(|r| (r.dt, r)).collect();

    // 平台发起的那个事件进了 event_count(4)，没进分母(3)/未回复(1)/分位数
    assert_eq!(
        (
            by[&d(25)].event_count,
            by[&d(25)].merchant_event_count,
            by[&d(25)].unreplied_count
        ),
        (Some(4), Some(3), Some(1))
    );
    assert_eq!(
        (
            by[&d(25)].first_reply_p50_sec,
            by[&d(25)].first_reply_p90_sec
        ),
        (Some(7200), Some(7200))
    );
    assert_eq!(
        (
            by[&d(26)].event_count,
            by[&d(26)].merchant_event_count,
            by[&d(26)].unreplied_count,
            by[&d(26)].first_reply_p50_sec,
            by[&d(26)].first_reply_p90_sec
        ),
        (Some(1), Some(1), Some(0), Some(0), Some(0))
    );
}

#[test]
fn every_day_in_the_window_gets_a_row_even_with_no_messages() {
    let rows = group_rows(
        "C",
        "R",
        &days(),
        &BTreeMap::new(),
        Some(&sample()),
        Status::Ok,
    );
    assert_eq!(rows.iter().map(|r| r.dt).collect::<Vec<_>>(), days().days());
    assert!(rows.iter().all(|r| (r.msg_count, r.sender_count) == (0, 0)));
}

#[test]
fn both_attributions_are_computable_from_the_same_stored_facts() {
    // 事实全存，解释随时可换，**不重跑 LLM**
    let evs = sample();
    let t = types(evs.len());
    let key = |rows: Vec<AgentRow>| -> BTreeMap<(String, NaiveDate), u32> {
        rows.into_iter()
            .map(|r| ((r.agent, r.dt), r.event_count))
            .collect()
    };

    // a3 那一行是平台发起的工单推送 —— 首响不算它，但处理量算：推工单也是干活
    let fr = key(agent_rows(
        "C",
        "R",
        &evs,
        &t,
        CURRENT_VERSION,
        Attribution::FirstResponder,
    ));
    assert_eq!(
        fr,
        [
            (("a1".into(), d(25)), 1),
            (("a2".into(), d(25)), 1),
            (("a3".into(), d(25)), 1),
            (("a1".into(), d(26)), 1)
        ]
        .into()
    );
    let ap = key(agent_rows(
        "C",
        "R",
        &evs,
        &t,
        CURRENT_VERSION,
        Attribution::AllParticipants,
    ));
    assert_eq!(
        ap,
        [
            (("a1".into(), d(25)), 1),
            (("a2".into(), d(25)), 2),
            (("a3".into(), d(25)), 1),
            (("a1".into(), d(26)), 1)
        ]
        .into()
    );
    // 未回复的事件在 first_responder 口径下不落到任何人头上
    assert_eq!(fr.values().sum::<u32>(), 4);
    assert_eq!(ap.values().sum::<u32>(), 5);
}

#[test]
fn agent_rows_carry_the_six_column_semantic_key() {
    let evs = sample();
    let rows = agent_rows(
        "C",
        "R",
        &evs,
        &types(evs.len()),
        CURRENT_VERSION,
        Attribution::default(),
    );
    assert!(rows.iter().all(|r| r.corp == "C"
        && r.room == "R"
        && r.event_type == UNTYPED
        && r.taxonomy_version == CURRENT_VERSION));
}

#[test]
#[should_panic(expected = "types 必须与 events 一一对应")]
fn misaligned_types_are_a_bug_not_a_silent_mislabel() {
    agent_rows(
        "C",
        "R",
        &sample(),
        &types(2),
        CURRENT_VERSION,
        Attribution::default(),
    );
}

/// 客服日消息量与事件级指标**互不依赖**：它没有 `Status`、没有 `event_type`、
/// 没有 `taxonomy_version`，抽取失败的群照样算得出来。
/// 这条钉住「为什么它是另一张表而不是 `AgentRow` 上的一列」。
#[test]
fn agent_msg_rows_do_not_depend_on_extraction_or_taxonomy() {
    let counts: BTreeMap<(NaiveDate, String), usize> = [
        ((d(25), "a1".to_string()), 12),
        ((d(25), "a9".to_string()), 40),
        ((d(26), "a1".to_string()), 3),
    ]
    .into();
    let rows = agent_msg_rows("C", "R", &counts);
    assert_eq!(
        rows.iter()
            .map(|r| (r.agent.as_str(), r.dt, r.msg_count))
            .collect::<Vec<_>>(),
        [("a1", d(25), 12), ("a9", d(25), 40), ("a1", d(26), 3)]
    );
    assert!(rows.iter().all(|r| r.corp == "C" && r.room == "R"));

    // `a9` 一次首响都没抢到，于是 `agent_rows` 上**一行都没有** —— 而消息量表上有他。
    // 这正是这张表存在的理由：挂成 AgentRow 的一列，最该看的人恰好看不见。
    let events = sample();
    let agents = agent_rows(
        "C",
        "R",
        &events,
        &types(events.len()),
        CURRENT_VERSION,
        Attribution::FirstResponder,
    );
    assert!(!agents.iter().any(|r| r.agent == "a9"));
    assert!(rows.iter().any(|r| r.agent == "a9"));
}

/// 首响分位数走**工作时段口径**，不是墙钟差。
///
/// 这条是这个仓库最怕的那类错的正面用例：夜里进来的消息按墙钟算是 10 小时、
/// 按工作时段算是 30 分钟，两个数都「看起来正常」，没有任何东西会报错。
#[test]
fn first_reply_quantiles_use_working_hours_not_wall_clock() {
    // 23:00 商家发问，次日 09:00 客服回 —— 墙钟 10 小时，工作时段只有次日 08:30→09:00。
    let overnight = ev(25, 23, Some(9), Some("a1"), &["a1"], Role::External);
    let overnight = Event {
        first_agent_reply_time: Some(d(26).and_hms_opt(9, 0, 0).unwrap()),
        ..overnight
    };
    let rows = group_rows(
        "C",
        "R",
        &days(),
        &counts(),
        Some(std::slice::from_ref(&overnight)),
        Status::Ok,
    );
    let by: BTreeMap<NaiveDate, &GroupRow> = rows.iter().map(|r| (r.dt, r)).collect();
    assert_eq!(
        by[&d(25)].first_reply_p50_sec,
        Some(1800),
        "墙钟差是 36000，工作时段口径必须是 1800"
    );
}
