//! 每条语句的三样素材：**定位哪些行**（[`Shard`]）· **表和列的名字** · **占位符**。
//!
//! 一条业务逻辑都没有 —— 三个写读文件（`facts` / `labels` / `read`）和启动自检
//! （`schema`）共用这一份，表名和列名于是只写一遍。

use crate::window::Window;
use chrono::NaiveDate;

/// 一次 `INSERT` 最多带几行。MySQL 的预处理占位符上限是 65535。
///
/// **数的是真正绑上去的那批列，不是 [`EVENT_COLS`]** —— `facts` 写的是
/// `EVENT_FACT_COLS` ＋ `source_messages` 共 15 列（标注列由 ⑤ 单独更新，不在 INSERT 里），
/// 500 行 = 7500 个，留着一个数量级的余量。跑批一个群一天几百个事件，正常撞不到。
///
/// ⚠️ **这个数现在同时受字节约束，而那一半没有量过。** `source_messages` 是装
/// 全文的 `MEDIUMTEXT`：500 行 × 约 8 条来源消息 × 数百中文字符，单条语句轻易到数 MB，
/// 撞的是 `max_allowed_packet` 而不是占位符上限 —— 而失败发生在**模型 token 已经烧完
/// 之后**，重试一次仍以同样方式失败。
///
/// 本轮**不改成按字节切块**：还没有一次实测的单语句字节数，拿一个没依据的数字换另一个
/// 没依据的数字不算改进。取而代之的是 [`check_schema`] 在启动第一秒查一次
/// `@@max_allowed_packet`，低于阈值直接拒绝启动。真量出来了再决定要不要切块。
pub(super) const BATCH: usize = 500;

/// 表名 —— DELETE / INSERT / [`check_schema`] 三处引用**同一个常量**。
/// `b_merchant_group_agent_metric_daily` 这个 35 字符的名字曾经写了 4 遍，
/// 打错一个字母是运行期的 `Table doesn't exist`，跟 `COL_*` 同一个理由。
///
/// ⑤ 上了 v1 之后 `b_merchant_group_taxonomy` 有了真实读取点（[`read_taxonomy`]），
/// 所以它也进了这份清单和 [`check_schema`]。**查表不查行** —— v0 期
/// 这张表一行都没有是正常状态，不是启动失败。
///
/// ⚠️ **`pub(crate)` 不是 `pub(super)`，为的是 `web/cache.rs` 的「数据戳」查询。**
/// 那条 SQL 读四张表各自的最后一次写，用来作废响应缓存 —— 漏改表名是**静默**的：
/// 戳查不到 → 缓存永远不装入 → 只是变慢，没有任何东西会报错。表名不是 SQL 语句，
/// 放出去不削弱「写库 SQL 一条不许外流」（那条约束的是 `INSERT` / `DELETE` 本身）。
///
/// ⚠️ **`web/query.rs` 里那 31 处表名仍是字面量，本轮没动。** 那些在
/// `const EVENT_SELECT: &str` / `const OK_DAYS: &str` 里，而 `const` 插不进另一个
/// `const` —— 要么加 `const_format` 依赖（撞「不加新依赖」），要么把两个常量改成
/// `LazyLock<String>`（那个文件里已有三个先例）。升级路径是后者。不急的理由是
/// 那边漏改会直接 500，看得见；`cache.rs` 那条看不见，所以先修它。
pub(crate) const T_EVENT: &str = "b_merchant_group_event";
pub(crate) const T_GROUP: &str = "b_merchant_group_metric_daily";
pub(crate) const T_AGENT: &str = "b_merchant_group_agent_metric_daily";
pub(crate) const T_AGENT_MSG: &str = "b_merchant_group_agent_msg_daily";
pub(crate) const T_FAILURE: &str = "b_merchant_group_run_failure";
pub(crate) const T_TAXONOMY: &str = "b_merchant_group_taxonomy";
/// 冻结区重写的账。**不进 `web/cache.rs` 的数据戳** —— 没有任何取数读它，
/// 它变了页面上一个数字都不会变。
pub(super) const T_REWRITE: &str = "b_merchant_group_rewrite_log";

/// ⚠️ **标注列只有 `event_type` 一列。** 曾经还有一个 `event_types`（JSON 全集，
/// 副类只给 webUI 下钻），2026-09-14 连同整套多标签机制移除 ——
/// 见 [`crate::stage::classify::Label`]。
/// ⚠️ `source_messages` 排在**最末尾**，而且**只在这里和 [`check_schema`] 出现** ——
/// 它是展示列（既非事实列也非标注列）。放进 [`EVENT_FACT_COLS`] 会让 `read_events`
/// 自动开始读它（那个列表是从 `EVENT_FACT_COLS` 派生的），于是 ⑤ 打标和
/// `recompute` 全历史重打标会把整个正文语料拉进内存 —— 静默的性能塌方。
/// 写入侧在 `facts.rs` 里显式拼上它，不走这个常量。
pub(super) const EVENT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary, \
    last_msg_role, followup_wait_max_sec, event_type, taxonomy_version, source_messages";
pub(super) const GROUP_COLS: &str = "corpid, roomid, dt, msg_count, sender_count, event_count, \
    merchant_event_count, unreplied_count, first_reply_p50_sec, first_reply_p90_sec, \
    extraction_status, classification_status, agent_accounts, fact_completed_time";
pub(super) const AGENT_COLS: &str =
    "corpid, room, agent, dt, event_type, taxonomy_version, event_count, official_user_id";
/// `b_merchant_group_agent_msg_daily` 的列。**没有 `taxonomy_version`，也没有
/// `official_user_id`** —— 前者是因为消息数不依赖词表（打标与重打标不碰这张表），
/// 后者是因为账号映射已经在 `b_merchant_group_metric_daily.agent_accounts` 里，
/// 存第二份会打架。
pub(super) const AGENT_MSG_COLS: &str = "corpid, room, agent, dt, msg_count";
/// `run_failure` 的列。四组列名里它曾经是唯一没有常量的一组 —— INSERT 语句里写一遍、
/// [`check_schema`] 的清单里再写一遍，也没进那条占位符个数的测试。
/// ⚠️ **`window_since` / `window_until` 是失败覆盖的数据窗口，不是 `run_date`。**
/// `run_date` 是跑批日；T+2 之下两者差两天，而 `lookback_days` 可配、backfill 窗口任意，
/// 从跑批日反推数据窗口不可靠。只读工作台判事实新鲜度靠这两列把影响面夹住 ——
/// 没有它们，今天一次失败会把这个群**全部历史**标成 unknown（含冻结区里早已成功的天），
/// 而冻结区不会再被重抽。写入方三处（抽取失败 · 只记账 · 打标失败）手里都有 `Shard`。
pub(super) const FAILURE_COLS: &str =
    "run_date, corpid, roomid, reason, stage, window_since, window_until";
/// [`EVENT_COLS`] 的前 14 个 —— **事实列**。末尾两个 `event_type` /
/// `taxonomy_version` 是标注列，不在这里：[`read_events`] 还原的是 `Event`，而 `Event`
/// 只装事实列（标签不刻在它上面，是每次算出来的）。下面那条测试钉住「它必须是
/// EVENT_COLS 的前缀」。
pub(super) const EVENT_FACT_COLS: &str = "corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, \
    first_agent_reply_time, occurred_on, asker, asker_role, agents, first_responder, summary, \
    last_msg_role, followup_wait_max_sec";
/// `IN (...)` 的集合上限（`docs/database-conventions.md`：控制在 1000 以内）。
pub(super) const IN_MAX: usize = 1000;

/// 只有 classify 真正读的四列。**`centroid` 不在这里** —— 今天两条归纳路径产出的
/// 都只有名字和描述，那一列恒 NULL（`schema.sql` 留着「有就多一条路径」）。
/// `parent_name` 在：词表是两级的，一级要进分类 prompt 的分组标题。
pub(super) const TAXONOMY_COLS: &str = "type_id, parent_name, name, description";

/// 冻结区重写记录的列。`frozen_before` 一起进自检 —— 它是「当时的冻结线是哪天」，
/// 随 `lookback_days` 变，事后反推不出来。
pub(super) const REWRITE_COLS: &str = "run_date, window_since, window_until, frozen_before, rooms";

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
        assert_eq!(EVENT_COLS.split(',').count(), 17);
        assert_eq!(GROUP_COLS.split(',').count(), 14);
        assert_eq!(AGENT_COLS.split(',').count(), 8);
        assert_eq!(AGENT_MSG_COLS.split(',').count(), 5);
        assert_eq!(FAILURE_COLS.split(',').count(), 7);
        assert_eq!(TAXONOMY_COLS.split(',').count(), 4);
        // 读回来还原 Event 的那 14 列，必须就是 EVENT_COLS 去掉末尾两个标注列与展示列 ——
        // 加一列事实列却忘了改这里，`read_events` 会静默少读一个字段。
        assert_eq!(EVENT_FACT_COLS.split(',').count(), 14);
        assert!(
            EVENT_COLS.starts_with(EVENT_FACT_COLS),
            "事实列不再是 EVENT_COLS 的前缀了"
        );
        assert_eq!(
            EVENT_COLS[EVENT_FACT_COLS.len()..].trim_start_matches(", "),
            "event_type, taxonomy_version, source_messages"
        );
        // 展示列**不许**混进事实列：进去了 `read_events` 就会自动开始读正文。
        assert!(!EVENT_FACT_COLS.contains("source_messages"));
        // `read_events` 的 CAST 按列名替换 —— 这两个名字必须各自只出现一次，
        // 否则会替换到别的列上（`first_agent_reply_time` 不含 `agents`，这条钉住它）。
        assert_eq!(EVENT_FACT_COLS.matches("agents").count(), 1);
        assert_eq!(EVENT_FACT_COLS.matches("source_msg_ids").count(), 1);
        // `read_events` 用的是裸下标 `r.get(N)`，而这 14 列有 8 列都是字符串 ——
        // 顺序漂了**类型兼容、编译通过、测试也可能过**，只有真连库才看得出事件写串。
        // 唯一钉住它的地方就是这里：下标 ↔ 列名的对应关系从常量自己数出来。
        let cols: Vec<&str> = EVENT_FACT_COLS.split(',').map(str::trim).collect();
        for (i, name) in [
            "corpid",
            "roomid",
            "source_msg_ids",
            "first_msg_time",
            "last_msg_time",
            "first_agent_reply_time",
            "occurred_on",
            "asker",
            "asker_role",
            "agents",
            "first_responder",
            "summary",
            "last_msg_role",
            "followup_wait_max_sec",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(cols[i], *name, "read_events 的 r.get({i}) 取的是这一列");
        }
        // `id` 接在这 14 列之后 —— `read_events` 的 `r.get(14)`。
        assert_eq!(cols.len(), 14);
        assert_eq!(values("a, b, c"), "(?, ?, ?)");
        assert_eq!(holes(3), "?, ?, ?");
    }
}
