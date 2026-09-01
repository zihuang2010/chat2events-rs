//! 两张指标表的行类型，以及两个口径枚举。**只有形状，没有计算** —— 算法在
//! `compute.rs`。

use chrono::NaiveDate;

/// 抽取状态。**用枚举不用字符串** —— Python 那边要在函数口上手写
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
    /// 生产今天恒走默认口径 —— 切换入口（手动重算 CLI）还没搬（见 `docs/status.md` ⑥ 行）。
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
}
