//! 调用链 —— 分段 → 段调用 → 自适应二分。
//!
//! **段之间必须串行**（后一段要看前一段的便签），并行只加在群与群之间（ADR-0004）。
//! 「是否切分」看运行时信号，**没有阈值参数**；两半共用同一份 `drafts` 且串行 ——
//! **二分不产生接缝**。

use super::{
    assemble::{align, assemble, merge, orphans},
    model::{SegError, SegmentModel},
    render::view,
    segment::{cut, segments},
    types::{Draft, Event},
};
use crate::{BoxError, ingest::Message};
use std::{collections::BTreeMap, future::Future, pin::Pin};

/// 把 `msgs[lo..hi]` 交给模型，就地更新 `drafts`。校验通过才写回。
///
/// 只剩三步：建视图（[`view`]）/ 发请求 / 合并（[`merge`]）。
pub(super) async fn one_call<M: SegmentModel + Sync>(
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
pub(super) fn run<'a, M: SegmentModel + Sync>(
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
        tracing::info!(
            msgs = msgs.len(),
            segments = segs.len(),
            "[分段] 串行传便签"
        );
    }
    // 全群一套 drafts —— ref 编号全局唯一，便签跨段流动
    let mut drafts: BTreeMap<u32, Draft> = BTreeMap::new();
    for (lo, hi) in segs {
        run(model, msgs, lo, hi, &mut drafts, segment_msgs).await?;
    }
    // 最后统一对齐：便签已经跑完，这里只修最终输出，不回头影响段内流程。
    // （不需要清空 draft —— `Draft.idx` 恒非空由 `merge` 在生产点断言。）
    let drafts = align(drafts, msgs);
    orphans(&drafts, msgs);
    drafts.values().map(|d| assemble(d, msgs)).collect()
}

/// 对拍 / `--dry` 用：按**真实分段**逐段渲染，走的是和生产同一条 `view`。
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
