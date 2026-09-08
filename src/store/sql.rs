//! 每条语句的三样素材：**定位哪些行**（[`Shard`]）· **表和列的名字** · **占位符**。
//!
//! 一条业务逻辑都没有 —— 三个写读文件（`facts` / `labels` / `read`）和启动自检
//! （`schema`）共用这一份，表名和列名于是只写一遍。

use crate::window::Window;
use chrono::NaiveDate;

/// 一次 `INSERT` 最多带几行。MySQL 的预处理占位符上限是 65535，`event` 表 15 列 ——
/// 500 行 = 7500 个，留着一个数量级的余量。跑批一个群一天几百个事件，正常撞不到。
pub(super) const BATCH: usize = 500;

/// 表名 —— DELETE / INSERT / [`check_schema`] 三处引用**同一个常量**。
/// `b_merchant_group_agent_metric_daily` 这个 35 字符的名字曾经写了 4 遍，
/// 打错一个字母是运行期的 `Table doesn't exist`，跟 `COL_*` 同一个理由。
///
/// ⑤ 上了 v1 之后 `b_merchant_group_taxonomy` 有了真实读取点（[`read_taxonomy`]），
/// 所以第五张表也进了这份清单和 [`check_schema`]。**查表不查行** —— v0 期
/// 这张表一行都没有是正常状态，不是启动失败。
pub(super) const T_EVENT: &str = "b_merchant_group_event";
pub(super) const T_GROUP: &str = "b_merchant_group_metric_daily";
pub(super) const T_AGENT: &str = "b_merchant_group_agent_metric_daily";
pub(super) const T_FAILURE: &str = "b_merchant_group_run_failure";
pub(super) const T_TAXONOMY: &str = "b_merchant_group_taxonomy";

/// ⚠️ `event_type` 与 `event_types` 是**两列不是一列**：前者是主类（单值，进指标
/// 语义键），后者是全集（JSON，只给 webUI 下钻）。副类不进任何指标 —— 一个事件
/// 计进 N 行会让 `SUM(event_count) > 事件数`，见 [`crate::classify::Labels`]。
pub(super) const EVENT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary, \
    event_type, event_types, taxonomy_version";
pub(super) const GROUP_COLS: &str = "corpid, roomid, dt, msg_count, sender_count, event_count, \
    merchant_event_count, unreplied_count, first_reply_p50_sec, first_reply_p90_sec, \
    extraction_status, classification_status, agent_accounts, fact_completed_time";
pub(super) const AGENT_COLS: &str =
    "corpid, room, agent, dt, event_type, taxonomy_version, event_count, official_user_id";
/// `run_failure` 的列。四组列名里它曾经是唯一没有常量的一组 —— INSERT 语句里写一遍、
/// [`check_schema`] 的清单里再写一遍，也没进那条占位符个数的测试。
pub(super) const FAILURE_COLS: &str = "run_date, corpid, roomid, reason, stage";
/// [`EVENT_COLS`] 的前 12 个 —— **事实列**。末尾三个 `event_type` / `event_types` /
/// `taxonomy_version` 是标注列，不在这里：[`read_events`] 还原的是 `Event`，而 `Event`
/// 只装事实列（标签不刻在它上面，是每次算出来的）。下面那条测试钉住「它必须是
/// EVENT_COLS 的前缀」。
pub(super) const EVENT_FACT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary";
/// `IN (...)` 的集合上限（`docs/database-conventions.md`：控制在 1000 以内）。
pub(super) const IN_MAX: usize = 1000;

/// 只有 classify 真正读的四列。**`centroid` 不在这里** —— 今天两条归纳路径产出的
/// 都只有名字和描述，那一列恒 NULL（`schema.sql` 留着「有就多一条路径」）。
/// `parent_name` 在：词表是两级的，一级要进分类 prompt 的分组标题。
pub(super) const TAXONOMY_COLS: &str = "type_id, parent_name, name, description";

/// `(?, ?, …)`，个数**从列名串自己数出来** —— 手写一个数字，加列时忘了改就是一次
/// 运行期的 `Column count doesn't match`。
pub(super) fn values(cols: &str) -> String {
    format!("({})", holes(cols.split(',').count()))
}

/// `?, ?, …`，用于 `IN (…)`。
pub(super) fn holes(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// **失败隔离粒度 —— 群 × 本次窗口。** 承重不变量 2 / 3 / 5 全都以它为单位。
///
/// 这三样永远一起出现、永远是同一个意思，而 `corp` 和 `room` **都是 `&str`** ——
/// 按位置传的时候递反了**编译通过、测试也可能通过**（同一个 corp 下尤其），
/// 上线后的表现是「事件写到别的群名下」。`read_events` 那条注释已经论证过同一种
/// 静默错法（8 列都是字符串、错位类型兼容、编译通过），只是当时没往函数签名上推。
///
/// `days` 是 `&Window` 而不是一对裸日期：[`write_room`] 要拿整个窗口喂
/// [`stray_days`]（承重不变量 1 的守卫），那是对 `Window` 的真实语义依赖，
/// 不只是取两个端点。`Window` 的「非空、连续、升序」由它自己的构造保证。
#[derive(Clone, Copy)]
pub struct Shard<'a> {
    pub corp: &'a str,
    pub room: &'a str,
    pub days: &'a Window,
}

impl<'a> Shard<'a> {
    pub fn new(corp: &'a str, room: &'a str, days: &'a Window) -> Self {
        Self { corp, room, days }
    }

    /// 摊平成四个绑定值 —— 纯事件区间操作只用得上两个端点，不用整个窗口。
    pub(super) fn parts(&self) -> (&'a str, &'a str, NaiveDate, NaiveDate) {
        (self.corp, self.room, self.days.since(), self.days.until())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 占位符个数从列名串数出来 —— 加列忘了改数字就是一次运行期的列数不匹配。
    #[test]
    fn placeholder_count_follows_the_column_list() {
        assert_eq!(EVENT_COLS.split(',').count(), 15);
        assert_eq!(GROUP_COLS.split(',').count(), 14);
        assert_eq!(AGENT_COLS.split(',').count(), 8);
        assert_eq!(FAILURE_COLS.split(',').count(), 5);
        assert_eq!(TAXONOMY_COLS.split(',').count(), 4);
        // 读回来还原 Event 的那 12 列，必须就是 EVENT_COLS 去掉末尾两个标注列 ——
        // 加一列事实列却忘了改这里，`read_events` 会静默少读一个字段。
        assert_eq!(EVENT_FACT_COLS.split(',').count(), 12);
        assert!(
            EVENT_COLS.starts_with(EVENT_FACT_COLS),
            "事实列不再是 EVENT_COLS 的前缀了"
        );
        assert_eq!(
            EVENT_COLS[EVENT_FACT_COLS.len()..].trim_start_matches(", "),
            "event_type, event_types, taxonomy_version"
        );
        // `read_events` 的 CAST 按列名替换 —— 这两个名字必须各自只出现一次，
        // 否则会替换到别的列上（`first_agent_reply_time` 不含 `agents`，这条钉住它）。
        assert_eq!(EVENT_FACT_COLS.matches("agents").count(), 1);
        assert_eq!(EVENT_FACT_COLS.matches("source_msg_ids").count(), 1);
        assert_eq!(values("a, b, c"), "(?, ?, ?)");
        assert_eq!(holes(3), "?, ?, ?");
    }
}
