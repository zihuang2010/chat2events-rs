//! 从 [`Event`] 和 `Conversation.msg_counts` 算出指标行。**纯函数：零 IO、零 SQL、
//! 零 duckdb。**

use super::rows::{AgentRow, Attribution, GroupRow, Status};
use crate::{extract::Event, ingest::Role, window::Window};
use chrono::NaiveDate;
use std::collections::BTreeMap;

/// 用分位数不用均值：一条几小时才回的消息会把均值整个带偏。
fn pct(secs: &[u32], p: f64) -> Option<u32> {
    if secs.is_empty() {
        return None;
    }
    let i = ((secs.len() as f64 * p) as usize).min(secs.len() - 1);
    Some(secs[i])
}

/// `metric_group_daily` 的行。**纯函数：不读库不写库。**
///
/// `events = None` 表示这个群没算出来（`Failed`）。消息级指标两种情况都算得出来
/// （不依赖 LLM），所以失败的群照样有 `msg_count`。
///
/// **首响相关的三个数（`unreplied_count` / p50 / p90）只算商家发起的事件**
/// （`asker_role = EXTERNAL`）：平台发起的工单推送里 `first_agent_reply_time` 就是首条
/// 消息自己 —— 首响恒 0 秒、且永远算「已回复」，两个数会一起被带偏。分母单独出一列，
/// 否则 BI 只能拿 `event_count` 去除，得到的未回复率静默偏低。
pub fn group_rows(
    corp: &str,
    room: &str,
    days: &Window,
    msg_counts: &BTreeMap<NaiveDate, (usize, usize)>,
    events: Option<&[Event]>,
    status: Status,
) -> Vec<GroupRow> {
    let mut by_day: BTreeMap<NaiveDate, Vec<&Event>> = BTreeMap::new();
    for e in events.unwrap_or(&[]) {
        by_day.entry(e.occurred_on).or_default().push(e);
    }

    days.days()
        .iter()
        .map(|&dt| {
            let (n, senders) = msg_counts.get(&dt).copied().unwrap_or((0, 0));
            let base = GroupRow {
                corp: corp.into(),
                room: room.into(),
                dt,
                msg_count: n as u32,
                sender_count: senders as u32,
                event_count: None,
                merchant_event_count: None,
                unreplied_count: None,
                first_reply_p50_sec: None,
                first_reply_p90_sec: None,
                status,
            };
            // Failed -> 事件级全部 NULL，**不是 0**（承重不变量 4）
            if events.is_none() {
                return base;
            }

            let evs = by_day.get(&dt).map(Vec::as_slice).unwrap_or(&[]);
            let asked: Vec<&&Event> = evs
                .iter()
                .filter(|e| e.asker_role == Role::External)
                .collect();
            let mut secs: Vec<u32> = asked
                .iter()
                .filter_map(|e| {
                    e.first_agent_reply_time
                        .map(|t| (t - e.first_msg_time).num_seconds().max(0) as u32)
                })
                .collect();
            secs.sort_unstable();
            GroupRow {
                event_count: Some(evs.len() as u32),
                merchant_event_count: Some(asked.len() as u32),
                unreplied_count: Some((asked.len() - secs.len()) as u32),
                first_reply_p50_sec: pct(&secs, 0.5),
                first_reply_p90_sec: pct(&secs, 0.9),
                ..base
            }
        })
        .collect()
}

/// `metric_agent_daily` 的行 —— 六列语义键 + 处理量。**纯函数。**
///
/// `types[i]` 是 `events[i]` 的标签，由 `daily` 在**事务外**算好传进来（⑤ 的注释说明
/// 了为什么不在这里调）。⑥ 和 ⑦ 共用同一份，同一个 event 的 type 不会算两遍。
///
/// **不单独存总量行** —— 总量 = 求和。存两处会打架。
///
/// 失败的群传空 `events`，于是**一行都没有** —— 承重不变量 5：那张表上失败的群是
/// 整行缺失，不是 0。
pub fn agent_rows(
    corp: &str,
    room: &str,
    events: &[Event],
    types: &[&str],
    taxonomy_version: &str,
    attribution: Attribution,
) -> Vec<AgentRow> {
    assert_eq!(
        events.len(),
        types.len(),
        "types 必须与 events 一一对应 —— 错位会把标签安到别的事件头上（构造保证，调用方拉齐）"
    );
    let mut counts: BTreeMap<(String, NaiveDate, String), u32> = BTreeMap::new();
    for (e, &t) in events.iter().zip(types) {
        let who: Vec<&String> = match attribution {
            Attribution::FirstResponder => e.first_responder.iter().collect(),
            Attribution::AllParticipants => e.agents.iter().collect(),
        };
        for a in who {
            // 无平台回复的事件在 first_responder 口径下不计入任何人
            *counts
                .entry((a.clone(), e.occurred_on, t.to_string()))
                .or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|((agent, dt, event_type), event_count)| AgentRow {
            corp: corp.into(),
            room: room.into(),
            agent,
            dt,
            event_type,
            taxonomy_version: taxonomy_version.into(),
            event_count,
        })
        .collect()
}
