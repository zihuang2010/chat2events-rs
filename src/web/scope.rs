//! 只读取数的 **SQL 片段与它的绑定值** —— 一次遍历同时产出，**无法失同步**。
//!
//! 三个文件的分工：`params.rs` 管「请求长什么样」，本文件管「请求怎么变成带绑定的
//! SQL」，`query.rs` 管「`SELECT` 本身」。依赖方向 `params` ← `scope` ← `query`，无环
//! （本文件从 `query` 取的只有分母边界那个**常量**，SQL 仍然全在 `query.rs`）。
//!
//! ## 它守的是什么
//!
//! 此前 SQL 文本和它的绑定值在**两处分别组装**，靠注释保持同步 —— `query.rs` 里
//! 曾有七条「顺序错了不会报错，只会算错」的警告，讲的都是接缝处的顺序：
//! 分组的分支条件落在最前面、直方图桶边界排在分母边界之前、SLA 在 CTE 与筛选片段
//! **之后**……那是这个模块最贵的失败模式：MySQL 一声不吭，页面照样渲染，
//! 只是每个数字都错。
//!
//! interface 只有一条规矩：**唯一的追加入口是成对的（文本，绑定）**。文本与绑定
//! 只能一起进来，于是「第 n 个 `?` 对应第 n 个绑定」是**构造出来的**，
//! 不是维护出来的。另有三个预置片段（分母边界 · 窗口 · 筛选），它们各自把自己的
//! 绑定一并带上，调用方连数都不用数。
//!
//! [`Scope::finish`] 再用 `assert!` 兜一道 —— **用 `assert!` 不用 `debug_assert!`**：
//! 后者在 release 里会蒸发，而这条不变量恰恰要在生产上第一次请求时就炸，
//! 不能安静地算错一整天。

use super::params::{Bind, Filters};
use super::query::KNOWN_OK_DAYS;
use chrono::NaiveDate;

/// 一条只读 `SELECT` 的文本与绑定。**成对追加，一次遍历同时产出。**
#[derive(Default)]
pub(super) struct Scope {
    sql: String,
    binds: Vec<Bind>,
}

impl Scope {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// **唯一的追加入口**：一段文本 ＋ 它里面的 `?` 按出现顺序要的值。
    ///
    /// 没有「只加文本」或「只加绑定」的口子 —— 那正是此前失同步的那条路。
    /// 不带占位符的片段传空数组即可（编译期就看得出它不该带值）。
    pub(super) fn push(mut self, sql: &str, binds: impl IntoIterator<Item = Bind>) -> Self {
        self.sql.push_str(sql);
        self.binds.extend(binds);
        self
    }

    /// 预置片段：**分母边界**（已知成功群日）的 `JOIN`。三个绑定跟着它走。
    ///
    /// 调用方的文本停在基表 `b_merchant_group_event e` 之后即可，
    /// `JOIN (…) d ON …` 整段由这里出 —— 于是「分母边界的三个 `?` 排在
    /// 外层窗口之前」这件事再也不用写在注释里。
    pub(super) fn known_ok_days(self, corp: &str, since: NaiveDate, until: NaiveDate) -> Self {
        self.push(
            &format!(" JOIN ({KNOWN_OK_DAYS}) d ON d.roomid = e.roomid AND d.dt = e.occurred_on"),
            window_binds(corp, since, until),
        )
    }

    /// 预置片段：**查询窗口**。三个绑定跟着它走。
    pub(super) fn window(self, corp: &str, since: NaiveDate, until: NaiveDate) -> Self {
        self.push(
            " WHERE e.corpid = ? AND e.occurred_on BETWEEN ? AND ?",
            window_binds(corp, since, until),
        )
    }

    /// 预置片段：**动态筛选**。片段与绑定由 [`Filters::clause`] 一次产出，
    /// 所以一条语句里只构造一次 —— 此前同一个函数里它被重复构造五六次，
    /// 每次重新分配，而文本只取第一次那份。
    pub(super) fn filters(self, filters: &Filters, sla_sec: u32, last_day: NaiveDate) -> Self {
        let (clause, binds) = filters.clause(sla_sec, last_day);
        self.push(&clause, binds)
    }

    /// 交出文本与绑定。
    ///
    /// ⚠️ **`assert!` 不是 `debug_assert!`**（理由见模块文档）。计数按字面 `?` 数 ——
    /// 本模块经手的 SQL 里没有任何字符串字面量含 `?`，加一个之前先想清楚这条断言。
    pub(super) fn finish(self) -> (String, Vec<Bind>) {
        assert_eq!(
            self.sql.matches('?').count(),
            self.binds.len(),
            "占位符个数与绑定个数不等，这条 SQL 会静默算错：{}",
            self.sql
        );
        (self.sql, self.binds)
    }
}

fn window_binds(corp: &str, since: NaiveDate, until: NaiveDate) -> [Bind; 3] {
    [
        Bind::Str(corp.to_owned()),
        Bind::Date(since),
        Bind::Date(until),
    ]
}
