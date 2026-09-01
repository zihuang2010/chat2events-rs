//! 分段 —— 把一个群一天切成 `ceil(n / segment_msgs)` 段（**ADR-0004**）。
//!
//! 这里只管「切在哪」。「要不要再切」是运行时看模型信号决定的，在 [`super::pipeline::run`] 里，
//! **没有阈值参数**。
//!
//! [`cut`] 同时服务两处：等分切点的微调、二分时的中点选择 —— 两处要的是同一件事
//! （挪到相邻消息时间间隔最大处），所以是同一个函数。

use crate::ingest::Message;

// ─────────────────────────────────────────────────────────────────────────────
// 分段与自适应二分（ADR-0004）
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
    // ⚠️ **平局取最小下标**（Python `max(win, key=...)` 的行为）。Rust 的 `max_by_key`
    // 取**最后一个**最大值，直接用会让分段边界与 Python 版不一致。
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
/// `cap` 约 10% —— **`cap` 是省钱旋钮不是硬上限**，撑爆了 [`super::pipeline::run`] 会二分。
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
        // 全是 30s，处处平手 —— Python 的 max(win, key=...) 取第一个，
        // Rust 的 max_by_key 取最后一个，反了分段边界就跟 Python 版不一致
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
