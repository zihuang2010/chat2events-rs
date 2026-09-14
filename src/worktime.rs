//! 工作时段口径 `[08:30, 21:00)` —— **全站唯一的一份定义**。
//!
//! 这三个常量此前住在 `extract::assemble`（私有），因为当时只有
//! `followup_wait_max_sec` 走工作时段，首响时效走墙钟差。**现在两个都走它**，
//! 于是它有了三个使用点、三种语言：
//!
//! | 谁 | 怎么用 | 入口 |
//! |---|---|---|
//! | ③ 抽取 | `followup_wait_max_sec` 写入时算 | [`elapsed`] |
//! | ⑥ 指标 | `first_reply_p*_sec` 写入时算 | [`between`] |
//! | webUI 取数 | 首响 / 超时 / 分位数查询期现算 | [`sql_between`] |
//!
//! **SQL 那份由本模块用同样的常量拼出来**，不是在 `web/query.rs` 里手抄一遍数字 ——
//! 手抄的那份改了 `WORK_CLOSE_SEC` 不会跟着变，而错法是静默的：口径漂了只会让
//! 报表上的秒数变小一点，没有任何东西会报错。
//!
//! 第四份在前端 `webui/src/domain/worktime.ts`（浏览器里也要现算一遍）。
//! 那份改不动这份，但**不再只靠注释互指** —— `the_frontend_copy_uses_the_same_three_constants`
//! 把它读进来对着本文件的常量核，改了这边没同步改那边就当场红。
//!
//! ⚠️ **每天都算工作日**：周末与节假日不扣。扣它们要一份工作日历，今天没有
//! （`webui` 的「工作时间口径」缺口条目说的就是这个）；客服群七天在线，
//! 先按天天开工算。加工作日历那天改的是这一个模块。

use chrono::{Datelike, NaiveDateTime, Timelike};

pub const WORK_OPEN_SEC: i64 = 8 * 3600 + 30 * 60;
pub const WORK_CLOSE_SEC: i64 = 21 * 3600;
pub const WORK_DAY_SEC: i64 = WORK_CLOSE_SEC - WORK_OPEN_SEC;

/// `t` 之前累计的工作秒数，**原点任意** —— 有意义的只有两点之差。
/// 跨夜、跨多天、落在时段外全由这一个 `clamp` 吃掉，不分支。
pub fn elapsed(t: NaiveDateTime) -> i64 {
    let days = i64::from(t.date().num_days_from_ce());
    let secs = i64::from(t.time().num_seconds_from_midnight());
    days * WORK_DAY_SEC + secs.clamp(WORK_OPEN_SEC, WORK_CLOSE_SEC) - WORK_OPEN_SEC
}

/// `from` 到 `to` 之间的工作秒数，**下界 0**。
///
/// 钳到 0 而不是让它变负：`first_agent_reply_time < first_msg_time` 是上游时间戳
/// 乱序，不是「负的响应时长」。此前 `metrics` 那处写的是 `.max(0)`，同一件事。
pub fn between(from: NaiveDateTime, to: NaiveDateTime) -> i64 {
    (elapsed(to) - elapsed(from)).max(0)
}

/// [`between`] 的 MySQL 版 —— 两个 `DATETIME` 列之间的工作秒数，下界 0。
///
/// `TO_DAYS` 对应 `num_days_from_ce`（原点不同，差值相同），`TIME_TO_SEC` 对应
/// `num_seconds_from_midnight`。[`WORK_OPEN_SEC`] 那个偏移在差里**自己抵消**，
/// 所以这里不减它 —— 减了结果一样，只是多两个常量。
///
/// 任一端为 `NULL`（未回复）时整个表达式是 `NULL`，与它取代的
/// `GREATEST(0, TIMESTAMPDIFF(...))` 行为一致：`GREATEST(0, NULL)` 也是 `NULL`。
///
/// 库里的 `DATETIME` 不做时区转换，取值即业务本地时间（`CONTEXT.md`），
/// 所以这里和 Rust 那份一样直接取墙钟，不碰时区。
pub fn sql_between(from: &str, to: &str) -> String {
    format!(
        "GREATEST(0, (TO_DAYS({to}) - TO_DAYS({from})) * {WORK_DAY_SEC} \
         + LEAST(GREATEST(TIME_TO_SEC({to}), {WORK_OPEN_SEC}), {WORK_CLOSE_SEC}) \
         - LEAST(GREATEST(TIME_TO_SEC({from}), {WORK_OPEN_SEC}), {WORK_CLOSE_SEC}))"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn t(day: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 8, day)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    #[test]
    fn time_outside_the_window_does_not_count() {
        // 时段内直算
        assert_eq!(between(t(25, 9, 0), t(25, 10, 0)), 3600);
        // 开门前的等待从 08:30 起算
        assert_eq!(between(t(25, 7, 0), t(25, 9, 0)), 1800);
        // 关门后到次日开门前：一秒都不算
        assert_eq!(between(t(25, 21, 30), t(25, 23, 0)), 0);
        assert_eq!(between(t(25, 22, 0), t(26, 8, 0)), 0);
        // 跨夜只算两头的时段内部分：20:30→21:00 的 30 分 + 次日 08:30→09:00 的 30 分
        assert_eq!(between(t(25, 20, 30), t(26, 9, 0)), 3600);
        // 跨整天：中间那天整整一个 WORK_DAY_SEC
        assert_eq!(
            between(t(25, 20, 30), t(27, 9, 0)),
            1800 + WORK_DAY_SEC + 1800
        );
    }

    #[test]
    fn reversed_timestamps_clamp_to_zero_not_negative() {
        assert_eq!(between(t(25, 10, 0), t(25, 9, 0)), 0);
    }

    /// SQL 那份必须**从同一批常量拼出来**。手抄一份数字的话，改了这里的
    /// `WORK_CLOSE_SEC` 而 SQL 没跟着变 —— 报表上的秒数悄悄变小，没有任何东西会报错。
    #[test]
    fn the_sql_expression_is_built_from_the_same_constants() {
        let sql = sql_between("a", "b");
        for n in [WORK_DAY_SEC, WORK_OPEN_SEC, WORK_CLOSE_SEC] {
            assert!(sql.contains(&n.to_string()), "SQL 里少了常量 {n}：{sql}");
        }
        assert_eq!(sql.matches("TO_DAYS").count(), 2);
        assert_eq!(sql.matches("TIME_TO_SEC").count(), 2);
        assert!(sql.starts_with("GREATEST(0,"), "下界 0 不能丢");
    }

    /// 第四份在浏览器里（`webui/src/domain/worktime.ts`）。跨语言，编译器管不到 ——
    /// 此前只有两边的注释互指，而这个模块存在的**全部理由**就是「手抄的那份改了
    /// `WORK_CLOSE_SEC` 不会跟着变，错法是静默的」。这条测试把唯一剩下的手抄份钉住。
    ///
    /// **期望的算式由本文件的常量现拼**，不是又抄一遍数字：改了 `WORK_OPEN_SEC`，
    /// 期望值跟着变，前端没同步改就当场红。
    #[test]
    fn the_frontend_copy_uses_the_same_three_constants() {
        let ts = include_str!("../webui/src/domain/worktime.ts");
        // 前端把开门时间写成「时 * 3600 + 分 * 60」、关门时间写成「时 * 3600」。
        // 关门时间将来若带上分钟，下面这条断言会先炸 —— 那时连同期望算式一起改。
        assert_eq!(WORK_CLOSE_SEC % 3600, 0, "关门时间带分钟了，期望算式要改");
        for expected in [
            format!(
                "WORK_OPEN_SEC = {} * 3600 + {} * 60",
                WORK_OPEN_SEC / 3600,
                WORK_OPEN_SEC % 3600 / 60
            ),
            format!("WORK_CLOSE_SEC = {} * 3600", WORK_CLOSE_SEC / 3600),
            // 前端那份必须**推导**出来，不能各写各的数
            "WORK_DAY_SEC = WORK_CLOSE_SEC - WORK_OPEN_SEC".to_owned(),
        ] {
            assert!(
                ts.contains(&expected),
                "webui/src/domain/worktime.ts 与 src/worktime.rs 口径不一致，缺：{expected}"
            );
        }
    }
}
