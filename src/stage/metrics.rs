//! ⑥ 指标 metrics —— 三张指标表的行的唯一来源（写库 SQL 在 ⑦ `store`）。
//!
//! **纯函数模块：零 IO、零 SQL、零 duckdb。** 两个来源 —— **事件级**指标读 [`crate::stage::extract::Event`]，
//! **消息级**指标读 `Conversation.msg_counts` / `.agent_msg_counts`（搭 ① 的同一趟车算好的）。
//!
//! 指标表**不受分片冻结约束** —— 它依赖的事实全都还在，随时可整体重算。所以「换归属
//! 口径」「词表升版重打标」都不用重跑 LLM。
//!
//! 行类型和算出它们的函数住在同一个文件：行类型上那些注释（「下面三个数的分母是
//! `merchant_event_count` 不是 `event_count`」「失败时全 `None` 不是 0」）说的全是
//! 那几个函数的行为，隔一个文件放就要来回翻。
//!
//! **⑥ 是单文件模块，没有 `mod.rs`** —— 这是第二次合并了。行类型先从 `rows.rs`
//! 合回 `compute.rs`（73 行里 40 行在描述算法，读的人两边来回翻），于是目录里
//! 只剩一个生产孩子，那一层不再换任何东西、只留下一跳，`mod.rs` 退化成纯转发。
//! 测试块 306 行 ≥ 100，按 `lib.rs` 的规则留在 `metrics/tests.rs`。

use crate::{stage::extract::Event, stage::ingest::Role, window::Window, worktime};
use chrono::NaiveDate;
use std::collections::BTreeMap;

// ─────────────────────────────────────────────────────────────────────────────
// 三张表的行 + 两个口径枚举 —— 只有形状
// ─────────────────────────────────────────────────────────────────────────────

/// 抽取状态。**用枚举不用字符串** —— 裸字符串要在函数口上手写
/// `if status not in ("ok","failed"): raise`，这里由类型兜住。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 算出来了（**可能是 0 个事件**，那也是 `Ok`）。
    Ok,
    /// 没算出来。事件级指标全部 `NULL`。
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// 归属口径。**这个开关只作用于本模块，不作用于 ③ 抽取** —— 事实全存
/// （`Event.agents` 存了涉及的全部 INTERNAL 成员），解释随时可重算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Attribution {
    #[default]
    FirstResponder,
    /// 生产今天恒走默认口径 —— 切换入口（手动重算 CLI）还没做。
    /// 这个变体由测试钉着「事实全存、口径可换、不重跑 LLM」，允许 dead_code 而不是删掉它。
    #[allow(dead_code)]
    AllParticipants,
}

/// `b_merchant_group_metric_daily` 的一行。
///
/// **`Ok([])` 与 `Failed` 绝不混淆**（承重不变量 4）：五个事件级字段是 `Option`，
/// `Failed` 时全 `None`，`Ok([])` 时是 `Some(0)`。**绝不用 0 表示「没算出来」。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRow {
    pub corp: String,
    pub room: String,
    pub dt: NaiveDate,
    /// 消息级 —— **不依赖抽取，失败的群照样有**。
    pub msg_count: u32,
    pub sender_count: u32,
    pub event_count: Option<u32>,
    /// `asker_role = EXTERNAL` 的事件数。**下面三个数的分母就是它，不是
    /// `event_count`** —— 用 `event_count` 当分母会得到一个偏低但看起来正常的未回复率。
    pub merchant_event_count: Option<u32>,
    pub unreplied_count: Option<u32>,
    pub first_reply_p50_sec: Option<u32>,
    pub first_reply_p90_sec: Option<u32>,
    pub status: Status,
}

/// `b_merchant_group_agent_metric_daily` 的一行。语义键是前六列。
///
/// `room` 进键是承重的：**键必须嵌套在「群 × 日」的失败隔离粒度里**。没有它，小明在
/// A 群和 B 群都干了活、B 群抽取失败被跳过时，他当天那一行会被只含 A 群的数字覆盖 ——
/// 残缺覆盖完整。跨群总量查询时 `SUM`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub corp: String,
    pub room: String,
    pub agent: String,
    pub dt: NaiveDate,
    pub event_type: String,
    pub taxonomy_version: String,
    pub event_count: u32,
    /// 平台客服账号，跑批从消息元信息补入；重打标保留库中已有值。
    pub official_user_id: Option<String>,
}

/// `b_merchant_group_agent_msg_daily` 的一行。语义键是全部四列。
///
/// **和 [`AgentRow`] 是两张表，不是一张表上的一列。** 三条理由，缺一条都不够：
///   * [`AgentRow`] 的语义键含 `event_type`，而消息数不随类型变 —— 挂上去同一个
///     数字要在 N 个类型行里各存一遍，BI 直连 `SUM` 会按类型数放大，且看起来正常。
///   * [`AgentRow`] 按 [`Attribution::FirstResponder`] 归属：发了 200 条却一次首响
///     都没抢到的客服在那张表上**一行都没有** —— 而这一列的用途正是看这种人。
///   * 生命周期对不上：那张表由打标阶段整段删重写、抽取失败时整行缺失、词表升版
///     再重写一遍；消息数**既不依赖抽取也不依赖词表**，抽取失败的群照样算得出来。
///
/// 所以它由**事实阶段**写（`store::write_room` 的同一个事务），
/// 打标与重打标一个字节都不碰它，也没有 `taxonomy_version`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMsgRow {
    pub corp: String,
    pub room: String,
    pub agent: String,
    pub dt: NaiveDate,
    /// 该客服该日在该群发的消息条数。**只数 `Role::Internal`**（由 ① 的
    /// `agent_counts` 保证），商家侧那半边在 [`GroupRow::msg_count`] 里。
    pub msg_count: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// 从事实算出那些行 —— 纯函数
// ─────────────────────────────────────────────────────────────────────────────

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
/// **p50 / p90 是工作时段秒数**（`worktime`，`[08:30, 21:00)`，周末节假日不扣），
/// 不是墙钟差。⚠️ 改这条口径会让这两列的**历史行与新行含义不同** —— 迁移 SQL 在
/// `docs/deploy.md`「首响口径改为工作时段」，那一步不重跑模型。
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
            // 首响时效走**工作时段口径**（`worktime`），不是墙钟差 —— 夜里 23:00 进来的
            // 消息次日 09:00 回，算的是 30 分钟而不是 10 小时。这条和
            // `followup_wait_max_sec`、webUI 的查询期现算是同一份定义。
            let mut secs: Vec<u32> = asked
                .iter()
                .filter_map(|e| {
                    e.first_agent_reply_time
                        .map(|t| worktime::between(e.first_msg_time, t) as u32)
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
            official_user_id: None,
        })
        .collect()
}

/// `metric_agent_msg_daily` 的行 —— 四列语义键 + 消息数。**纯函数。**
///
/// 输入是 ① 搭同一趟车算好的 `Conversation.agent_msg_counts`（已经只剩 INTERNAL），
/// 这里只负责补上 `corp` / `room` 摊成行。
///
/// **没有 `status` 参数，也不该有** —— 消息数不依赖抽取（承重不变量 4 管的是
/// 事件级列）。抽取失败的群这张表照写，那正是它的用处：模型挂了，「谁在群里说话」
/// 这件事仍然是已知的。
pub fn agent_msg_rows(
    corp: &str,
    room: &str,
    agent_msg_counts: &BTreeMap<(NaiveDate, String), usize>,
) -> Vec<AgentMsgRow> {
    agent_msg_counts
        .iter()
        .map(|((dt, agent), n)| AgentMsgRow {
            corp: corp.into(),
            room: room.into(),
            agent: agent.clone(),
            dt: *dt,
            msg_count: *n as u32,
        })
        .collect()
}

#[cfg(test)]
mod tests;
