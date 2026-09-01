//! ③④ 的领域类型 —— 模型被允许说的四样（[`EventDraft`]）、跨段累积的草稿
//! （[`Draft`]）、④ 的出口（[`Event`]）。

use crate::ingest::Role;
use chrono::{NaiveDate, NaiveDateTime};
use schemars::JsonSchema;
use serde::Deserialize;

/// `summary` 契约上限，**按 Unicode 码点算**不是字节。两个消费点（`model::validate`
/// 与 `assemble` 的双保险）跨子模块，所以住在这里而不是任何一个消费点里；
/// prompt / schemars description / `schema.sql` 里的「100」由
/// `the_prompt_the_schema_and_summary_max_agree` 钉住一致。
pub(super) const SUMMARY_MAX: usize = 100;

// ─────────────────────────────────────────────────────────────────────────────
// 领域类型
// ─────────────────────────────────────────────────────────────────────────────

/// 模型**被允许输出的全部东西**，就这四样：两个内容 + 两个控制。
///
/// 其余 11 个字段由 ④ `assemble::assemble` 从真实消息算出，**一个都不采信模型**。
#[derive(JsonSchema, Deserialize, Debug, Clone)]
pub struct EventDraft {
    /// 接【进行中的事件】的编号；新事件填 null。
    ///
    /// `r#ref` 是原始标识符 —— `ref` 是 Rust 关键字，但 serde / schemars 都按 `ref`
    /// 出面，与 Python 版的字段名一致。
    #[schemars(description = "接【进行中的事件】的编号；新事件填 null")]
    pub r#ref: Option<u32>,
    /// 本段内构成该事件的消息行号 `#N`。
    #[schemars(description = "本段内构成该事件的消息行号 #N")]
    pub msg_indexes: Vec<usize>,
    #[schemars(description = "中文一句话摘要，≤100 字")]
    pub summary: String,
    #[schemars(description = "这件事还没了结 = true")]
    pub still_open: bool,
}

/// 跨段累积的事件草稿。**`idx` 是【全局】消息下标，不是段内行号。**
///
/// **不变量：`idx` 恒非空且升序。** `validate` 拒绝空 `msg_indexes`，唯一的生产者
/// [`super::assemble::merge`] 在写入处断言 —— `render::note` 的 `expect`、`assemble` 里 `align` /
/// `orphans` 的裸下标全依赖这一条，不再各自防御。
#[derive(Debug, Clone, Default)]
pub(super) struct Draft {
    pub(super) idx: Vec<usize>,
    pub(super) summary: String,
    pub(super) still_open: bool,
}

/// ④ 的出口。**只有事实列。**
///
/// 标注列（`event_type` / `taxonomy_version`）**不在这里** —— 它们由 ⑤ 每次落库时
/// 现算（包括分片删重写那一次），所以分片重写不会丢标签。放进这个结构体就成了第二个
/// 真相来源：有人读 `e.event_type` 拿到抽取那一刻的常量，而库里已经重打过标。
///
/// **时间一律取自来源消息的真实时间戳**，不采信模型自己写的时间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub corpid: String,
    pub roomid: String,
    /// 非空；每个 ID 必须真实存在于该次抽取的消息里（承重不变量 6）。
    pub source_msg_ids: Vec<String>,
    pub first_msg_time: NaiveDateTime,
    pub last_msg_time: NaiveDateTime,
    /// 首条 `INTERNAL` 来源消息时间，可空 —— **首响锚点**。
    pub first_agent_reply_time: Option<NaiveDateTime>,
    /// `= date(first_msg_time)`，报表归属日 / 幂等分片键。
    pub occurred_on: NaiveDate,
    pub asker: String,
    /// `External` = 商家发起 / `Internal` = 平台发起（工单推送类）。
    pub asker_role: Role,
    /// 涉及的全部 `INTERNAL` 成员，全存 —— 换归属口径不用重跑 LLM。
    pub agents: Vec<String>,
    pub first_responder: Option<String>,
    /// **唯一一个来自模型的字段。** 归事实列。
    pub summary: String,
}
