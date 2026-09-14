//! 调用链 —— 分段 → 段调用 → 自适应二分。
//!
//! **段之间必须串行**（后一段要看前一段的便签），并行只加在群与群之间。
//! 「是否切分」看运行时信号，**没有阈值参数**；两半共用同一份 `drafts` 且串行 ——
//! **二分不产生接缝**。
//!
//! [`cut`] / [`segments`] 曾经单住 `segment.rs`。合回来是因为上面那句话
//! （「分段 → 段调用 → 自适应二分」）本来就把它们和调用链写成了同一件事，而
//! `segment.rs` 的模块文档两次反指 [`run`]：两个文件加起来不到 200 行生产代码，
//! 读一次调用链却要跳两趟。[`cut`] 的两个消费点（等分切点微调、二分中点选择）
//! 也本来就都在这个文件里。

use super::{
    assemble::{align, assemble, merge, orphans},
    model::{SegError, SegmentModel},
    render::view,
    types::{Draft, Event},
};
use crate::{BoxError, stage::ingest::Message};
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
/// 「是否切分」是运行时看信号决定的，**没有阈值参数**。两半共用同一份
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
    // 分子和分母印在同一行 —— `orphans` 自己那条 warn 只有分子，没有事件总数就没法说
    // 「改完变好了没有」，而它正是判 prompt / 便签改动有没有效的那个数。
    tracing::info!(
        events = drafts.len(),
        orphans = orphans(&drafts, msgs),
        "[抽取] 全群完成"
    );
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

// ─────────────────────────────────────────────────────────────────────────────
// 分段与自适应二分
// ─────────────────────────────────────────────────────────────────────────────

/// 把切点从 `target` 挪到附近**相邻消息时间间隔最大**的那一处。返回值恒在 `(lo, hi)`。
///
/// 纯下标算出来的切点对会话结构完全盲目。真实时间戳实测（1096 条）：`[0,365)` 二分的
/// 中点 182 处间隔仅 187s，而**全样本最大的间隔（24323s 的隔夜断点）就在 9 条之外**；
/// 顶层切点 365 处间隔只有 2s（切在一串连发消息中间），挪到 378 是 471s。
///
/// 窗口 = `span` 的 5%，`span` 是这一段的长度，**不新增参数**，再被 `(lo, hi)` 夹一次。
/// **夹这一下是承重的**：没有它，挪动会一路把切点推向末尾，后面就没位置放剩下的切点了
/// （`cap = 1` 时每条一段，本来一点余量都没有）。夹完窗口至少还含 `target` 本身，
/// 所以恒非空、恒严格递增 —— 划分性质不受影响，没余量时自动退化成不挪。
pub(super) fn cut(msgs: &[Message], lo: usize, hi: usize, target: usize, span: usize) -> usize {
    let r = (span / 20).max(1);
    let start = (lo + 1).max(target.saturating_sub(r));
    let end = hi.min(target + r + 1);
    // 调用方保证 lo < target < hi，于是 start <= target < end —— 构造上不可能为空。
    assert!(
        start < end,
        "cut 窗口空了：lo={lo} hi={hi} target={target} span={span}"
    );
    // ⚠️ **平局取最小下标是契约**，由 `ties_pick_the_lowest_index` 钉住。`max_by_key`
    // 取**最后一个**最大值，直接用会静默挪动分段边界 —— 全平手时尤其明显。
    let mut best = start;
    let mut best_gap = msgs[start].at - msgs[start - 1].at;
    for i in (start + 1)..end {
        let gap = msgs[i].at - msgs[i - 1].at;
        if gap > best_gap {
            best_gap = gap;
            best = i;
        }
    }
    best
}

/// 切成 `ceil(n / cap)` 段。`n <= cap` 时只有一段，一次调用跑完。
///
/// 等分只是起点：每个切点再用 [`cut`] 在 ±5% 内挪到时间间隔最大处。因此段长可能超出
/// `cap` 约 10% —— **`cap` 是省钱旋钮不是硬上限**，撑爆了 [`run`] 会二分。
pub(super) fn segments(msgs: &[Message], cap: usize) -> Vec<(usize, usize)> {
    let n = msgs.len();
    let k = n.div_ceil(cap).max(1);
    let mut cuts = vec![0usize];
    for i in 1..k {
        // 下界 = 上一个切点（切点必须严格递增，否则切出空段）
        // 上界 = 下一个切点的目标位置（否则挪动会一路把切点推到末尾，后面没位置了）
        let prev = *cuts.last().unwrap();
        cuts.push(cut(msgs, prev, n * (i + 1) / k, n * i / k, n / k));
    }
    cuts.push(n);
    cuts.windows(2).map(|w| (w[0], w[1])).collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{msgs, msgs_with};
    use super::*;

    #[test]
    fn segments_is_a_partition_and_does_not_split_what_fits() {
        for n in [1usize, 2, 167, 823] {
            for cap in [1usize, 7, 500, 1000, 1_000_000] {
                let sub = msgs(n);
                let bs = segments(&sub, cap);
                assert_eq!(
                    (bs[0].0, bs[bs.len() - 1].1),
                    (0, n),
                    "({n},{cap}) 没覆盖到头尾"
                );
                assert!(
                    bs.windows(2).all(|w| w[0].1 == w[1].0),
                    "({n},{cap}) 段之间有缝/重叠"
                );
                assert!(bs.iter().all(|&(lo, hi)| hi > lo), "({n},{cap}) 有空段");
                assert_eq!(bs.len(), n.div_ceil(cap).max(1), "({n},{cap}) 段数不对");
            }
        }
        assert_eq!(
            segments(&msgs(167), 500),
            [(0, 167)],
            "装得下就必须只有一段"
        );
    }

    #[test]
    fn cut_stays_strictly_inside_and_picks_the_largest_gap() {
        let ms = msgs_with(200, Some(96));
        for (lo, hi) in [(0usize, 200usize), (0, 2), (10, 13), (60, 130)] {
            let target = (lo + hi) / 2;
            let c = cut(&ms, lo, hi, target, hi - lo);
            assert!(lo < c && c < hi, "切点 {c} 跑出 ({lo},{hi})");
            let r = ((hi - lo) / 20).max(1);
            let win = (lo + 1).max(target.saturating_sub(r))..hi.min(target + r + 1);
            let best = win.map(|i| ms[i].at - ms[i - 1].at).max().unwrap();
            assert_eq!(
                ms[c].at - ms[c - 1].at,
                best,
                "({lo},{hi}) 没挑到间隔最大的"
            );
        }
    }

    #[test]
    fn cut_moves_off_the_middle_of_a_burst_onto_the_overnight_break() {
        // 中点 100 处是 30s 的连发；隔夜断点在 96，落在 ±5%（±10 条）窗口内
        let ms = msgs_with(200, Some(96));
        let c = cut(&ms, 0, 200, 100, 200);
        assert_eq!(c, 96, "切点没挪到隔夜断点上");
        assert!(
            ms[c].at - ms[c - 1].at > ms[100].at - ms[99].at,
            "挪过去反而更小了"
        );
    }

    #[test]
    fn cut_breaks_ties_on_the_lowest_index() {
        // 全是 30s，处处平手 —— 契约是取第一个（最小下标）；
        // max_by_key 取最后一个，反了分段边界就静默挪位
        let ms = msgs(200);
        let target = 100usize;
        let r = 200 / 20;
        assert_eq!(
            cut(&ms, 0, 200, target, 200),
            target - r,
            "平局必须取窗口里最小的下标"
        );
    }
}
