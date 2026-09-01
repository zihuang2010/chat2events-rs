//! ③ 抽取 extract ＋ ④ 装配 assemble —— 把会话变成结构化事件。
//!
//! ```text
//! Conversation ──切成 ceil(n / segment_msgs) 段──> 段1 →📝→ 段2 →📝→ 段K
//!                                                （全群共用一套 drafts，零接缝）
//!                                                          │
//!                                            EventDraft（模型只说四样）
//!                                                          ▼
//!                                          ④ assemble ── 11 个字段从真实消息算
//!                                                          ▼
//!                                                        Event
//! ```
//!
//! **端口是 [`SegmentModel`]**（③ 的真接缝）：换模型 / 换端点只改 [`LiveModel`]，
//! 二分逻辑一行不动。**④ [`assemble`] 明确不给端口** —— 它是承重不变量 6（溯源）
//! 的守卫，给它接缝等于给溯源留绕过口。
//!
//! 对外只有三样：[`Event`] · [`SegmentModel`] · [`extract`]。以下全是内部行为，
//! 不上浮到接口上：自适应二分 · 段间便签 · 序号↔`msg_id` 映射 · 正文脱敏 ·
//! schema 校验 · 溯源校验 · 重试。
//!
//! **模型根本不接触 `msg_id`**（承重不变量 6）：prompt 里给的是段内 1-based 序号，
//! 代码映射回 `msg_id`（唯一发生地是 [`merge`]）。序号越界直接是校验失败 ——
//! 这从根本上消灭了「模型编造 msgid」这个失败模式。
//!
//! 论证与实测数字在 **ADR-0001**（正文脱敏）· **ADR-0002**（`replyTo` 对齐）·
//! **ADR-0004**（串行链传便签、自适应二分）。搬运自 `../pychat2events/src/extract.py`。
//!
//! **文件布局**（拆成目录只为导航 —— 接口一字未动，对外仍然只有上面那三样）：
//!
//! ```text
//! extract/
//!   mod.rs       领域类型 · 端口 SegmentModel · 校验 · LiveModel · 调用链
//!   redact.rs    正文脱敏与订单号正则（ADR-0001）
//!   prompt.rs    SYSTEM —— 逐字搬运，改一个字所有实测结论作废
//!   render.rs    便签 · 匿名标签 · 行号箭头，view 是唯一出口
//!   segment.rs   分段与切点选择（ADR-0004）
//!   assemble.rs  ④ merge / align / assemble / orphans
//! ```

mod assemble;
mod prompt;
mod redact;
mod render;
mod segment;

// 子模块的东西在这里落一次名，调用点就跟拆分前一模一样（`assemble(d, msgs)` 而不是
// `assemble::assemble(d, msgs)`）—— 模块名在类型空间、函数名在值空间，不冲突。
use assemble::{align, assemble, merge, orphans};
use prompt::SYSTEM;
use redact::{ORDER_NO, PLACEHOLDER, first_phone};
use render::view;
use segment::{cut, segments};

use crate::{
    BoxError,
    ingest::{Message, Role},
    llm::{Llm, LlmError, Turn},
};
use chrono::{NaiveDate, NaiveDateTime};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    pin::Pin,
};

/// 允许一次「序号越界」的自我修正，不多给 —— 逼急了模型会编一个合法序号。
const MAX_RETRIES: u32 = 1;

/// 便签里带的原话截断长度（字符数，不是字节）。
const NOTE_QUOTE: usize = 40;

/// `summary` 契约上限，**按 Unicode 码点算**不是字节。
const SUMMARY_MAX: usize = 100;

// ─────────────────────────────────────────────────────────────────────────────
// 领域类型
// ─────────────────────────────────────────────────────────────────────────────

/// 模型**被允许输出的全部东西**，就这四样：两个内容 + 两个控制。
///
/// 其余 11 个字段由 ④ [`assemble`] 从真实消息算出，**一个都不采信模型**。
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

/// 端口上的出口类型。空列表合法 —— 这一段确实没有业务事件。
#[derive(JsonSchema, Deserialize, Debug)]
pub struct SegmentExtraction {
    pub events: Vec<EventDraft>,
}

/// 跨段累积的事件草稿。**`idx` 是【全局】消息下标，不是段内行号。**
#[derive(Debug, Clone, Default)]
struct Draft {
    idx: Vec<usize>,
    summary: String,
    still_open: bool,
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

// ─────────────────────────────────────────────────────────────────────────────
// 端口 SegmentModel —— 「把一段交给模型，拿回校验通过的结果」这一件事
// ─────────────────────────────────────────────────────────────────────────────

/// 一次段调用的失败。**两类的处置完全不同**，所以在类型上分开。
#[derive(Debug)]
pub enum SegError {
    /// 这一段模型吃不下 —— **切**（ADR-0004）。
    ///
    /// 「什么信号算太大」是端点知识、归适配器；「太大就切」跟谁家端点无关、归 [`run`]。
    TooBig(String),
    /// 其余全部 —— 不切，该群本日失败。**连接类错误在这里**：网络断了切成两半也
    /// 一样断，把它当「太大」会让一次故障放大成一整棵调用树。
    Failed(BoxError),
}

impl fmt::Display for SegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooBig(m) => f.write_str(m),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SegError {}

/// 把渲染好的一段交给模型，拿回**校验通过**的结果。
///
/// `segment_size` / `open_refs` 是校验用的上下文：越界序号和未知 ref 的报错原文
/// 会回灌进下一轮 prompt（[`MAX_RETRIES`]）。
///
/// 这是③的**真接缝**：生产的 [`LiveModel`] 和测试里的 `BisectStub` 两个适配器。
/// 换模型 / 换端点只改 `LiveModel`，[`run`] 的二分逻辑一行不动。
pub trait SegmentModel {
    fn call(
        &self,
        text: &str,
        segment_size: usize,
        open_refs: &BTreeSet<u32>,
    ) -> impl Future<Output = Result<Vec<EventDraft>, SegError>> + Send;
}

/// 校验模型这一段的输出。**不通过 = 该批次失败**，不做字段级兜底修补。
///
/// 报错文案**逐字搬自 Python 版** —— 它不是给人看的，是回灌进下一轮 prompt 给模型
/// 读的，模型要照着它自我修正。改文案等于改 prompt。
///
/// 三条规则各自的理由：
///   * **序号越界** —— 承重不变量 6 的守卫。模型看不到 `msg_id`，只看到段内序号，
///     越界即编造。顺带 `sorted(set(v))`：**去重 + 排序是契约不是顺手**。
///   * **ref 未知** —— 便签上没有的 ref 接不上任何 draft，放行就会凭空造一个。
///   * **summary 四条** —— 它归事实列，冻结区不可写，且 `sha256(summary)` 是 ⑤ 的
///     缓存键。**PII 一旦进去就是永久的，缓存还会把它焊死**，所以挡在这里，
///     不做落库前 scrub（那会改内容、让缓存键漂掉）。
fn validate(
    mut events: Vec<EventDraft>,
    segment_size: usize,
    open_refs: &BTreeSet<u32>,
) -> Result<Vec<EventDraft>, String> {
    let mut errs: Vec<String> = Vec::new();
    for e in &mut events {
        let bad: Vec<usize> = e
            .msg_indexes
            .iter()
            .copied()
            .filter(|i| !(1..=segment_size).contains(i))
            .collect();
        if bad.is_empty() {
            // 去重 + 排序是契约
            e.msg_indexes.sort_unstable();
            e.msg_indexes.dedup();
            if e.msg_indexes.is_empty() {
                errs.push("msg_indexes 不能为空".into());
            }
        } else {
            let list = bad
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            errs.push(format!("序号 [{list}] 超出本段范围 1-{segment_size}"));
        }

        if let Some(r) = e.r#ref
            && !open_refs.contains(&r)
        {
            let have = if open_refs.is_empty() {
                "（空）".to_string()
            } else {
                format!(
                    "[{}]",
                    open_refs
                        .iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            errs.push(format!(
                "E{r} 不在【进行中的事件】里，现有的是 {have}；本段新出现的事件请把 ref 填成 null"
            ));
        }

        let n = e.summary.chars().count();
        if n > SUMMARY_MAX {
            errs.push(format!("summary 长度 {n} 超过 {SUMMARY_MAX} 字，请压缩"));
        }
        if let Some(m) = ORDER_NO.find(&e.summary) {
            errs.push(format!(
                "summary 不得含订单号「{}」，只描述发生了什么",
                m.as_str()
            ));
        }
        if let Some(p) = first_phone(&e.summary) {
            errs.push(format!("summary 不得含手机号「{p}」"));
        }
        if let Some(m) = PLACEHOLDER.find(&e.summary) {
            errs.push(format!(
                "summary 不得含占位符「{}」—— 它是脱敏留下的记号，不是内容。\
                 改成「客户」「师傅」这样的角色词",
                m.as_str()
            ));
        }
    }
    if errs.is_empty() {
        Ok(events)
    } else {
        Err(errs.join("\n"))
    }
}

/// 真实调用。**端点知识全都住在这里** —— 换端点要改的就是这个类型。
pub struct LiveModel {
    llm: Llm,
}

impl LiveModel {
    pub fn new(llm: Llm) -> Self {
        Self { llm }
    }
}

impl SegmentModel for LiveModel {
    async fn call(
        &self,
        text: &str,
        segment_size: usize,
        open_refs: &BTreeSet<u32>,
    ) -> Result<Vec<EventDraft>, SegError> {
        let mut turns = vec![Turn::User(text.to_string())];
        let mut attempt = 0u32;
        loop {
            let got: crate::llm::Extracted<SegmentExtraction> =
                match self.llm.extract(SYSTEM, &turns).await {
                    Ok(v) => v,
                    // **端点知识 -> 切分信号的翻译就这两行。** 只认这两个：
                    // 截断（输出预算耗尽）和超时（连上了但这一段没算完）。
                    // `Other` 里含连接类错误，**绝不当成「太大」**（ADR-0004）。
                    Err(LlmError::Truncated) => {
                        return Err(SegError::TooBig("输出预算耗尽".into()));
                    }
                    Err(LlmError::Timeout) => {
                        return Err(SegError::TooBig("请求超时".into()));
                    }
                    Err(e) => return Err(SegError::Failed(Box::new(e))),
                };

            match validate(got.data.events, segment_size, open_refs) {
                Ok(events) => return Ok(events),
                Err(msg) if attempt < MAX_RETRIES => {
                    // 静默重试等于不知道模型在编序号。这条 warn 是唯一的信号。
                    tracing::warn!(segment_size, attempt, "模型输出没过校验，回灌报错重问：{msg}");
                    turns.push(Turn::Assistant(got.raw));
                    turns.push(Turn::User(format!(
                        "上一轮的输出没通过校验：\n{msg}\n\n请按上面的报错修正，重新输出全部事件。"
                    )));
                    attempt += 1;
                }
                // 次数用完 -> 该批次失败，不做字段级兜底修补、不落库半个事件。
                Err(msg) => {
                    return Err(SegError::Failed(
                        format!("校验重试 {MAX_RETRIES} 次后仍不通过：{msg}").into(),
                    ));
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 调用链
// ─────────────────────────────────────────────────────────────────────────────

/// 把 `msgs[lo..hi]` 交给模型，就地更新 `drafts`。校验通过才写回。
///
/// 只剩三步：建视图（[`view`]）/ 发请求 / 合并（[`merge`]）。
async fn one_call<M: SegmentModel + Sync>(
    model: &M,
    msgs: &[Message],
    lo: usize,
    hi: usize,
    drafts: &mut BTreeMap<u32, Draft>,
    segment_msgs: usize,
) -> Result<(), SegError> {
    let (text, open_refs) = view(msgs, lo, hi, drafts, segment_msgs);
    let events = model.call(&text, hi - lo, &open_refs).await?;
    merge(drafts, events, lo);
    Ok(())
}

/// 整段先试；模型吃不下就对半切，两半**按时间顺序、串行**跑。切不动了就抛出去。
///
/// 「是否切分」是运行时看信号决定的，**没有阈值参数**（ADR-0004）。两半共用同一份
/// `drafts` 且串行 —— **二分不产生接缝**。
///
/// 递归的 async 要装箱：Rust 的 `async fn` 不能直接自递归（future 大小无法确定）。
fn run<'a, M: SegmentModel + Sync>(
    model: &'a M,
    msgs: &'a [Message],
    lo: usize,
    hi: usize,
    drafts: &'a mut BTreeMap<u32, Draft>,
    segment_msgs: usize,
) -> Pin<Box<dyn Future<Output = Result<(), SegError>> + Send + 'a>> {
    Box::pin(async move {
        match one_call(model, msgs, lo, hi, drafts, segment_msgs).await {
            Err(SegError::TooBig(reason)) => {
                if hi - lo < 2 {
                    // 剩一条仍失败 —— 切不动了，该群本日失败
                    return Err(SegError::TooBig(reason));
                }
                let mid = cut(msgs, lo, hi, (lo + hi) / 2, hi - lo);
                // 二分不能是静默的：不打出来就没人知道某个群天天在被切
                tracing::warn!(n = hi - lo, lo, mid, hi, "[切分] {reason}");
                run(model, msgs, lo, mid, drafts, segment_msgs).await?;
                run(model, msgs, mid, hi, drafts, segment_msgs).await
            }
            other => other,
        }
    })
}

/// ③④ 的出口。成功返回 `Vec<Event>`（**可能为空 = 这天确实没有业务事件**）。
///
/// 全群一条串行链：段与段之间传便签，共用一套 `drafts` —— **群内零接缝**。段的起始长度
/// 由 `segment_msgs` 定，模型吃不下就在段内自适应二分。
///
/// 并行加在**群与群之间**，不加在段之间：段必须串行（后一段要看前一段的便签），
/// 在段之间强行并行等于每个边界丢一个接缝 —— 实测（167 条 / 43 个真实事件）切一刀就
/// 切开 5 个事件。那是拿准确率换单群延迟，只有「一次只跑一个群」才划算，跑批不是这个场景。
///
/// **不返回半个结果** —— 任何失败直接 `Err`，调用方靠它做群级失败隔离（承重不变量 3）。
/// `Ok(vec![])` 与 `Err` **绝不混淆**（承重不变量 4）。
pub async fn extract<M: SegmentModel + Sync>(
    msgs: &[Message],
    model: &M,
    segment_msgs: usize,
) -> Result<Vec<Event>, BoxError> {
    if msgs.is_empty() {
        return Ok(Vec::new());
    }
    let segs = segments(msgs, segment_msgs);
    if segs.len() > 1 {
        tracing::info!(msgs = msgs.len(), segments = segs.len(), "[分段] 串行传便签");
    }
    // 全群一套 drafts —— ref 编号全局唯一，便签跨段流动
    let mut drafts: BTreeMap<u32, Draft> = BTreeMap::new();
    for (lo, hi) in segs {
        run(model, msgs, lo, hi, &mut drafts, segment_msgs).await?;
    }
    // 最后统一对齐：便签已经跑完，这里只修最终输出，不回头影响段内流程。
    drafts.retain(|_, d| !d.idx.is_empty());
    let drafts = align(drafts, msgs);
    orphans(&drafts, msgs);
    drafts.values().map(|d| assemble(d, msgs)).collect()
}

/// 对拍 / `--dry` 用：按**真实分段**逐段渲染，走的是和生产同一条 [`view`]。
///
/// 便签为空 —— 它只有跑过模型才有内容，第一段本来就是空的，后面几段无从预测。
/// 段内二分也是运行时看输出预算才决定的，这里同样不预测。
pub fn preview(msgs: &[Message], segment_msgs: usize) -> String {
    let empty = BTreeMap::new();
    segments(msgs, segment_msgs)
        .into_iter()
        .map(|(lo, hi)| {
            format!(
                "===== 段 {lo}:{hi} =====\n{}",
                view(msgs, lo, hi, &empty, segment_msgs).0
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests;
