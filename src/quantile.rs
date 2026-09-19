//! 首响分位数的口径 —— **全站唯一的一份定义**，照 [`crate::worktime`] 那个形状做。
//!
//! 定义只有一句：**升序数组取下标 `min(len - 1, floor(len * p))`**。
//! 不插值、不去重、不取均值、不做「标准分位数」那套线性插值 —— 那些都会给出别的数字。
//!
//! | 谁 | 干什么 | 入口 |
//! |---|---|---|
//! | ⑥ 指标 | 算 `metric_daily.first_reply_p*_sec` 写进库，**BI 报表直连读它** | [`of`] |
//! | webUI 取数 | 概览 / 按天 / 按群 / 按客服 / 按分类的分位数，查询期现算 | [`sql_pick`] |
//!
//! 第三份在前端 `webui/src/domain/metrics.ts` 的 `quantile()`（浏览器里也要现算一遍）。
//!
//! ⚠️ **三份实现，此前只有两份被钉住。** 金标向量 `parity-vectors.json` 的 `expected`
//! 是从 `events` 算出来的，而 `groupDaily.first_reply_p50_sec` 在那组向量里是**输入** ——
//! 于是落库那一份（也就是报表真正读的那一列）**不在任何对拍里**。三个数字今天相等，
//! 但没有任何东西保证明天还相等，而分家是静默的：报表照样显示一个看起来合理的秒数。
//!
//! 现在三份读同一组 `quantileCases`：
//! `stage::metrics::tests` 离线跑 [`of`] · `web::tests` 的 `mysql_` 测试跑真 SQL 验
//! [`sql_pick`] · 前端 `test/mock/parity.test.ts` 跑 `quantile()`。**改口径要改三处**，
//! 改漏一处就有一条测试红。

/// 升序数组的分位数。**空集合返回 `None` 而不是 0**（承重不变量 4 的形状：
/// 没有已回复事件时算不出分位数，那不是「0 秒」）。
///
/// ⚠️ **入参必须已升序**，本函数不排序 —— 排序是调用方的事（它通常本来就要排一次，
/// 在这里再排一遍是白花的）。乱序传进来不会报错，只会给出一个错的数字。
pub fn of(sorted_asc: &[u32], p: f64) -> Option<u32> {
    if sorted_asc.is_empty() {
        return None;
    }
    let i = ((sorted_asc.len() as f64 * p) as usize).min(sorted_asc.len() - 1);
    Some(sorted_asc[i])
}

/// [`of`] 的 MySQL 版 —— 挑出第 `p` 分位那一行的 `secs`。
///
/// 期望外层已经算好这三样：`secs`（值）、`rn`（`ROW_NUMBER() OVER (ORDER BY secs)`，
/// 1-based）、`n`（`COUNT(*) OVER ()`）。窗口函数怎么分区是调用方的事 ——
/// 总览不分区，按群 / 按客服 / 按分类 `PARTITION BY` 各自的键。
///
/// `rn = LEAST(n, FLOOR(n * p) + 1)` 就是 [`of`] 的 `min(len - 1, floor(len * p))`：
/// 后者 0-based、前者 1-based，所以 `+1`；`LEAST(n, ...)` 对应 `.min(len - 1)`。
///
/// 空集合时外层 `GROUP BY` 不出行、或 `MIN` 返回 `NULL` —— 和 [`of`] 的 `None` 一致。
pub fn sql_pick(p: f64) -> String {
    format!("MIN(CASE WHEN rn = LEAST(n, FLOOR(n * {p}) + 1) THEN secs END)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个出口读**同一组**金标向量 —— 另外两处（跑批落库、前端）也读它。
    /// 这里只验 Rust 出口；SQL 出口要真 MySQL，在 `web::tests` 的 `mysql_` 测试里。
    #[test]
    fn the_rust_exit_matches_the_shared_parity_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../webui/src/domain/parity-vectors.json")).unwrap();
        let cases = vectors["quantileCases"]
            .as_array()
            .expect("金标向量缺 quantileCases");
        assert!(!cases.is_empty(), "向量为空，这条测试会永远绿");

        for case in cases {
            let secs: Vec<u32> = case["secs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u32)
                .collect();
            let note = case["note"].as_str().unwrap_or("");
            assert!(
                secs.windows(2).all(|w| w[0] <= w[1]),
                "向量必须升序：{note}"
            );
            assert_eq!(
                of(&secs, 0.5),
                case["p50"].as_u64().map(|v| v as u32),
                "p50 不符：{note} {secs:?}"
            );
            assert_eq!(
                of(&secs, 0.9),
                case["p90"].as_u64().map(|v| v as u32),
                "p90 不符：{note} {secs:?}"
            );
        }
    }

    /// SQL 那份是**拼**出来的，不是手抄的数字 —— 这条钉住「两个出口用的是同一个 `p`」。
    /// 手抄的危险在于：把 `0.9` 抄成 `0.99` 不会报错，只会让 p90 变大一点。
    #[test]
    fn the_sql_exit_carries_the_same_p_it_was_given() {
        assert_eq!(
            sql_pick(0.5),
            "MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.5) + 1) THEN secs END)"
        );
        assert_eq!(
            sql_pick(0.9),
            "MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.9) + 1) THEN secs END)"
        );
    }
}
