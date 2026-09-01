//! ⑥ 的测试。断言集搬自 Python 版的 `metrics.py::_self_check`。
//!
//! 承重不变量 4（`Ok([])` 与 `Failed` 绝不混淆）和 5（失败的群在 agent 表上整行缺失）
//! 就靠这几条守着 —— 错了不会报错，只会让报表安静地偏小。

use super::*;
use crate::{
    classify::{CURRENT_VERSION, UNTYPED},
    ingest::Role,
};
use chrono::NaiveDate;

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
fn ev(day: u32, h: u32, reply_h: Option<u32>, responder: Option<&str>, agents: &[&str], role: Role) -> Event {
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
        rows.iter().map(|r| (r.msg_count, r.sender_count)).collect::<Vec<_>>(),
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
        rows.iter().all(|r| (r.event_count, r.merchant_event_count, r.unreplied_count)
            == (Some(0), Some(0), Some(0))),
        "Ok([]) 该是 0 —— 这天确实没有业务事件，是正常状态"
    );
    assert!(
        rows.iter().all(|r| r.first_reply_p50_sec.is_none() && r.first_reply_p90_sec.is_none()),
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
        (by[&d(25)].event_count, by[&d(25)].merchant_event_count, by[&d(25)].unreplied_count),
        (Some(4), Some(3), Some(1))
    );
    assert_eq!(
        (by[&d(25)].first_reply_p50_sec, by[&d(25)].first_reply_p90_sec),
        (Some(7200), Some(7200))
    );
    assert_eq!(
        (by[&d(26)].event_count, by[&d(26)].merchant_event_count, by[&d(26)].unreplied_count,
         by[&d(26)].first_reply_p50_sec, by[&d(26)].first_reply_p90_sec),
        (Some(1), Some(1), Some(0), Some(0), Some(0))
    );
}

#[test]
fn every_day_in_the_window_gets_a_row_even_with_no_messages() {
    let rows = group_rows("C", "R", &days(), &BTreeMap::new(), Some(&sample()), Status::Ok);
    assert_eq!(rows.iter().map(|r| r.dt).collect::<Vec<_>>(), days().days());
    assert!(rows.iter().all(|r| (r.msg_count, r.sender_count) == (0, 0)));
}

#[test]
fn both_attributions_are_computable_from_the_same_stored_facts() {
    // 事实全存，解释随时可换，**不重跑 LLM**
    let evs = sample();
    let t = types(evs.len());
    let key = |rows: Vec<AgentRow>| -> BTreeMap<(String, NaiveDate), u32> {
        rows.into_iter().map(|r| ((r.agent, r.dt), r.event_count)).collect()
    };

    // a3 那一行是平台发起的工单推送 —— 首响不算它，但处理量算：推工单也是干活
    let fr = key(agent_rows("C", "R", &evs, &t, CURRENT_VERSION, Attribution::FirstResponder));
    assert_eq!(
        fr,
        [(("a1".into(), d(25)), 1), (("a2".into(), d(25)), 1),
         (("a3".into(), d(25)), 1), (("a1".into(), d(26)), 1)].into()
    );
    let ap = key(agent_rows("C", "R", &evs, &t, CURRENT_VERSION, Attribution::AllParticipants));
    assert_eq!(
        ap,
        [(("a1".into(), d(25)), 1), (("a2".into(), d(25)), 2),
         (("a3".into(), d(25)), 1), (("a1".into(), d(26)), 1)].into()
    );
    // 未回复的事件在 first_responder 口径下不落到任何人头上
    assert_eq!(fr.values().sum::<u32>(), 4);
    assert_eq!(ap.values().sum::<u32>(), 5);
}

#[test]
fn agent_rows_carry_the_six_column_semantic_key() {
    let evs = sample();
    let rows = agent_rows("C", "R", &evs, &types(evs.len()), CURRENT_VERSION, Attribution::default());
    assert!(rows.iter().all(|r| r.corp == "C"
        && r.room == "R"
        && r.event_type == UNTYPED
        && r.taxonomy_version == "v0"));
}

#[test]
#[should_panic(expected = "types 必须与 events 一一对应")]
fn misaligned_types_are_a_bug_not_a_silent_mislabel() {
    agent_rows("C", "R", &sample(), &types(2), CURRENT_VERSION, Attribution::default());
}
