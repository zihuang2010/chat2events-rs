//! 端口 [`SegmentModel`] ＋ 生产适配器 [`LiveModel`] ＋ 校验 [`validate`]
//! —— 「把一段交给模型，拿回**校验通过**的结果」这一件事。
//!
//! ③ 的**真接缝**就在这里：换模型 / 换端点只改本文件，`super` 里那套分段与自适应
//! 二分一行不动。端点知识（什么信号算「这一段太大」、schema 长什么样、重问几次）
//! 全部收在本文件内，不上浮到调用链上。

use super::{
    prompt::SYSTEM,
    redact::{ORDER_NO, PLACEHOLDER, first_phone},
    types::{EventDraft, SUMMARY_MAX},
};
use crate::{
    BoxError,
    llm::{Llm, LlmError, Turn},
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{collections::BTreeSet, fmt, future::Future};

/// 允许一次「序号越界」的自我修正，不多给 —— 逼急了模型会编一个合法序号。
const MAX_RETRIES: u32 = 1;

/// 模型这一段返回的 JSON 外壳。空列表合法 —— 这一段确实没有业务事件。
#[derive(JsonSchema, Deserialize, Debug)]
struct SegmentExtraction {
    events: Vec<EventDraft>,
}

/// 一次段调用的失败。**两类的处置完全不同**，所以在类型上分开。
#[derive(Debug)]
pub enum SegError {
    /// 这一段模型吃不下 —— **切**（ADR-0004）。
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
pub(super) fn validate(
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
                    tracing::warn!(
                        segment_size,
                        attempt,
                        "模型输出没过校验，回灌报错重问：{msg}"
                    );
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

// summary 归事实列，PII 一旦进去就是永久的，缓存还会把它焊死。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_validation_blocks_ids_phones_placeholders_and_overlength() {
        let bad = |s: &str| {
            validate(
                vec![EventDraft {
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
                .contains("订单号")
        );
        assert!(bad("客户18472625055要求改期").unwrap().contains("手机号"));
        for ph in ["商家发来<手机号>", "客户信息<略>", "回复@某人"] {
            assert!(bad(ph).unwrap().contains("占位符"), "占位符没挡住: {ph}");
        }
        // 长度按 Unicode 码点，不是字节 —— 101 个汉字是 303 字节
        assert!(bad(&"啊".repeat(101)).unwrap().contains("超过 100 字"));
        assert!(bad(&"啊".repeat(100)).is_none(), "刚好 100 字该放行");
    }

    #[test]
    fn validation_rejects_out_of_range_indexes_and_unknown_refs() {
        let ev = |r: Option<u32>, ix: Vec<usize>| {
            vec![EventDraft {
                r#ref: r,
                msg_indexes: ix,
                summary: "正常".into(),
                still_open: true,
            }]
        };
        let refs: BTreeSet<u32> = [2u32].into_iter().collect();

        assert!(
            validate(ev(None, vec![0]), 10, &refs)
                .unwrap_err()
                .contains("超出本段范围 1-10")
        );
        assert!(
            validate(ev(None, vec![11]), 10, &refs)
                .unwrap_err()
                .contains("超出本段范围 1-10")
        );
        assert!(
            validate(ev(Some(5), vec![1]), 10, &refs)
                .unwrap_err()
                .contains("E5 不在")
        );
        assert!(
            validate(ev(Some(2), vec![1]), 10, &refs).is_ok(),
            "便签上有的 ref 该放行"
        );

        // 去重 + 排序是契约不是顺手
        let ok = validate(ev(None, vec![3, 1, 3]), 10, &refs).unwrap();
        assert_eq!(ok[0].msg_indexes, [1, 3]);
    }
}
