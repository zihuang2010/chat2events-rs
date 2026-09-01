//! `ingest` 的测试 —— `ingest` 的子模块，私有项照常可见。
//! fixture 在 `crate::testutil`。

use super::{layout::*, read::*, types::*};
use crate::{testutil, window::Window};
use chrono::NaiveDate;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 8, d).unwrap()
}

/// 本地时间转上游的毫秒时间戳。「本地 = UTC+8」的换算收在 [`testutil::upstream_ms`]，
/// 不在测试里再手写一份 —— 那是和生产 `TZ` 同一个事实的第二份拷贝。
fn ms(d: u32, hour: u32, min: u32) -> i64 {
    testutil::upstream_ms(day(d).and_hms_opt(hour, min, 0).unwrap())
}

fn row(id: &str, at_ms: i64, sender: &str) -> Value {
    json!({
        "schemaVersion": 1, "parserVersion": 1,
        "corpId": "C", "officialRoomId": "R",
        "sourceMessageId": id, "standardType": "TEXT",
        "messageTime": at_ms,
        "sender": {"easyUserId": sender, "officialUserId": null,
                   "identityType": "INTERNAL"},
        "content": "原文兜底", "analysisText": "正文",
        "semanticPayload": {"replyTo": {"sourceMessageId": "回指目标",
                                        "sourceMsgType": 0}}
    })
}

/// 5 天 × 2 条，两个发言人。
fn sample() -> Vec<Value> {
    (25..30)
        .flat_map(|d| {
            [
                row(&format!("m{d}a"), ms(d, 9, 0), "u1"),
                row(&format!("m{d}b"), ms(d, 10, 0), "u2"),
            ]
        })
        .collect()
}

/// 建一个只有一个月文件的 raw 区。
fn raw(name: &str, month: &str, rows: &[Value]) -> PathBuf {
    let root = testutil::fresh_root("ingest", name);
    testutil::write_month(&root, month, "C", "R", rows);
    root
}

/// 样本那 5 天的窗口。
fn all() -> Window {
    Window::span(day(25), day(29))
}

// ── 纯函数 ───────────────────────────────────────────────────────────

#[test]
fn months_single_month() {
    assert_eq!(months(&all()), ["202608"]);
}

#[test]
fn months_across_months() {
    let w = Window::span(day(31), NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
    assert_eq!(months(&w), ["202608", "202609"]);
}

#[test]
fn room_path_is_the_layout() {
    assert_eq!(
        room_path(Path::new("/raw"), "202608", "C", "R"),
        Path::new("/raw/202608/C/R.ndjson")
    );
}

// ── 样本集成 ─────────────────────────────────────────────────────────

#[test]
fn list_rooms_only_rooms_with_files() {
    let root = raw("list", "202608", &sample());
    assert_eq!(
        list_rooms(&root, &all()),
        [("C".to_string(), "R".to_string())]
    );
}

#[test]
fn split_no_dupes_no_gaps_ordered_counts_match() {
    let root = raw("split", "202608", &sample());
    let conv = read_room(&root, "C", "R", &all()).unwrap();

    assert_eq!(conv.msgs.len(), 10);
    assert_eq!(
        conv.msgs
            .iter()
            .map(|m| &m.msg_id)
            .collect::<HashSet<_>>()
            .len(),
        10
    );
    // 后续分段 / 切点 / 便签全站在这条上
    assert!(conv.msgs.windows(2).all(|w| w[0].at <= w[1].at));
    assert!(conv.msgs.iter().all(|m| m.corp == "C" && m.room == "R"));
    // msg_counts 必须由同一批消息算出，不能是另走一条路数出来的
    for (d, (n, senders)) in &conv.msg_counts {
        let same_day: Vec<_> = conv.msgs.iter().filter(|m| m.at.date() == *d).collect();
        assert_eq!(*n, same_day.len());
        assert_eq!(
            *senders,
            same_day
                .iter()
                .map(|m| &m.sender_id)
                .collect::<HashSet<_>>()
                .len()
        );
    }
    assert_eq!(conv.msg_counts.values().map(|(n, _)| n).sum::<usize>(), 10);
}

#[test]
fn window_excludes_everything_outside() {
    let root = raw("window", "202608", &sample());
    let win = Window::span(day(26), day(27));
    let msgs = read_room(&root, "C", "R", &win).unwrap().msgs;
    assert_eq!(msgs.len(), 4);
    assert!(
        msgs.iter()
            .all(|m| win.since() <= m.at.date() && m.at.date() <= win.until())
    );
}

#[test]
fn text_never_empty_falls_back_to_content() {
    let mut rows = sample();
    rows[0]["analysisText"] = json!("");
    let root = raw("text", "202608", &rows);
    let msgs = read_room(&root, "C", "R", &all()).unwrap().msgs;
    assert!(msgs.iter().all(|m| !m.text.is_empty()));
    assert_eq!(msgs[0].text, "原文兜底");
}

#[test]
fn at_is_local_time_not_utc() {
    let root = raw("tz", "202608", &sample());
    let msgs = read_room(&root, "C", "R", &all()).unwrap().msgs;
    // 构造时给的是 08-25 09:00 本地时间；读回来必须还是它，不是 01:00 UTC
    assert_eq!(msgs[0].at, day(25).and_hms_opt(9, 0, 0).unwrap());
}

#[test]
fn empty_window_returns_empty_conversation() {
    let root = raw("empty", "202608", &sample());
    let conv = read_room(&root, "C", "R", &Window::span(day(20), day(20))).unwrap();
    assert!(conv.msgs.is_empty() && conv.msg_counts.is_empty());
}

// ── 下钻 ─────────────────────────────────────────────────────────────

#[test]
fn drilldown_returns_exactly_requested_ids() {
    let root = raw("drill", "202608", &sample());
    let want = ["m25a".to_string(), "m27b".to_string(), "m29a".to_string()];
    let got = read_by_ids(&root, "C", "R", &all(), &want).unwrap();
    assert_eq!(
        got.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        ["m25a", "m27b", "m29a"]
    );
}

#[test]
fn drilldown_errors_when_ids_missing() {
    let root = raw("drill-miss", "202608", &sample());
    let want = ["m25a".to_string(), "不存在的ID".to_string()];
    let e = read_by_ids(&root, "C", "R", &all(), &want).unwrap_err();
    assert!(matches!(e, IngestError::Missing(_)), "{e}");
    assert!(e.to_string().contains("取不到 1 个"), "{e}");
}

#[test]
fn drilldown_errors_when_window_too_narrow() {
    let root = raw("drill-narrow", "202608", &sample());
    let want = ["m29a".to_string()];
    let w = Window::span(day(25), day(25));
    assert!(read_by_ids(&root, "C", "R", &w, &want).is_err());
}

// ── 五道守卫 ─────────────────────────────────────────────────────────

#[test]
fn dedupe_same_msg_id_appears_once() {
    let mut rows = sample();
    rows.extend(sample()); // 每条来两遍
    let root = raw("dupe", "202608", &rows);
    assert_eq!(read_room(&root, "C", "R", &all()).unwrap().msgs.len(), 10);
}

#[test]
fn missing_required_field_fails_room() {
    for blank in ["sourceMessageId", "easyUserId"] {
        let mut rows = sample();
        if blank == "sourceMessageId" {
            rows[1]["sourceMessageId"] = json!("");
        } else {
            rows[1]["sender"]["easyUserId"] = json!("");
        }
        let root = raw(&format!("required-{blank}"), "202608", &rows);
        let e = read_room(&root, "C", "R", &all()).unwrap_err();
        assert!(matches!(e, IngestError::Room(_)), "{blank}: {e}");
        assert!(e.to_string().contains("缺必填字段"), "{blank}: {e}");
    }
}

/// 认不出的 `identityType` = 该群失败，**不兜底成任意一边**。
///
/// 这是 `Role` 换掉裸字符串之后新长出来的守卫：此前 `unwrap_or_default()` 会让上游
/// 加一个新身份类型时静默变成空串，然后 `== "INTERNAL"` 恒假 —— 那一整个群的消息
/// 全被当成商家发言，`agents` 空、首响全 NULL，而没有任何一处会报错。
#[test]
fn an_unknown_identity_type_fails_the_room() {
    for bad in [json!("BOT"), json!(""), json!(null)] {
        let mut rows = sample();
        rows[1]["sender"]["identityType"] = bad.clone();
        let root = raw(&format!("role-{bad}"), "202608", &rows);
        let e = read_room(&root, "C", "R", &all()).unwrap_err();
        assert!(matches!(e, IngestError::Room(_)), "{bad}: {e}");
        assert!(e.to_string().contains("identityType"), "{bad}: {e}");
    }
}

#[test]
fn text_never_empty_both_blank_fails_room() {
    // 契约第一条就是 `text` 恒非空。COALESCE 兜完底还是空 = 上游连占位符都没给。
    // ⚠️ `content` 给空串时 COALESCE 返回空串而不是 NULL —— 只判 NULL 漏得掉，
    //    所以这里两个分支都要跑。
    for content in [json!(""), json!(null)] {
        let mut rows = sample();
        rows[1]["analysisText"] = json!("");
        rows[1]["content"] = content.clone();
        let root = raw(
            &format!("blank-text-{}", content.is_null()),
            "202608",
            &rows,
        );
        let e = read_room(&root, "C", "R", &all()).unwrap_err();
        assert!(matches!(e, IngestError::Room(_)), "{content}: {e}");
        assert!(e.to_string().contains("缺必填字段"), "{content}: {e}");
    }
}

#[test]
fn upstream_version_mismatch_fails_run() {
    let mut rows = sample();
    rows[0]["parserVersion"] = json!(2);
    let root = raw("version", "202608", &rows);
    let e = read_room(&root, "C", "R", &all()).unwrap_err();
    // 整轮 vs 该群，处置方式不同 —— 类型上必须分得开
    assert!(matches!(e, IngestError::Upstream(_)), "{e}");
}

#[test]
fn misplaced_file_errors() {
    let mut rows = sample();
    for r in &mut rows {
        r["corpId"] = json!("别的CORP");
    }
    let root = raw("misplaced", "202608", &rows);
    let e = read_room(&root, "C", "R", &all()).unwrap_err();
    assert!(e.to_string().contains("路径却是"), "{e}");
}

#[test]
fn month_guard_rejects_foreign_month() {
    // 8 月的消息塞进 9 月文件，窗口跨月两边都读
    let root = raw("month", "202609", &sample());
    let w = Window::span(day(25), NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
    let e = read_room(&root, "C", "R", &w).unwrap_err();
    assert!(e.to_string().contains("不在文件所属月份"), "{e}");
}

#[test]
fn month_guard_known_blindspot() {
    // **这条测试记录的是守卫拦不住的情况，不是 bug。**
    // 守卫只看窗口过滤后活下来的行：8 月的消息被放进 9 月文件、而窗口整个落在
    // 8 月时，我们根本不会打开 9 月文件，那条消息就是静默漏掉的。
    // 实际敞口≈0（跨月窗口本来就读两个月），根治要让 mirror 认识 messageTime，
    // 那会破坏「上游字段名只出现在 ingest 里」。这里把代价写下来，不假装它不存在。
    let root = raw("blindspot", "202609", &sample());
    let conv = read_room(&root, "C", "R", &Window::span(day(25), day(26))).unwrap();
    assert!(
        conv.msgs.is_empty(),
        "漏掉了，且不会报错 —— 这是已认领的代价"
    );
}

// ── 跨月 ─────────────────────────────────────────────────────────────

#[test]
fn cross_month_reads_both_files_missing_one_is_ok() {
    let root = raw("crossmonth", "202608", &sample());
    let w = Window::span(day(29), NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
    // 9 月文件不存在（新建群 / 已解散）—— DuckDB 对不存在的路径直接报错，
    // 所以必须先过 is_file
    let msgs = read_room(&root, "C", "R", &w).unwrap().msgs;
    assert_eq!(msgs.len(), 2);

    let sep: Vec<Value> = (1..3)
        .map(|d| {
            let at = NaiveDate::from_ymd_opt(2026, 9, d)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap();
            row(&format!("s{d}"), testutil::upstream_ms(at), "u1")
        })
        .collect();
    testutil::write_month(&root, "202609", "C", "R", &sep);

    let msgs = read_room(&root, "C", "R", &w).unwrap().msgs;
    assert_eq!(
        msgs.iter()
            .map(|m| m.at.format("%m").to_string())
            .collect::<HashSet<_>>(),
        HashSet::from(["08".to_string(), "09".to_string()])
    );
}
