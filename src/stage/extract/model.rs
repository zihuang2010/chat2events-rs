//! 端口 [`SegmentModel`] ＋ 生产适配器 [`LiveModel`] ＋ 校验 [`validate`]
//! —— 「把一段交给模型，拿回**校验通过**的结果」这一件事。
//!
//! ③ 的**真接缝**就在这里：换模型 / 换端点只改本文件，`super` 里那套分段与自适应
//! 二分一行不动。端点知识（什么信号算「这一段太大」、schema 长什么样、重问几次）
//! 全部收在本文件内，不上浮到调用链上。

use super::{
    prompt::SYSTEM,
    redact::{NOISE, first_phone},
    types::{EventDraft, SUMMARY_COLUMN, SUMMARY_MAX, WireDraft},
};
use crate::{
    BoxError,
    llm::{Llm, LlmError, Turn},
    rejection::Rejection,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
};

/// 允许一次自我修正，不多给 —— 逼急了模型会编一个合法序号。
///
/// 硬规则和软规则共用这个预算，但用尽之后的去向相反：硬的该批次失败，
/// 软的放行（见 [`validate`] 的三层）。
const MAX_RETRIES: u32 = 1;

/// 撞输出上限后**原样重发**几次，全中才认定「这一段太大」。
///
/// 跑飞（strict JSON schema 下随机陷入重复生成，实测中招率约三分之一）和「这一段真的
/// 太大」在 `finish_reason = length` 上**长得一模一样**，端点不给第二个信号可分。
/// 唯一能分开它们的是重发：跑飞是随机的、与输入规模无关，重发就过；段真太大是确定的，
/// 重发照样撞顶。
///
/// 2 次 ⇒ 误切概率从 1/3 降到约 3.7%，代价是真太大的段每层二分多烧 2 次调用。
/// 这笔账在 `[llm.extract].max_tokens` 压到 12000 之后才划算：撞顶只要约 110s，
/// 不再是吃满 `timeout_secs` 的五分钟。**两处是一起改的，动一个要回头看另一个。**
///
/// ⚠️ 这跟 `llm.rs` 的 `extract_retry` 不是一回事、也不能改用它：那边重发到底就把
/// `Truncated` 吞成成功或失败，而这里**必须把「重发到底仍撞顶」如实翻译成切分信号**。
pub(super) const RUNAWAY_RETRIES: u32 = 2;

/// 模型这一段返回的 JSON 外壳。空列表合法 —— 这一段确实没有业务事件。
#[derive(JsonSchema, Deserialize, Debug)]
struct SegmentExtraction {
    events: Vec<WireDraft>,
}

/// 一次段调用的失败。**两类的处置完全不同**，所以在类型上分开。
#[derive(Debug)]
pub enum SegError {
    /// 这一段模型**处理不好** —— **切**。两个来源：
    ///
    ///   * **吃不下**（截断 / 超时）—— 端点知识，由 [`LiveModel`] 翻译。
    ///   * **长到数不清行号**（[`Invalid::Oversized`]）—— **领域知识**，由 [`validate`]
    ///     分档。序号越界和 ref 错在长段上才高发，切小真能解决。
    ///
    /// 「什么信号算处理不好」归适配器与校验；「处理不好就切」跟谁家端点无关、归 `super::run`。
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
/// 会回灌进下一轮 prompt（`MAX_RETRIES`）。
///
/// 这是③的**真接缝**：生产的 [`LiveModel`] 和测试里的 `BisectStub` 两个适配器。
/// 换模型 / 换端点只改 `LiveModel`，`super::run` 的二分逻辑一行不动。
pub trait SegmentModel {
    fn call(
        &self,
        text: &str,
        segment_size: usize,
        open_refs: &BTreeSet<u32>,
    ) -> impl Future<Output = Result<Vec<EventDraft>, SegError>> + Send;
}

/// 校验通过的一段 —— 事件，外加**不值得整群作废**的那些抱怨。
///
/// `Debug` 是 `unwrap_err()` 要的。它打的是 `EventDraft`（本来就要落库）和
/// [`Rejection`] 的运维版 —— 逐字证据仍然只走 [`Rejection::verbatim`]。
#[derive(Debug)]
pub(super) struct Checked {
    pub(super) events: Vec<EventDraft>,
    /// 只有软规则不过时的重问文案。`None` = 全过。
    pub(super) soft: Option<Rejection>,
}

/// 校验不通过的两类。**处置完全相反，所以在类型上分开**（跟 [`SegError`] 一个规矩）。
///
/// ⚠️ **判据是「缩小问题能不能解决它」，不是「错得多严重」。**
#[derive(Debug)]
pub(super) enum Invalid {
    /// 模型在长段上数不清行号 —— **切小再来**，走 `super::run` 那套自适应二分。
    ///
    /// [`prompt`](super::prompt) 的模块注释记着实测案例：**391 行的一段**里模型把行号
    /// 当 ref 填，给出 E360 / E258 / E240，而便签最大编号是 102。段越长越容易犯，
    /// 所以缩小问题真的能解决它。
    Oversized(Rejection),
    /// 切了也一样犯 —— 该批次失败。
    ///
    /// PII（手机号）和「整句只有记号」跟段长无关，切到底只是白烧上千次调用。
    Fatal(Rejection),
}

impl Invalid {
    /// 回灌给模型的那份 —— **两类都先重问一次**，用尽之后才分道扬镳。
    pub(super) fn rejection(&self) -> &Rejection {
        match self {
            Self::Oversized(r) | Self::Fatal(r) => r,
        }
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 两个变体都只给运维版（规则名 + 条数）。逐字证据只走 `Rejection::verbatim`。
        fmt::Display::fmt(self.rejection(), f)
    }
}

/// 校验模型这一段的输出。**硬规则不通过 = 该批次失败**，不做字段级兜底修补。
///
/// 报错文案**不是给人看的**，是回灌进下一轮 prompt 给模型
/// 读的，模型要照着它自我修正。改文案等于改 prompt。
/// 给人看的那份是 [`Rejection`] 的规则名，两者不是一个东西。
///
/// ⚠️ **规则分三档，处置各不相同 —— 此前它们挤在同一个全或无闸门后面。**
///   * **规模相关**（序号越界 / ref / `msg_indexes` 空 / summary 超列宽）——
///     [`Invalid::Oversized`]，
///     重问一次仍不过就**切小再试**。承重不变量 6：模型看不到 `msg_id`，只看到段内
///     序号，越界即编造 —— 而它数不清，多半是因为这一段太长。summary 写过
///     [`SUMMARY_COLUMN`] 同理：段太长才会揉出一条 589 字的 summary。
///     顺带 `sorted(set(v))`：**去重 + 排序是契约不是顺手**。
///     ref 那两种错误的分别见 [`parse_ref`]。
///   * **规模无关 · PII**（summary 含手机号 / 抹完什么都不剩）——
///     [`Invalid::Fatal`]，该批次失败。它归事实列、冻结区不可写，且 `sha256(summary)`
///     是 ⑤ 的缓存键，**一旦进去就是永久的，缓存还会把它焊死**。切了也一样犯。
///     **订单号不在这一档** —— 它是业务标识不是个人信息，见 [`NOISE`]。
///   * **规模无关 · 可读性**（占位符 / 订单号 / 超长）—— **根本不失败**。这一档
///     此前也按 PII 处罚，实测一轮打挂 11 个群，每个只因 ×1 条 summary 就整日
///     0 事件落库；订单号是**第二轮**同样的账 —— 一段 149 个事件里 5 条抄了单号，
///     十天窗口照样归零（理由与实测数字在 [`NOISE`]）。
///     占位符是脱敏抹掉 PII 之后**留下的洞**，三个记号本身不含任何 PII；
///     超长 101 字而列宽 `VARCHAR(200)` 装得下。三者都不是数据问题。
///     **只有 100~[`SUMMARY_COLUMN`] 之间才走这档** —— 过了列宽是上一档。
///
/// 占位符和订单号**就地抹掉**（这是唯一一处字段级修补，范围钉死在 [`NOISE`] 上）。
/// `validate` 的注释此前反对 scrub，理由是「改内容会让缓存键漂掉」——
/// 那说的是**落库前** scrub；在这里抹，`summary` 从一开始就是清理后的值，
/// 缓存键就是它的 sha256，没有任何东西可漂。
/// 走抹除不走重问，是因为这条**修过一轮了**：prompt 的 summary 规则里已有明令和
/// 改写范例（`the_prompt_and_the_masks_agree` 钉着），模型照犯 —— 事件本身就是
/// 「改地址」「换电话」时，唯一的锚点已经被抹成 `<略>`，它只能照抄。
pub(super) fn validate(
    events: Vec<WireDraft>,
    segment_size: usize,
    open_refs: &BTreeSet<u32>,
) -> Result<Checked, Invalid> {
    // 第一元是规则名，**会出进程**；第二元是逐字证据，不出 `extract`。见 [`Rejection`]。
    // 分三档收，判据是「缩小问题能不能解决它」—— 收尾处按档论处。
    let mut oversized: Vec<(&'static str, String)> = Vec::new();
    let mut fatal: Vec<(&'static str, String)> = Vec::new();
    // 软的那档：值得回灌重问一次，但重问用尽后放行，不赔上整群。
    let mut soft: Vec<(&'static str, String)> = Vec::new();
    let mut out: Vec<EventDraft> = Vec::with_capacity(events.len());
    for mut e in events {
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
                oversized.push(("msg_indexes 为空", "msg_indexes 不能为空".into()));
            }
        } else {
            let list = bad
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            oversized.push((
                "序号越界",
                format!("序号 [{list}] 超出本段范围 1-{segment_size}"),
            ));
        }

        // `parse_ref` 的两条错误都是规模相关的，所以收进 `oversized`。
        let r#ref = parse_ref(e.r#ref.as_deref(), open_refs, &mut oversized);

        // **先抹记号再量长度** —— 抹完可能就不超了。
        if NOISE.is_match(&e.summary) {
            // 抹掉留下的空洞顺手收拾：`@某人 催一下` 抹完是 ` 催一下`。
            // 中文 summary 里的空格本就偶发，把空白归一没有副作用。
            e.summary = NOISE
                .replace_all(&e.summary, "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            // 抹完什么都不剩 = 模型整句只写了记号和单号，那是真的没写 summary。
            // 这是抹除**引入的**新失败模式，所以它是硬的。
            if e.summary.is_empty() {
                fatal.push((
                    "summary 只有记号",
                    "summary 不能只由脱敏占位符和订单号构成，要写清楚发生了什么".into(),
                ));
            }
        }
        let n = e.summary.chars().count();
        // ⚠️ 超过列宽不能跟「101 字」同档放行 —— 放行只是把失败推迟到 `assemble`
        // 的硬闸，而那时全群已经抽完，照样整日 0 事件，还白烧了一整群的调用。
        // 归规模相关是因为判据对得上：589 字的 summary 多半是模型把一长段揉成了
        // 一个事件，段切小之后它没那么多东西可写。
        if n > SUMMARY_COLUMN {
            oversized.push((
                "summary 超出列宽",
                format!("summary 长度 {n} 超过 {SUMMARY_MAX} 字，请压缩"),
            ));
        } else if n > SUMMARY_MAX {
            soft.push((
                "summary 超长",
                format!("summary 长度 {n} 超过 {SUMMARY_MAX} 字，请压缩"),
            ));
        }
        if let Some(p) = first_phone(&e.summary) {
            fatal.push(("summary 含手机号", format!("summary 不得含手机号「{p}」")));
        }
        out.push(EventDraft {
            r#ref,
            msg_indexes: std::mem::take(&mut e.msg_indexes),
            summary: std::mem::take(&mut e.summary),
            still_open: e.still_open,
        });
    }
    // **`Fatal` 优先**：PII 挡在那儿，切小再试也救不回来，切到底只是白烧。
    // 挡下整批时另两档一并回灌，让模型一次改完，别赚一次重问只修一半。
    if !fatal.is_empty() {
        fatal.extend(oversized);
        fatal.extend(soft);
        return Err(Invalid::Fatal(Rejection::new(fatal)));
    }
    if !oversized.is_empty() {
        oversized.extend(soft);
        return Err(Invalid::Oversized(Rejection::new(oversized)));
    }
    Ok(Checked {
        events: out,
        soft: (!soft.is_empty()).then(|| Rejection::new(soft)),
    })
}

/// `"E2"` -> `2`。报错文案是**回灌给模型读的**，改它等于改 prompt。
///
/// 两条报错分得很开是有意的：格式不对说明模型没照抄记号（多半又把行号写进来了），
/// 编号不在便签上说明它编了一个不存在的事件 —— 两者要模型做的修正完全不同。
fn parse_ref(
    raw: Option<&str>,
    open_refs: &BTreeSet<u32>,
    errs: &mut Vec<(&'static str, String)>,
) -> Option<u32> {
    let s = raw?;
    let have = if open_refs.is_empty() {
        "（空）".to_string()
    } else {
        format!(
            "[{}]",
            open_refs
                .iter()
                .map(|r| format!("E{r}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    match s.strip_prefix('E').and_then(|d| d.parse::<u32>().ok()) {
        Some(n) if open_refs.contains(&n) => Some(n),
        Some(n) => {
            errs.push((
                "ref 不在便签上",
                format!(
                    "E{n} 不在【进行中的事件】里，现有的是 {have}；本段新出现的事件请把 ref 填成 null"
                ),
            ));
            None
        }
        None => {
            errs.push((
                "ref 格式错",
                format!(
                    "ref「{s}」不是合法编号 —— 要照抄【进行中的事件】里 E 开头的整个记号（如 \"E2\"），\
                     现有的是 {have}。**行号 #N 不是 ref**，行号只填进 msg_indexes；\
                     本段新出现的事件请把 ref 填成 null"
                ),
            ));
            None
        }
    }
}

/// 把模型上一轮的原始输出和校验报错放回对话 —— 只发一条 `User`，模型看不见自己错在哪。
///
/// **[`Rejection::verbatim`] 的生产调用点只有这一处**（逐字那份带证据，可能含 PII，
/// 只许进下一轮 prompt）。
fn reissue(turns: &mut Vec<Turn>, raw: String, r: &Rejection) {
    turns.push(Turn::Assistant(raw));
    turns.push(Turn::User(format!(
        "上一轮的输出没通过校验：\n{}\n\n请按上面的报错修正，重新输出全部事件。",
        r.verbatim()
    )));
}

/// 真实调用。**端点知识全都住在这里** —— 换端点要改的就是这个类型。
///
/// 顺带记本轮的模型用量（[`Self::usage`]）。生产上整轮只造一个（`daily::run` 里
/// `Arc::new`，全部群共享），所以这两个计数天然就是**整轮口径**。
pub struct LiveModel {
    llm: Llm,
    /// 段调用次数。**含二分切出来的、校验重问的和跑飞重发的** —— 它数的是
    /// 「真的发出去几个请求」，不是「分了几段」，因为要拿它当产能的分母。
    calls: AtomicU64,
    /// 累计模型耗时（毫秒）。**墙钟不等于它除以并发**：群与群之间有读取、落库和
    /// 排队的空档。当外推的分子用，不当进度条。
    millis: AtomicU64,
}

/// 本轮的模型用量 —— 给收尾那行日志做产能外推用。
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub calls: u64,
    pub secs: f64,
}

impl LiveModel {
    pub fn new(llm: Llm) -> Self {
        Self {
            llm,
            calls: AtomicU64::new(0),
            millis: AtomicU64::new(0),
        }
    }

    /// 到目前为止发出去的段调用数和累计耗时。
    ///
    /// **`llm.rs` 每次调用打的那行 `elapsed_ms` 是逐次的、算完就扔**，全仓没有第二份
    /// 累计 —— 这里接住它，不是重复记账。
    pub fn usage(&self) -> Usage {
        Usage {
            calls: self.calls.load(Ordering::Relaxed),
            secs: self.millis.load(Ordering::Relaxed) as f64 / 1000.0,
        }
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
        let mut runaways = 0u32;
        loop {
            // 计时包住调用本身，**失败的那次也算** —— 超时和跑飞照样烧了墙钟，
            // 只数成功的会让产能外推系统性偏乐观。
            let started = std::time::Instant::now();
            let result = self.llm.extract(SYSTEM, &turns).await;
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.millis
                .fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            let got: crate::llm::Extracted<SegmentExtraction> = match result {
                Ok(v) => v,
                // **端点知识 -> 切分信号的翻译就这几行。** 只认这两个：
                // 截断（输出预算耗尽）和超时（连上了但这一段没算完）。
                // `Other` 里含连接类错误，**绝不当成「太大」**。
                //
                // 截断先原样重发（[`RUNAWAY_RETRIES`]）：随机跑飞和「段真太大」在
                // `finish_reason = length` 上无从区分，只有重发能分。静默重发等于不知道
                // 模型在跑飞，所以每次都喊一声。
                Err(LlmError::Truncated) if runaways < RUNAWAY_RETRIES => {
                    runaways += 1;
                    tracing::warn!(
                        segment_size,
                        attempt = runaways,
                        "模型撞输出上限，原样重发（分不清跑飞还是这段太大）"
                    );
                    continue;
                }
                Err(LlmError::Truncated) => {
                    return Err(SegError::TooBig(format!(
                        "重发 {RUNAWAY_RETRIES} 次仍撞输出上限"
                    )));
                }
                Err(LlmError::Timeout) => {
                    return Err(SegError::TooBig("请求超时".into()));
                }
                Err(e) => return Err(SegError::Failed(Box::new(e))),
            };

            // ⚠️ 下面是 `verbatim()` 仅有的生产调用点（都在 [`reissue`] 里）。逐字那份
            //    进 prompt，日志和 `SegError` 只拿 `Display`（规则名 + 条数）——
            //    见 [`Rejection`]。
            match validate(got.data.events, segment_size, open_refs) {
                Ok(Checked { events, soft: None }) => return Ok(events),
                // 软规则（可读性）：值得重问一次，**但重问用尽就放行**。
                // 让它整群作废是把可读性问题按 PII 处罚 —— 实测一轮打挂 11 个群。
                Ok(Checked {
                    events,
                    soft: Some(r),
                }) => {
                    if attempt < MAX_RETRIES {
                        tracing::warn!(segment_size, attempt, "软校验没过，回灌报错重问：{r}");
                        reissue(&mut turns, got.raw, &r);
                        attempt += 1;
                    } else {
                        // 放行也必须喊一声，否则没人知道库里在积累难看的 summary
                        tracing::warn!(
                            segment_size,
                            "软校验重问 {MAX_RETRIES} 次后仍不通过，放行（不赔上整群）：{r}"
                        );
                        return Ok(events);
                    }
                }
                // 两类都先重问一次 —— 同一段重问便宜，切小再跑贵。**顺序不能反。**
                Err(e) if attempt < MAX_RETRIES => {
                    // 静默重试等于不知道模型在编序号。这条 warn 是唯一的信号。
                    tracing::warn!(segment_size, attempt, "模型输出没过校验，回灌报错重问：{e}");
                    reissue(&mut turns, got.raw, e.rejection());
                    attempt += 1;
                }
                // 重问用尽，去向相反。**规模相关的交给二分**：段越长模型越数不清行号，
                // 切小是真能解决它的 —— `super::run` 一行不动就接住了这个信号。
                Err(Invalid::Oversized(r)) => {
                    return Err(SegError::TooBig(format!(
                        "校验重问 {MAX_RETRIES} 次后仍不通过：{r}"
                    )));
                }
                // 切了也一样犯 -> 该批次失败，不做字段级兜底修补、不落库半个事件。
                Err(Invalid::Fatal(r)) => {
                    return Err(SegError::Failed(
                        format!("校验重试 {MAX_RETRIES} 次后仍不通过：{r}").into(),
                    ));
                }
            }
        }
    }
}

// summary 归事实列，PII 一旦进去就是永久的，缓存还会把它焊死。
#[cfg(test)]
mod tests {
    use super::*;

    /// **跑飞和「这段真的太大」在 `finish_reason = length` 上无从区分**，
    /// 只有重发能分开：随机跑飞重发就过，段真太大重发照样撞顶。
    ///
    /// 两个方向都钉住 —— 只钉「重发到底要切」，把 [`RUNAWAY_RETRIES`] 改成 0
    /// 照样绿（那就退回了改动之前的行为，每次跑飞白切一刀）。
    #[tokio::test]
    async fn hitting_the_output_cap_is_reissued_before_it_counts_as_too_big() {
        use crate::testutil::{completion, http_model, test_llm};
        let runaway = || (200u16, completion(r#"{"events":[{"ref":nul"#, "length"));
        let good = || (200u16, completion(r#"{"events":[]}"#, "stop"));
        let call = |base: String| async move {
            LiveModel::new(test_llm(&base, "test"))
                .call("段", 10, &BTreeSet::new())
                .await
        };

        // 预算之内恢复 —— 重发把跑飞消化掉，不切
        let mut replies: Vec<_> = (0..RUNAWAY_RETRIES).map(|_| runaway()).collect();
        replies.push(good());
        let (base, server) = http_model(replies, false);
        assert!(
            call(base).await.is_ok(),
            "重发范围内恢复的跑飞不该失败，更不该切"
        );
        assert_eq!(server.join().unwrap().len(), RUNAWAY_RETRIES as usize + 1);

        // 重发到底仍撞顶 —— 这才是「太大」，如实翻译成切分信号交给 super::run
        let (base, server) = http_model((0..=RUNAWAY_RETRIES).map(|_| runaway()).collect(), false);
        assert!(
            matches!(call(base).await, Err(SegError::TooBig(_))),
            "重发到底仍撞顶必须变成切分信号，不能吞成失败"
        );
        assert_eq!(server.join().unwrap().len(), RUNAWAY_RETRIES as usize + 1);
    }

    /// **软规则重问一次就放行，硬规则重问一次就失败** —— 同一个 `MAX_RETRIES`
    /// 预算，用尽之后去向相反。这条钉的是那个去向。
    ///
    /// 没有它，「超长降级」是半个改动：`validate` 分了档，而 `call` 照样把整群打掉。
    #[tokio::test]
    async fn a_soft_rejection_is_reissued_once_and_then_let_through() {
        use crate::testutil::{completion, http_model, test_llm};
        let overlong = || {
            let ev = serde_json::json!({"events":[{
                "ref": null, "msg_indexes": [1],
                "summary": "啊".repeat(SUMMARY_MAX + 1), "still_open": true}]});
            (200u16, completion(&ev.to_string(), "stop"))
        };
        // 两次都超长：重问一次（MAX_RETRIES = 1）之后放行，事件留下
        let (base, server) = http_model(vec![overlong(), overlong()], false);
        let events = LiveModel::new(test_llm(&base, "test"))
            .call("段", 10, &BTreeSet::new())
            .await
            .expect("软规则重问用尽必须放行，不能赔上整群");
        assert_eq!(events.len(), 1, "放行时事件不能丢");
        assert_eq!(
            server.join().unwrap().len(),
            MAX_RETRIES as usize + 1,
            "该重问一次再放行"
        );
    }

    /// summary 的三层处置各钉一遍：**手机号硬失败 · 记号抹掉 · 超长只是软抱怨**。
    ///
    /// ⚠️ 此前三层是同一层（全部硬失败），实测一轮打挂 11 个群，其中 9 个只因
    /// ×1 条 summary 带了个脱敏记号。占位符是抹掉 PII 之后**留下的洞**，
    /// 三个记号本身不含 PII；超长 101 字而列宽 `VARCHAR(200)` 装得下。
    ///
    /// ⚠️ **订单号是第二轮同样的账**，2026-09-16 一并挪进抹除档：它是业务标识不是
    /// 个人信息（`redact::NOISE` 记着实测数字），而全或无的闸门让 149 个事件里
    /// 5 条抄了单号就整十天归零。**手机号留在硬失败那档，这条测试钉的就是这个分野。**
    #[test]
    fn summary_rules_split_into_hard_pii_scrubbed_placeholders_and_soft_length() {
        let check = |s: &str| {
            validate(
                vec![WireDraft {
                    r#ref: None,
                    msg_indexes: vec![1],
                    summary: s.into(),
                    still_open: true,
                }],
                super::super::tests::SEG,
                &BTreeSet::new(),
            )
        };
        let pass = |s: &str| check(s).expect("这条不该失败");
        // **PII 这档必须是 `Fatal`，不能是 `Oversized`** —— 切小再试救不回手机号，
        // 判成 Oversized 就会一路切到底白烧上千次调用，最后照样失败。
        let fatal = |s: &str| match check(s).err() {
            Some(Invalid::Fatal(r)) => r,
            other => panic!("「{s}」该判 Fatal，实际是 {other:?}"),
        };

        assert!(
            check("商家要求加单，平台已受理").is_ok(),
            "正常 summary 被误拒"
        );

        // ── 手机号：硬失败，整批作废。`sha256(summary)` 是 ⑤ 的缓存键，进去就焊死了
        assert!(
            fatal("客户18472625055要求改期")
                .verbatim()
                .contains("手机号")
        );

        // ── 记号：不失败，就地抹掉并收拾空洞
        for (input, want) in [
            ("商家发来<手机号>", "商家发来"),
            ("客户信息<略>需确认", "客户信息需确认"),
            ("@某人 催一下进度", "催一下进度"),
            // 订单号跟占位符同档 —— 这一条此前是 `Fatal`，一段 149 个事件里
            // 中 5 条就整十天 0 条落库
            ("5127366458053009229 要求加单", "要求加单"),
            ("JDLY202608031734008496改期到周一", "改期到周一"),
            (
                "三方5127681781169041222与3316977912130066680并单",
                "三方与并单",
            ),
        ] {
            let got = pass(input);
            assert_eq!(got.events[0].summary, want, "记号没抹干净：{input}");
            assert!(got.soft.is_none(), "抹掉就完事了，不该再留一条软抱怨");
        }
        // 手机号不能被单号的抹除顺带带走 —— 它仍然要硬失败
        assert!(
            fatal("5127366458053009229 的客户18472625055要求改期")
                .verbatim()
                .contains("手机号"),
            "抹掉单号之后手机号还得逮得住"
        );
        // 抹完什么都不剩 = 模型整句只写了记号，那是真的没写 summary
        assert!(fatal("<略>").verbatim().contains("要写清楚发生了什么"));
        assert!(
            fatal("5127366458053009229")
                .verbatim()
                .contains("要写清楚发生了什么"),
            "整句只有一个单号也是没写 summary"
        );

        // ── 超长：软的。事件照样拿得到，只附一条重问文案
        // 长度按 Unicode 码点，不是字节 —— 101 个汉字是 303 字节
        let long = pass(&"啊".repeat(SUMMARY_MAX + 1));
        assert_eq!(long.events.len(), 1, "超长不该丢掉事件");
        assert!(
            long.soft
                .expect("超长该留下软抱怨，否则模型没机会自己压缩")
                .verbatim()
                .contains(&format!("超过 {SUMMARY_MAX} 字"))
        );
        assert!(
            pass(&"啊".repeat(SUMMARY_MAX)).soft.is_none(),
            "刚好 {SUMMARY_MAX} 字该干净通过"
        );
        assert!(
            pass(&"啊".repeat(SUMMARY_COLUMN)).soft.is_some(),
            "刚好 {SUMMARY_COLUMN} 字还装得进列，仍是软的"
        );

        // ── 超列宽：**规模相关**，交给二分。放行只会把失败推迟到 `assemble` 的
        // 硬闸，而那时全群已经抽完 —— 照样整日 0 事件，还白烧了一整群的调用。
        match check(&"啊".repeat(SUMMARY_COLUMN + 1)).err() {
            Some(Invalid::Oversized(r)) => assert!(r.verbatim().contains("请压缩")),
            other => panic!("超列宽该判 Oversized，实际是 {other:?}"),
        }
    }

    /// **这条钉的是那条真实泄漏路径**：校验揪出来的手机号曾经逐字进 `run.log`
    /// 和 `b_merchant_group_run_failure.reason`（`TEXT`，没有保留期）——
    /// 挡 PII 进 `summary` 的闸自己在往库里写 PII。
    ///
    /// 三条渲染路径全测：`Display`（日志和 `SegError` 走这条）、`Debug`（`unwrap`
    /// 和 `{:?}` 走这条）、以及运维版本身。只有 `verbatim()` 允许带证据。
    #[test]
    fn the_operator_facing_rendering_never_carries_evidence() {
        const PHONE: &str = "18472625055";
        const ORDER: &str = "5127366458053009229";
        let r = validate(
            vec![WireDraft {
                r#ref: Some("360".into()),
                msg_indexes: vec![99],
                summary: format!("客户{PHONE}的单{ORDER}要改期<略>"),
                still_open: true,
            }],
            10,
            &BTreeSet::new(),
        )
        .unwrap_err();

        for (how, s) in [("Display", r.to_string()), ("Debug", format!("{r:?}"))] {
            assert!(!s.contains(PHONE), "{how} 漏了手机号：{s}");
            assert!(!s.contains(ORDER), "{how} 漏了订单号：{s}");
            assert!(!s.contains("改期"), "{how} 漏了 summary 正文：{s}");
            assert!(!s.contains("360"), "{how} 漏了模型给的 ref 原文：{s}");
        }
        // 但规则名和条数必须在，否则运维看不出发生了什么
        let ops = r.to_string();
        for rule in ["summary 含手机号", "序号越界", "ref 格式错"] {
            assert!(ops.contains(rule), "运维版少了规则名「{rule}」：{ops}");
        }
        // 逐字那份反过来：证据必须在，否则模型不知道删哪几个字
        assert!(
            r.rejection().verbatim().contains(PHONE),
            "回灌给模型的那份丢了证据"
        );
        // ⚠️ 同时撞上 PII 和序号越界时**必须判 Fatal** —— 切小救不回手机号，
        //    判成 Oversized 就会一路切到底白烧，最后照样失败。
        assert!(
            matches!(r, Invalid::Fatal(_)),
            "PII 在场时必须是 Fatal，实际 {r:?}"
        );
    }

    /// 这四条规则**必须判成 [`Invalid::Oversized`]** —— 它们是模型在长段上数不清行号
    /// 的表现，重问不好使就该切小再试，而不是一把打掉整群。
    #[test]
    fn out_of_range_indexes_and_unknown_refs_are_oversized_not_fatal() {
        let ev = |r: Option<&str>, ix: Vec<usize>| {
            vec![WireDraft {
                r#ref: r.map(str::to_string),
                msg_indexes: ix,
                summary: "正常".into(),
                still_open: true,
            }]
        };
        let refs: BTreeSet<u32> = [2u32].into_iter().collect();
        // 判错档的代价不对称：判成 Fatal 就丢掉了「切小能救回来」这条路
        let oversized = |events, seg| match validate(events, seg, &refs).err() {
            Some(Invalid::Oversized(r)) => r,
            other => panic!("该判 Oversized，实际是 {other:?}"),
        };

        assert!(
            oversized(ev(None, vec![0]), 10)
                .verbatim()
                .contains("超出本段范围 1-10")
        );
        assert!(
            oversized(ev(None, vec![11]), 10)
                .verbatim()
                .contains("超出本段范围 1-10")
        );
        assert!(
            oversized(ev(Some("E5"), vec![1]), 10)
                .verbatim()
                .contains("E5 不在")
        );
        assert!(
            oversized(ev(None, vec![]), 10)
                .verbatim()
                .contains("msg_indexes 不能为空")
        );
        assert!(
            validate(ev(Some("E2"), vec![1]), 10, &refs).is_ok(),
            "便签上有的 ref 该放行"
        );
        assert_eq!(
            validate(ev(Some("E2"), vec![1]), 10, &refs).unwrap().events[0].r#ref,
            Some(2),
            "\"E2\" 必须解析成 2 交给 ④"
        );

        // ⚠️ **这条钉的是那个真实故障**：391 行的一段里模型把行号当 ref 填，
        // 给出 E360 / E258 / E240 而便签最大编号是 102。现在裸数字在 ref 位置上
        // 根本不是合法值，报错还直说「行号不是 ref」—— 那句是回灌给模型看的。
        for bad in ["360", "#360", "E", "e2", "E2 那件"] {
            let r = oversized(ev(Some(bad), vec![1]), 400);
            let msg = r.verbatim();
            assert!(
                msg.contains("不是合法编号") && msg.contains("行号 #N 不是 ref"),
                "ref「{bad}」该被当成格式错，实际：{msg}"
            );
        }

        // 去重 + 排序是契约不是顺手
        let ok = validate(ev(None, vec![3, 1, 3]), 10, &refs).unwrap();
        assert_eq!(ok.events[0].msg_indexes, [1, 3]);
    }
}
