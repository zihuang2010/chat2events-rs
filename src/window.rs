//! 跑批窗口 —— 「非空、连续、升序」由构造保证。
//!
//! 此前窗口以裸 `&[NaiveDate]` 在 9 个签名里流转，每个使用点各自重推
//! 「非空 / 有序」（两处 `min().unwrap()`、一处 `first()`），其中当年那个
//! `read_by_ids`（已删）对空窗口是可达 panic。收进类型后，不变量只在这里成立一次。

use chrono::NaiveDate;

#[derive(Debug, Clone)]
pub struct Window {
    days: Vec<NaiveDate>,
}

impl Window {
    /// 每日跑批的窗口 `[T-(N+1), T-2]`。跳过当天和昨天，读取此前 N 个自然日。
    ///
    /// `lookback = 0` 是配置错误，按「启动期配置直接崩」的规矩 panic ——
    /// 空窗口流进读取层只会静默读出 0 条。
    pub fn new(run_date: NaiveDate, lookback: u32) -> Self {
        assert!(
            lookback >= 1,
            "lookback_days 必须 ≥ 1（0 产生空窗口），改 config.toml"
        );
        Self::span(
            run_date - chrono::Duration::days(i64::from(lookback) + 1),
            run_date - chrono::Duration::days(2),
        )
    }

    /// 闭区间 `[since, until]`，逐日展开。下钻用它：窗口必须覆盖
    /// `[occurred_on, date(last_msg_time)]` —— 一个事件的来源消息可以跨天甚至跨月。
    pub fn span(since: NaiveDate, until: NaiveDate) -> Self {
        assert!(since <= until, "窗口区间倒挂：{since} > {until}");
        Self {
            days: since.iter_days().take_while(|d| *d <= until).collect(),
        }
    }

    pub fn since(&self) -> NaiveDate {
        *self
            .days
            .first()
            .expect("构造保证非空：new/span 都拒绝空区间")
    }

    pub fn until(&self) -> NaiveDate {
        *self
            .days
            .last()
            .expect("构造保证非空：new/span 都拒绝空区间")
    }

    /// 窗口内的每一天，升序。
    pub fn days(&self) -> &[NaiveDate] {
        &self.days
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    #[test]
    fn daily_window_skips_today_and_yesterday() {
        let w = Window::new(d(9, 7), 2);
        assert_eq!(w.days(), [d(9, 4), d(9, 5)]);
        assert_eq!((w.since(), w.until()), (d(9, 4), d(9, 5)));
        assert_eq!(Window::new(d(9, 8), 2).days(), [d(9, 5), d(9, 6)]);
    }

    #[test]
    fn daily_window_keeps_the_requested_day_count_across_calendar_boundaries() {
        for (run_date, lookback, since, until) in [
            ("2026-09-07", 1, "2026-09-05", "2026-09-05"),
            ("2026-09-07", 4, "2026-09-02", "2026-09-05"),
            ("2026-09-01", 2, "2026-08-29", "2026-08-30"),
            ("2026-01-01", 2, "2025-12-29", "2025-12-30"),
            ("2024-03-02", 2, "2024-02-28", "2024-02-29"),
        ] {
            let w = Window::new(run_date.parse().unwrap(), lookback);
            assert_eq!(w.since(), since.parse::<NaiveDate>().unwrap());
            assert_eq!(w.until(), until.parse::<NaiveDate>().unwrap());
            assert_eq!(w.days().len(), lookback as usize);
        }
    }

    #[test]
    #[should_panic]
    fn zero_lookback_is_a_config_error() {
        Window::new(d(9, 1), 0);
    }

    #[test]
    fn span_is_inclusive_contiguous_and_crosses_months() {
        let w = Window::span(d(8, 30), d(9, 1));
        assert_eq!(w.days(), [d(8, 30), d(8, 31), d(9, 1)]);
    }
}
