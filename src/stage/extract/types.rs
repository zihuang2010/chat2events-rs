//! ③④ 的领域类型 —— 模型被允许说的四样（[`EventDraft`]）、跨段累积的草稿
//! （[`Draft`]）、④ 的出口（[`Event`]）。

use crate::stage::ingest::Role;
use chrono::{NaiveDate, NaiveDateTime};
use schemars::JsonSchema;
use serde::Deserialize;

/// `summary` 的**质量上限**，按 Unicode 码点算不是字节。
///
/// ⚠️ **它是软的 —— 超了只 warn 加重问一次，不整群作废。** 硬的是
/// [`SUMMARY_COLUMN`]。此前两处（`model::validate` 与 `assemble`）都拿它当硬闸，
/// 于是「模型写了 101 字」和「summary 里混进手机号」同等处罚：整群整日 0 事件。
/// 实测一轮 11 个失败群里有 2 个是这么废的，而 `VARCHAR(200)` 明明装得下。
///
/// prompt / schemars description / `schema.sql` 里的「100」由
/// `the_prompt_the_schema_and_summary_max_agree` 钉住一致 —— 契约照旧写 100，
/// 变的只是超了之后怎么办。
pub(super) const SUMMARY_MAX: usize = 100;

/// `summary` 的**存储上限** —— `schema.sql` 的 `VARCHAR(200)`，唯一的硬约束。
///
/// 超过它就真的写不进去（落库当场报错，而那是群级失败）。闸在两处：`model::validate`
/// 把它归**规模相关**（重问一次不过就切小再试 —— 揉出一条 589 字 summary 的段本来
/// 就太长），`assemble` 是最后一道。100 和 200 之间是「难看但能用」，放行。
pub(super) const SUMMARY_COLUMN: usize = 200;

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
/// use chat2events_rs::stage::extract::EventDraft;
/// let draft = EventDraft { r#ref: None, msg_indexes: vec![], summary: String::new(), still_open: false };
/// ```
///
/// ```compile_fail
/// use chat2events_rs::stage::extract::EventDraft;
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
        // 软规则（超长）在这里直接放行 —— 它的处置是「重问一次再放行」，而这个
        // 同步构造器没有模型可重问，生产路径最终也是放行。占位符照样被抹掉。
        let mut checked = super::model::validate(vec![wire], segment_size, open_refs)
            .map_err(|e| crate::BoxError::from(e.to_string()))?;
        Ok(checked
            .events
            .pop()
            .expect("单条输入校验成功后必有一条结果"))
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
    /// 末条来源消息发送方的角色 —— **`asker_role` 的镜像**：同一个 `src`，取末条而非
    /// 首条。`External` = 商家说完没人接（把人晾着），`Internal` = 客服收的尾。
    /// 和 `first_agent_reply_time` 一样是确定性计算，**不经模型**。
    ///
    /// `Option` 的唯一来源是**加这一列之前抽取的历史行** —— 冻结区不可回填。
    /// `None` = 没算过，**不是某一边**（承重不变量 4 的形状）；`assemble` 产出的恒为 `Some`。
    pub last_msg_role: Option<Role>,
    /// 后续轮次最长等待的**工作时段秒数** —— 首响之后每次 EXTERNAL→INTERNAL 取最大。
    ///
    /// 和 `last_msg_role` 同源同形：确定性计算不经模型，`Option` 的唯一来源是加这一列
    /// 之前抽取的历史行（冻结区不可回填），`assemble` 产出的恒为 `Some`。
    ///
    /// ⚠️ **`Some(0)` 和 `None` 是两件事**：前者 = 确实没有后续轮次（算出来的事实），
    /// 后者 = 没算过（承重不变量 4）。
    ///
    /// ⚠️ 工作时段口径（`[08:30, 21:00)`，周末节假日不扣）住在 [`crate::worktime`]，
    /// **首响时效走同一份定义**。它曾经是全站唯一走工作时段的列。
    ///
    /// 两者的**可改性仍然不同**，这条没变：首响由两个时间列在查询期现算，换口径
    /// 重算一遍就行；本列查询期拿不到中间轮次，只能在这里算，**口径在写入那一刻
    /// 就定死了** —— 而且 max 取的是哪一轮会随口径变，事后换不回来。
    pub followup_wait_max_sec: Option<u32>,
    /// 来源消息的渲染快照 —— **展示列，既不是事实列也不是标注列。**
    ///
    /// 和 `source_msg_ids` 等长同序（`assemble` 里由同一个 `src` 生成，构造上恒真）。
    ///
    /// ⚠️ **`store::read_events` 读回来的 `Event` 这一项恒为空。** 那条路径服务的是
    /// ⑤ 打标和 `recompute` 重打标，它们只要 `summary` —— 把正文一起读回来，
    /// `recompute` 的全历史扫描就会把整个正文语料拉进内存。所以它**不在
    /// `EVENT_FACT_COLS` 里**，`read_events` 的列表正是从那个常量派生的。
    /// 空 `Vec` 在这里只意味着「没读」，不意味着「这个事件没有来源消息」——
    /// 承重不变量 6 保证后者不可能发生。
    pub source_messages: Vec<SourceMessage>,
}

/// 一条来源消息在 webUI 上的样子。**字段与下钻接口的返回体逐字相同** ——
/// `web` 那边直接把这一列的字符串原样回给前端，不做任何转换，
/// 所以这里改一个键名就是改一次前端契约（`webui/src/domain/schemas.ts`）。
///
/// ⚠️ **不存 `msg_type` / `reply_to` / 媒体 URL。** 前者的唯一用途（判占位符）
/// 在写入时就用掉了，`text` 里已经是 `[图片]`；后两者今天没有渲染点。
/// 端口上每多一个死字段都是收税，这条规矩对落库的展示列同样成立。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceMessage {
    pub msg_id: String,
    /// `%Y-%m-%d %H:%M:%S`，UTC+8 墙钟 —— 与 `EVENT_SELECT` 里那几个
    /// `DATE_FORMAT` 同一个形态，前端拿到的所有时间长一个样。
    pub at: String,
    pub sender_id: String,
    /// `INTERNAL` / `EXTERNAL`，即 [`Role::as_str`]。
    pub sender_role: &'static str,
    /// 文本消息是正文；非文本消息是 [`crate::stage::ingest::Message::placeholder`] 的占位符。
    pub text: String,
}
