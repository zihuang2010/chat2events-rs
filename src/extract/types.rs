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

/// 模型**被允许输出的全部东西**，就这四样：两个内容 + 两个控制。**线上形态。**
///
/// 其余 11 个字段由 ④ `assemble::assemble` 从真实消息算出，**一个都不采信模型**。
///
/// ⚠️ **`ref` 是字符串 `"E2"`，不是整数 2 —— 这是根治一个真实故障的类型选择。**
/// 曾经它是 `Option<u32>`，而 prompt 明说「如 E2 就填 2」：唯一能区分
/// 「便签编号」和「段内行号」的那个 `E` 被主动剥掉之后，`ref: 360` 和
/// `msg_indexes: [360]` 在 schema 上完全同型。实测后果是模型把行号当 ref 填
/// （一段 391 行、便签最大编号 102，它给了 E360 / E258 / E240）。
/// 现在行号在 ref 这个位置上**根本无法表达** —— 模型只能照抄一个 `E` 开头的记号。
#[derive(JsonSchema, Deserialize, Debug, Clone)]
pub(super) struct WireDraft {
    #[schemars(
        description = "接【进行中的事件】的编号，照抄 E 开头的整个记号（如 \"E2\"）；本段新出现的事件填 null。行号 #N 不是 ref"
    )]
    pub(super) r#ref: Option<String>,
    #[schemars(description = "本段内构成该事件的消息行号 #N")]
    pub(super) msg_indexes: Vec<usize>,
    #[schemars(description = "中文一句话摘要，≤100 字")]
    pub(super) summary: String,
    #[schemars(description = "这件事还没了结 = true")]
    pub(super) still_open: bool,
}

/// 校验**通过之后**的形态 —— `ref` 已经从 `"E2"` 解析成 `2`。
///
/// 线上形态与领域形态分开，是为了让「行号不能当 ref」这条由**类型**保证，
/// 而不是由一条事后校验保证。解析只发生在 `model::validate` 一处，
/// ④ `assemble` 拿到的永远是已经解析好的编号，不需要认识线上格式。
///
/// `r#ref` 是原始标识符 —— `ref` 是 Rust 关键字。
/// 外部适配器通过 [`EventDraft::new`] 构造，不能绕过校验或事后修改字段。
///
/// ```compile_fail
/// use chat2events_rs::extract::EventDraft;
/// let draft = EventDraft { r#ref: None, msg_indexes: vec![], summary: String::new(), still_open: false };
/// ```
///
/// ```compile_fail
/// use chat2events_rs::extract::EventDraft;
/// use std::collections::BTreeSet;
/// let mut draft = EventDraft::new(vec![1], "商家要求改期".into(), None, false, 1, &BTreeSet::new()).unwrap();
/// draft.msg_indexes.clear();
/// ```
#[derive(Debug, Clone)]
pub struct EventDraft {
    /// 接【进行中的事件】的编号；新事件是 `None`。
    pub(crate) r#ref: Option<u32>,
    /// 本段内构成该事件的消息行号 `#N`，已去重升序。
    pub(crate) msg_indexes: Vec<usize>,
    pub(crate) summary: String,
    pub(crate) still_open: bool,
}

impl EventDraft {
    /// 复用抽取的领域校验；段长与便签集合使用本次 SegmentModel::call 收到的值。
    pub fn new(
        msg_indexes: Vec<usize>,
        summary: String,
        reference: Option<u32>,
        still_open: bool,
        segment_size: usize,
        open_refs: &std::collections::BTreeSet<u32>,
    ) -> crate::Result<Self> {
        let wire = WireDraft {
            r#ref: reference.map(|r| format!("E{r}")),
            msg_indexes,
            summary,
            still_open,
        };
        let mut drafts = super::model::validate(vec![wire], segment_size, open_refs)
            .map_err(|e| crate::BoxError::from(e.to_string()))?;
        Ok(drafts.pop().expect("单条输入校验成功后必有一条结果"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn public_draft_construction_enforces_the_same_domain_rules() {
        let refs = BTreeSet::from([1]);
        let good = EventDraft::new(
            vec![2, 1, 2],
            "商家要求改期".into(),
            Some(1),
            true,
            2,
            &refs,
        )
        .unwrap();
        assert_eq!(good.msg_indexes, [1, 2]);
        for indexes in [vec![], vec![0], vec![3]] {
            assert!(
                EventDraft::new(indexes, "商家要求改期".into(), None, false, 2, &refs).is_err()
            );
        }
        assert!(EventDraft::new(vec![1], "商家要求改期".into(), Some(2), false, 2, &refs).is_err());
        let error = EventDraft::new(vec![1], "联系18472625055改期".into(), None, false, 2, &refs)
            .unwrap_err();
        assert!(error.to_string().contains("手机号"));
        assert!(!error.to_string().contains("18472625055"));
    }
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
