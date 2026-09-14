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
/// 不在测试里再手写一份 —— 那是和生产 `TZ_OFFSET_MICROS` 同一个事实的第二份拷贝。
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
fn oversized_room_fails_without_returning_a_partial_conversation() {
    use std::io::Write;
    let root = testutil::fresh_root("ingest", "room-budget");
    let path = room_path(&root, "202608", "C", "R");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    // 按行生成约 33 MiB 正文，测试自身不先持有整份大 JSON fixture。
    let text = "x".repeat(64 * 1024);
    for i in 0..520 {
        let mut value = row(&format!("m{i}"), ms(25, 10, 0), "u1");
        value["analysisText"] = text.clone().into();
        serde_json::to_writer(&mut file, &value).unwrap();
        writeln!(file).unwrap();
    }
    file.flush().unwrap();
    drop(file);
    assert!(
        matches!(read_room(&root, "C", "R", &all()), Err(IngestError::Room(message)) if message.contains("32 MiB"))
    );
    let small = raw("room-after-budget", "202608", &sample());
    assert_eq!(
        read_room(&small, "C", "R", &all()).unwrap().msgs.len(),
        10,
        "超限不会破坏共享读取实例"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn internal_account_keeps_easy_id_and_optional_official_id() {
    for (name, role, account, expected) in [
        (
            "internal",
            "INTERNAL",
            json!("13523611718"),
            Some("13523611718"),
        ),
        (
            "letter-account",
            "INTERNAL",
            json!("staff.a"),
            Some("staff.a"),
        ),
        ("external", "EXTERNAL", json!("external-account"), None),
        ("null-account", "INTERNAL", Value::Null, None),
        ("empty-account", "INTERNAL", json!(""), None),
    ] {
        let mut message = row("m1", ms(25, 9, 0), "1688857091747413");
        message["sender"]["identityType"] = json!(role);
        message["sender"]["officialUserId"] = account;
        let root = raw(name, "202608", &[message]);
        let conv = read_room(&root, "C", "R", &all()).unwrap();
        assert_eq!(conv.msgs[0].sender_id, "1688857091747413");
        assert_eq!(conv.msgs[0].official_user_id.as_deref(), expected);
    }
    let mut message = row("m1", ms(25, 9, 0), "1688857091747413");
    message["sender"]
        .as_object_mut()
        .unwrap()
        .remove("officialUserId");
    let root = raw("missing-account", "202608", &[message]);
    assert_eq!(
        read_room(&root, "C", "R", &all()).unwrap().msgs[0].official_user_id,
        None
    );
}

#[test]
fn daily_window_reads_only_the_two_days_before_yesterday() {
    let date = |d| NaiveDate::from_ymd_opt(2026, 9, d).unwrap();
    let rows: Vec<_> = [
        ("before", 3, 23, 59, 59, 999),
        ("start", 4, 0, 0, 0, 0),
        ("end", 5, 23, 59, 59, 999),
        ("yesterday", 6, 0, 0, 0, 0),
        ("today", 7, 0, 0, 0, 0),
    ]
    .into_iter()
    .map(|(id, d, hour, min, sec, milli)| {
        let at = date(d).and_hms_milli_opt(hour, min, sec, milli).unwrap();
        row(id, testutil::upstream_ms(at), "u1")
    })
    .collect();
    let root = raw("daily-window", "202609", &rows);
    let w = Window::new(date(7), 2);
    let conv = read_synced_room(&root, "C", "R", &w, &["202609".into()]).unwrap();
    assert_eq!(
        conv.msgs
            .iter()
            .map(|m| m.msg_id.as_str())
            .collect::<Vec<_>>(),
        ["start", "end"]
    );
    assert_eq!(conv.msg_counts.len(), 2);
    assert_eq!(conv.msg_counts[&date(4)], (1, 1));
    assert_eq!(conv.msg_counts[&date(5)], (1, 1));
}

#[test]
fn list_rooms_only_rooms_with_files() {
    let root = raw("list", "202608", &sample());
    assert_eq!(
        list_rooms(&root, &all()),
        [("C".to_string(), "R".to_string())]
    );
}

#[test]
fn synced_reads_exclude_months_the_index_did_not_confirm() {
    let sep = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
    let root = raw("synced-months", "202608", &[row("old", ms(31, 9, 0), "u1")]);
    testutil::write_month(
        &root,
        "202609",
        "C",
        "R",
        &[row(
            "current",
            testutil::upstream_ms(sep.and_hms_opt(9, 0, 0).unwrap()),
            "u1",
        )],
    );
    let w = Window::span(day(31), sep);
    let complete =
        read_synced_room(&root, "C", "R", &w, &["202608".into(), "202609".into()]).unwrap();
    assert_eq!(complete.msgs.len(), 2);
    assert_eq!(complete.msg_counts[&day(31)], (1, 1));
    assert_eq!(complete.msg_counts[&sep], (1, 1));
    let conv = read_synced_room(&root, "C", "R", &w, &["202609".into()]).unwrap();
    assert_eq!(
        conv.msgs
            .iter()
            .map(|m| m.msg_id.as_str())
            .collect::<Vec<_>>(),
        ["current"]
    );
    assert!(!conv.msg_counts.contains_key(&day(31)));
    // 202608 的文件还在盘上，只是本轮没被索引确认 —— `read_synced_room` 不读它。
    assert!(room_path(&root, "202608", "C", "R").is_file());
}

#[test]
fn a_missing_synced_month_fails_instead_of_returning_partial_messages() {
    let root = raw("missing-synced", "202608", &sample());
    let w = Window::span(day(25), NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
    assert!(read_synced_room(&root, "C", "R", &w, &["202608".into(), "202609".into()]).is_err());
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

/// `agent_msg_counts` **只数 INTERNAL**。商家侧混进来这一列就废了 ——
/// 它的用途是「这个客服自己说了多少」，用来对冲只看处理量。
#[test]
fn agent_msg_counts_only_count_internal_senders() {
    let mut rows = vec![
        row("i1", ms(25, 9, 0), "agent00000000001"),
        row("i2", ms(25, 9, 30), "agent00000000001"),
        row("i3", ms(25, 10, 0), "agent00000000002"),
        // 同一个客服的第二天 —— 键含日期，不能并进 25 号那一格。
        row("i4", ms(26, 9, 0), "agent00000000001"),
    ];
    let mut merchant = row("e1", ms(25, 11, 0), "merchant00000001");
    merchant["sender"]["identityType"] = json!("EXTERNAL");
    rows.push(merchant);

    let root = raw("agent-msg", "202608", &rows);
    let conv = read_room(&root, "C", "R", &all()).unwrap();

    assert_eq!(
        conv.agent_msg_counts,
        [
            ((day(25), "agent00000000001".to_string()), 2),
            ((day(25), "agent00000000002".to_string()), 1),
            ((day(26), "agent00000000001".to_string()), 1),
        ]
        .into(),
        "商家那条不该出现，两个客服和两天都不该并格"
    );
    // 群级的 msg_count 是另一个口径：它含商家侧，两者相加得不到对方。
    assert_eq!(conv.msg_counts[&day(25)], (4, 3));
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

/// 缺 `messageTime` 的行**跳过，不掀翻这个群** —— 而且窗口外的那种也不行。
///
/// ⚠️ 这条测试此前钉的是反面（整群失败）。改判据的是**影响面**：读取 SQL 的
/// `WHERE "at" IS NULL OR …` 只放行这一类穿过窗口过滤，而 DuckDB 扫的是**整个月文件**
/// —— 合起来，月内任何一天的一条坏行都会让这个群**整月每天**失败，含窗口早就滑过去、
/// 已经冻结的天。冻结区不会再被重抽，那个 unknown 是永久的。
///
/// 而一条没有时间的消息本来就进不了任何窗口，让它废掉一个群的整月数据是护栏打错了地方。
/// **其余必填字段仍然整群失败**（见 `missing_required_field_fails_room`）：那些行的
/// 时间有值、已经被 SQL 夹在窗口内，能走到守卫的必然在窗口内。
#[test]
fn rows_without_a_timestamp_are_skipped_not_fatal() {
    for missing in [false, true] {
        let mut rows = sample();
        if missing {
            rows[1].as_object_mut().unwrap().remove("messageTime");
        } else {
            rows[1]["messageTime"] = Value::Null;
        }
        let root = raw(&format!("missing-time-{missing}"), "202608", &rows);
        // 少的就是那一条，别的照常读出来。
        let msgs = read_room(&root, "C", "R", &all()).unwrap().msgs;
        assert_eq!(msgs.len(), rows.len() - 1, "{missing}");
        assert!(!msgs.iter().any(|m| m.msg_id == "m25b"), "{missing}");
        assert_eq!(
            read_synced_room(&root, "C", "R", &all(), &["202608".into()])
                .unwrap()
                .msgs
                .len(),
            rows.len() - 1
        );
    }
}

/// **窗口外的坏行不毒化窗口内的天** —— 这条是上面那个改动真正要买的东西。
///
/// 月文件里 08-29 有一条缺时间的行，而本轮窗口只到 08-26。改动前：DuckDB 扫整个
/// 月文件、那一行穿过窗口过滤撞上守卫，于是 08-25~08-26 这两天**每轮都失败**，
/// 直到月份翻页；而它们滑出窗口之后就冻结了，永远没有事实。
#[test]
fn a_bad_row_outside_the_window_does_not_poison_the_window() {
    let mut rows = sample();
    // 08-29 那条（`sample` 的最后一对里第一条）抽掉时间，窗口取 08-25~08-26。
    let last = rows.len() - 2;
    assert_eq!(rows[last]["sourceMessageId"], json!("m29a"));
    rows[last].as_object_mut().unwrap().remove("messageTime");
    let root = raw("bad-row-outside-window", "202608", &rows);
    let narrow = Window::span(day(25), day(26));
    let msgs = read_room(&root, "C", "R", &narrow).unwrap().msgs;
    // 08-25、08-26 各两条，一条不少。
    assert_eq!(msgs.len(), 4);
    assert!(msgs.iter().all(|m| m.at.date() <= day(26)));
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

/// 必填字段守卫的报错**不许带正文**。
///
/// 这条错误一路走到 `daily::tally` 的 `tracing::error!` 进 `run.log`，而
/// **这条路径上 `redact::body` 根本没跑过** —— 照抄 `text` 就是把未脱敏的客户消息
/// 写进日志。触发条件恰好是「上游字段形状变了」，最可能真发生的那一种。
#[test]
fn the_required_field_guard_never_logs_the_body() {
    const PII: &str = "客户张伟 13812345678 朝阳区xx路5号楼302";
    let mut rows = sample();
    // 正文在场、msg_id 缺失 —— 守卫会因为 msg_id 触发，而正文正好摆在手边
    rows[1]["sourceMessageId"] = json!("");
    rows[1]["analysisText"] = json!(PII);
    rows[1]["content"] = json!(PII);
    let root = raw("guard-no-body", "202608", &rows);
    let e = read_room(&root, "C", "R", &all()).unwrap_err().to_string();

    assert!(e.contains("缺必填字段"), "{e}");
    assert!(!e.contains("13812345678"), "报错漏了手机号：{e}");
    assert!(!e.contains("张伟"), "报错漏了姓名：{e}");
    assert!(!e.contains("朝阳区"), "报错漏了地址：{e}");
    // 但「正文在不在」是有用的诊断，要留着
    assert!(e.contains("text=<非空>"), "丢了正文有无这个诊断位：{e}");
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

// ── 保留期 ───────────────────────────────────────────────────────────

/// 建一个装了若干月目录的 raw 区（每个月目录里放一个真月文件，好让删除是真的删数据）。
fn raw_months(name: &str, months: &[&str]) -> PathBuf {
    let root = testutil::fresh_root("ingest", name);
    for m in months {
        testutil::write_month(&root, m, "C", "R", &[row("m", ms(25, 9, 0), "u1")]);
    }
    root
}

fn month_dirs(root: &Path) -> HashSet<String> {
    std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// retention = 2 ⇒ 保留窗口最早月和它前一个月，更老的整个删掉。
#[test]
fn prune_keeps_the_window_month_and_one_before() {
    let root = raw_months("prune-2", &["202605", "202606", "202607", "202608"]);
    // 窗口在 202608（`all()` 是 8/25~8/29）
    assert_eq!(prune(&root, &all(), 2), 2);
    assert_eq!(
        month_dirs(&root),
        HashSet::from(["202607".into(), "202608".into()])
    );
}

/// retention = 1 ⇒ 只留窗口要读的月份。跨年往回退不能算错。
#[test]
fn prune_of_one_keeps_only_the_window_month() {
    let root = raw_months("prune-1", &["202512", "202601", "202608"]);
    assert_eq!(prune(&root, &all(), 1), 2);
    assert_eq!(month_dirs(&root), HashSet::from(["202608".into()]));
}

/// **本轮窗口要读的月份一个都不能删** —— 起点锚在 `w.since()` 而不是今天，
/// 所以 lookback 配到跨月也不会自伤。这条错了就是当天读不到数据。
#[test]
fn prune_never_touches_a_month_the_window_reads() {
    let w = Window::span(day(31), NaiveDate::from_ymd_opt(2026, 9, 2).unwrap());
    let root = raw_months("prune-window", &["202608", "202609"]);
    assert_eq!(prune(&root, &w, 1), 0);
    assert_eq!(months(&w).len(), 2, "这个窗口本来就该跨两个月");
    assert_eq!(
        month_dirs(&root),
        HashSet::from(["202608".into(), "202609".into()])
    );
}

/// 全仓唯一一处递归删生产数据的地方 —— **不长得像月目录的东西一律不碰**。
/// 门槛松一位，`raw_root` 底下手工放的任何东西都会在某天被静默删掉。
#[test]
fn prune_only_deletes_things_shaped_like_a_month_dir() {
    let root = raw_months("prune-shape", &["202601"]);
    std::fs::write(root.join("202602"), "我是文件不是目录").unwrap();
    std::fs::create_dir_all(root.join("notes")).unwrap();
    std::fs::create_dir_all(root.join("2026")).unwrap();
    std::fs::create_dir_all(root.join("20260a")).unwrap();

    assert_eq!(prune(&root, &all(), 2), 1, "只该删掉 202601");
    assert_eq!(
        month_dirs(&root),
        HashSet::from([
            "202602".into(),
            "notes".into(),
            "2026".into(),
            "20260a".into()
        ])
    );
}

/// 一次都没拉过 —— 目录不存在不是错误。
#[test]
fn prune_on_a_missing_root_is_a_no_op() {
    let root = testutil::fresh_root("ingest", "prune-missing");
    assert_eq!(prune(&root, &all(), 2), 0);
}

/// retention = 0 会把本轮要读的月份删掉 —— 配置错误当场崩，不静默降级成 1。
#[test]
#[should_panic(expected = "raw_retention_months")]
fn prune_of_zero_is_a_config_error() {
    prune(&testutil::fresh_root("ingest", "prune-zero"), &all(), 0);
}
