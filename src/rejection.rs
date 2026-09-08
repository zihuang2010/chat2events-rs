//! 模型逐字纠错证据与运维摘要分开，抽取和分类共同使用。

use std::{collections::BTreeMap, fmt};

/// 一次校验不通过。**两份文案，两个去处，绝不混用。**
///
/// 分开是因为这两个消费者的需求正好相反：
///   * **模型**要逐字证据才能自我修正 —— 「summary 不得含手机号「138…」」，
///     不说是哪个号，模型不知道删哪几个字。
///   * **运维**（stderr / `run_failure.reason`）只需要知道撞了哪条规则、几次。
///
/// 曾经它们是同一个 `String`：于是 `first_phone` 从 summary 里揪出来的那个手机号
/// 被逐字打进 `run.log`，并当 `reason` 写进 `b_merchant_group_run_failure`（`TEXT`，
/// **没有保留期**）。**挡 PII 进 `summary` 的那道闸，自己成了一条 PII 落库路径** ——
/// 而它恰好只在「真有 PII 漏过来了」时才触发（`redact::PHONE` 匹配不到空格 / 连字符
/// 形态，模型归一化后抄进 summary，这里才逮到）。
///
/// **`Display` 和 `Debug` 给的都是运维版**，逐字那份要显式 [`Rejection::verbatim`]。
/// 于是 `{}`、`{:?}`、`Box<dyn Error>` 三条路都漏不出证据 —— 想漏得先把手伸过来。
pub(crate) struct Rejection {
    /// 逐字，含证据原文。**只有两个合法去处：下一轮 prompt，和校验模块的测试。**
    to_model: String,
    /// 规则名 + 条数，不含任何来自消息或 `summary` 的内容。
    to_operator: String,
}

impl Rejection {
    /// `errs` 的第一元是**规则名**（会出进程，所以要短、稳、可 `GROUP BY`），
    /// 第二元是**逐字证据**（只回灌模型）。
    pub(crate) fn new(errs: Vec<(&'static str, String)>) -> Self {
        let mut by_rule: BTreeMap<&'static str, usize> = BTreeMap::new();
        for (rule, _) in &errs {
            *by_rule.entry(rule).or_default() += 1;
        }
        Self {
            to_operator: by_rule
                .iter()
                .map(|(r, n)| format!("{r} ×{n}"))
                .collect::<Vec<_>>()
                .join(" · "),
            to_model: errs
                .into_iter()
                .map(|(_, m)| m)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// 回灌给模型的逐字文案 —— **带证据原文，可能含 PII**。
    /// 名字起得刺眼是有意的：调用点应该少到一眼能数完。
    pub(crate) fn verbatim(&self) -> &str {
        &self.to_model
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_operator)
    }
}

/// 手写而不是 `derive` —— `derive` 会把 `to_model` 一起打出来，
/// 而 `unwrap()` / `{:?}` / `Box<dyn Error>` 都会走到这里。
impl fmt::Debug for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_operator)
    }
}
