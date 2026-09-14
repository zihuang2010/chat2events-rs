//! 端口 [`SegmentModel`] ＋ 生产适配器 [`LiveModel`] ＋ 校验 [`validate`]
//! —— 「把一段交给模型，拿回**校验通过**的结果」这一件事。
//!
//! ③ 的**真接缝**就在这里：换模型 / 换端点只改本文件，`super` 里那套分段与自适应
//! 二分一行不动。端点知识（什么信号算「这一段太大」、schema 长什么样、重问几次）
//! 全部收在本文件内，不上浮到调用链上。

use super::{
    prompt::SYSTEM,
    redact::{ORDER_NO, PLACEHOLDER, first_phone},
    types::{EventDraft, SUMMARY_MAX, WireDraft},
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

/// 允许一次「序号越界」的自我修正，不多给 —— 逼急了模型会编一个合法序号。
const MAX_RETRIES: u32 = 1;

/// 模型这一段返回的 JSON 外壳。空列表合法 —— 这一段确实没有业务事件。
#[derive(JsonSchema, Deserialize, Debug)]
struct SegmentExtraction {
    events: Vec<WireDraft>,
}

/// 一次段调用的失败。**两类的处置完全不同**，所以在类型上分开。
#[derive(Debug)]
pub enum SegError {
    /// 这一段模型吃不下 —— **切**。
    ///
    /// 「什么信号算太大」是端点知识、归适配器；「太大就切」跟谁家端点无关、归 `super::run`。
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

/// 校验模型这一段的输出。**不通过 = 该批次失败**，不做字段级兜底修补。
///
/// 报错文案**不是给人看的**，是回灌进下一轮 prompt 给模型
/// 读的，模型要照着它自我修正。改文案等于改 prompt。
/// 给人看的那份是 [`Rejection`] 的规则名，两者不是一个东西。
///
/// 三条规则各自的理由：
///   * **序号越界** —— 承重不变量 6 的守卫。模型看不到 `msg_id`，只看到段内序号，
///     越界即编造。顺带 `sorted(set(v))`：**去重 + 排序是契约不是顺手**。
///   * **ref** —— 便签上没有的 ref 接不上任何 draft，放行就会凭空造一个。
///     线上是字符串 `"E2"`，这里解析成 `2`：**「行号当 ref」这个失败模式由类型挡掉，
///     不由这条校验挡掉**（见 [`WireDraft`] 的注释）。这里只剩两种真错误：
///     格式不对、以及编号不在便签上。
///   * **summary 四条** —— 它归事实列，冻结区不可写，且 `sha256(summary)` 是 ⑤ 的
///     缓存键。**PII 一旦进去就是永久的，缓存还会把它焊死**，所以挡在这里，
///     不做落库前 scrub（那会改内容、让缓存键漂掉）。
pub(super) fn validate(
    events: Vec<WireDraft>,
    segment_size: usize,
    open_refs: &BTreeSet<u32>,
) -> Result<Vec<EventDraft>, Rejection> {
    // 第一元是规则名，**会出进程**；第二元是逐字证据，不出 `extract`。见 [`Rejection`]。
    let mut errs: Vec<(&'static str, String)> = Vec::new();
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
                errs.push(("msg_indexes 为空", "msg_indexes 不能为空".into()));
            }
        } else {
            let list = bad
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            errs.push((
                "序号越界",
                format!("序号 [{list}] 超出本段范围 1-{segment_size}"),
            ));
        }

        let r#ref = parse_ref(e.r#ref.as_deref(), open_refs, &mut errs);

        let n = e.summary.chars().count();
        if n > SUMMARY_MAX {
            errs.push((
                "summary 超长",
                format!("summary 长度 {n} 超过 {SUMMARY_MAX} 字，请压缩"),
            ));
        }
        if let Some(m) = ORDER_NO.find(&e.summary) {
            errs.push((
                "summary 含订单号",
                format!("summary 不得含订单号「{}」，只描述发生了什么", m.as_str()),
            ));
        }
        if let Some(p) = first_phone(&e.summary) {
            errs.push(("summary 含手机号", format!("summary 不得含手机号「{p}」")));
        }
        if let Some(m) = PLACEHOLDER.find(&e.summary) {
            errs.push((
                "summary 含占位符",
                format!(
                    "summary 不得含占位符「{}」—— 它是脱敏留下的记号，不是内容。\
                     改成「客户」「师傅」这样的角色词",
                    m.as_str()
                ),
            ));
        }
        out.push(EventDraft {
            r#ref,
            msg_indexes: std::mem::take(&mut e.msg_indexes),
            summary: std::mem::take(&mut e.summary),
            still_open: e.still_open,
        });
    }
    if errs.is_empty() {
        Ok(out)
    } else {
        Err(Rejection::new(errs))
    }
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

/// 真实调用。**端点知识全都住在这里** —— 换端点要改的就是这个类型。
///
/// 顺带记本轮的模型用量（[`Self::usage`]）。生产上整轮只造一个（`daily::run` 里
/// `Arc::new`，全部群共享），所以这两个计数天然就是**整轮口径**。
pub struct LiveModel {
    llm: Llm,
    /// 段调用次数。**含二分切出来的和校验重问的** —— 它数的是「真的发出去几个请求」，
    /// 不是「分了几段」，因为要拿它当产能的分母。
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
                // **端点知识 -> 切分信号的翻译就这两行。** 只认这两个：
                // 截断（输出预算耗尽）和超时（连上了但这一段没算完）。
                // `Other` 里含连接类错误，**绝不当成「太大」**。
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
                // ⚠️ 这两个分支是 `verbatim()` 仅有的两个生产调用点。逐字那份进 prompt，
                //    日志和 `SegError` 只拿 `Display`（规则名 + 条数）—— 见 [`Rejection`]。
                Err(r) if attempt < MAX_RETRIES => {
                    // 静默重试等于不知道模型在编序号。这条 warn 是唯一的信号。
                    tracing::warn!(segment_size, attempt, "模型输出没过校验，回灌报错重问：{r}");
                    turns.push(Turn::Assistant(got.raw));
                    turns.push(Turn::User(format!(
                        "上一轮的输出没通过校验：\n{}\n\n请按上面的报错修正，重新输出全部事件。",
                        r.verbatim()
                    )));
                    attempt += 1;
                }
                // 次数用完 -> 该批次失败，不做字段级兜底修补、不落库半个事件。
                Err(r) => {
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

    #[test]
    fn summary_validation_blocks_ids_phones_placeholders_and_overlength() {
        let bad = |s: &str| {
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
            .err()
        };
        assert!(
            bad("商家要求加单，平台已受理").is_none(),
            "正常 summary 被误拒"
        );
        assert!(
            bad("5127366458053009229 要求加单")
                .unwrap()
                .verbatim()
                .contains("订单号")
        );
        assert!(
            bad("客户18472625055要求改期")
                .unwrap()
                .verbatim()
                .contains("手机号")
        );
        for ph in ["商家发来<手机号>", "客户信息<略>", "回复@某人"] {
            assert!(
                bad(ph).unwrap().verbatim().contains("占位符"),
                "占位符没挡住: {ph}"
            );
        }
        // 长度按 Unicode 码点，不是字节 —— 101 个汉字是 303 字节
        assert!(
            bad(&"啊".repeat(101))
                .unwrap()
                .verbatim()
                .contains("超过 100 字")
        );
        assert!(bad(&"啊".repeat(100)).is_none(), "刚好 100 字该放行");
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
        for rule in [
            "summary 含手机号",
            "summary 含订单号",
            "序号越界",
            "ref 格式错",
        ] {
            assert!(ops.contains(rule), "运维版少了规则名「{rule}」：{ops}");
        }
        // 逐字那份反过来：证据必须在，否则模型不知道删哪几个字
        assert!(r.verbatim().contains(PHONE), "回灌给模型的那份丢了证据");
    }

    #[test]
    fn validation_rejects_out_of_range_indexes_and_unknown_refs() {
        let ev = |r: Option<&str>, ix: Vec<usize>| {
            vec![WireDraft {
                r#ref: r.map(str::to_string),
                msg_indexes: ix,
                summary: "正常".into(),
                still_open: true,
            }]
        };
        let refs: BTreeSet<u32> = [2u32].into_iter().collect();

        assert!(
            validate(ev(None, vec![0]), 10, &refs)
                .unwrap_err()
                .verbatim()
                .contains("超出本段范围 1-10")
        );
        assert!(
            validate(ev(None, vec![11]), 10, &refs)
                .unwrap_err()
                .verbatim()
                .contains("超出本段范围 1-10")
        );
        assert!(
            validate(ev(Some("E5"), vec![1]), 10, &refs)
                .unwrap_err()
                .verbatim()
                .contains("E5 不在")
        );
        assert!(
            validate(ev(Some("E2"), vec![1]), 10, &refs).is_ok(),
            "便签上有的 ref 该放行"
        );
        assert_eq!(
            validate(ev(Some("E2"), vec![1]), 10, &refs).unwrap()[0].r#ref,
            Some(2),
            "\"E2\" 必须解析成 2 交给 ④"
        );

        // ⚠️ **这条钉的是那个真实故障**：391 行的一段里模型把行号当 ref 填，
        // 给出 E360 / E258 / E240 而便签最大编号是 102。现在裸数字在 ref 位置上
        // 根本不是合法值，报错还直说「行号不是 ref」—— 那句是回灌给模型看的。
        for bad in ["360", "#360", "E", "e2", "E2 那件"] {
            let r = validate(ev(Some(bad), vec![1]), 400, &refs).unwrap_err();
            let msg = r.verbatim();
            assert!(
                msg.contains("不是合法编号") && msg.contains("行号 #N 不是 ref"),
                "ref「{bad}」该被当成格式错，实际：{msg}"
            );
        }

        // 去重 + 排序是契约不是顺手
        let ok = validate(ev(None, vec![3, 1, 3]), 10, &refs).unwrap();
        assert_eq!(ok[0].msg_indexes, [1, 3]);
    }
}
