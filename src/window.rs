//! 跑批窗口 —— 「非空、连续、升序」由构造保证。
//!
//! 此前窗口以裸 `&[NaiveDate]` 在 9 个签名里流转，每个使用点各自重推
//! 「非空 / 有序」（两处 `min().unwrap()`、一处 `first()`），其中
//! `read_by_ids` 那处对空窗口是可达 panic。收进类型后，不变量只在这里成立一次。

use chrono::NaiveDate;

#[derive(Debug, Clone)]
pub struct Window {
    days: Vec<NaiveDate>,
}

impl Window {
    /// 每日跑批的窗口 `[T-N, T-1]`。T 当天还没过完，不进窗口。
    ///
    /// `lookback = 0` 是配置错误，按「启动期配置直接崩」的规矩 panic ——
    /// 空窗口流进读取层只会静默读出 0 条。
    pub fn new(run_date: NaiveDate, lookback: u32) -> Self {
        assert!(
            lookback >= 1,
            "lookback_days 必须 ≥ 1（0 产生空窗口），改 config.toml"
        );
        Self::span(
            run_date - chrono::Duration::days(i64::from(lookback)),
            run_date - chrono::Duration::days(1),
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
    fn new_is_t_minus_n_to_t_minus_1() {
        let w = Window::new(d(9, 1), 2);
        assert_eq!(w.days(), [d(8, 30), d(8, 31)]);
        assert_eq!((w.since(), w.until()), (d(8, 30), d(8, 31)));
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
