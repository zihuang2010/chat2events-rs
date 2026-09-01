//! 分段 —— 把一个群一天切成 `ceil(n / segment_msgs)` 段（**ADR-0004**）。
//!
//! 这里只管「切在哪」。「要不要再切」是运行时看模型信号决定的，在 [`super::run`] 里，
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
    assert!(start < end, "cut 窗口空了：lo={lo} hi={hi} target={target} span={span}");
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
