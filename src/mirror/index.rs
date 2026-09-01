//! 索引表 —— 「谁、在哪、到第几字节」。**索引表知识整体住在本文件**：
//! 上游字段名只允许出现在 [`MonthFile`] 和下面那条 `index_sql!` 里，
//! 跟 `ingest/read.rs` 对 DuckDB 那套一个规矩。
//!
//! 下载与落盘（reqwest + `std::fs`）在父模块 —— 两边只通过 [`MonthFile`] 交换数据。

use super::error::Result;
use crate::{ingest, window::Window};
use sqlx::{MySqlPool, Row};

/// 索引表的一行 = 一个群一个月的月文件。
#[derive(Debug, Clone)]
pub(super) struct MonthFile {
    pub(super) corp: String,
    pub(super) room: String,
    pub(super) month: String,
    pub(super) object_key: String,
    /// 下一批 AppendObject 预期字节位置 = 已确认的文件末尾
    pub(super) position: u64,
    /// 已确认追加记录数 = 免费的、独立于字节数的完整性校验
    pub(super) record_count: u64,
}

/// 上游字段名只允许出现在这里。**列名写字面量，不各起一个 `COL_*` 常量** ——
/// 跟 `ingest/read.rs` 的 `select_sql!` 同一个理由，见那边的注释（那 8 个常量买到的
/// 只是把「测试失败」提前成「编译失败」，而下面那两条测试正是查这条 SQL 的文本）。
///
/// **为什么仍是 `macro_rules!` 而不是 `const`**：`format!` 要在编译期校验 `{months}`
/// 那个运行期占位符，`const` + `.replace` 做不到。
macro_rules! index_sql {
    () => {
        r#"
SELECT corp_id, official_room_id, file_month, ndjson_object_key,
       ndjson_position, ndjson_record_count
FROM b_wecom_group_message_month_file
WHERE file_status = 0 AND is_deleted = 0
  AND file_month IN ({months})
  AND (ndjson_last_append_time IS NULL OR ndjson_last_append_time >= ?)
ORDER BY corp_id, official_room_id, file_month
"#
    };
}

/// 拼出这一轮的 SQL。`?` 的个数 = 月份数 + 1（窗口起点），**顺序即 bind 顺序**。
///
/// 分出来只为让下面那个测试摸得到它 —— 占位符个数和 bind 个数对不上是一次
/// 整轮失败，而这个文件没有离线可测的东西（要真 MySQL）。
fn build_sql(n_months: usize) -> String {
    format!(index_sql!(), months = vec!["?"; n_months].join(", "))
}

/// 窗口覆盖的月份里**本轮可能有新字节**的月文件。跨月时同一个群会有两行。
///
/// 月份是我们自己 `%Y%m` 格式化出来的，仍然走 `?` 绑定 —— 没有理由为「反正拼不坏」
/// 破一次例。
///
/// **三道筛选，各挡一类东西**：
///
/// - `is_deleted = 0` —— 上游的软删除。
/// - `file_status = 0` —— 1 = 冻结，不该再读（`CONTEXT.md` 一直把它写成筛选条件，
///   此前代码没实现）。顺带让 `idx_file_status(file_status, file_month, id)` 用得上 ——
///   只有 `is_deleted` 时它是全表扫。
/// - `ndjson_last_append_time >= 窗口起点` —— 上游从 T-N 之前就没再追加过的文件，
///   **必然不含窗口内的消息**：追加时刻只会晚于消息时刻，不会早于。所以跳过它不是
///   近似，是恒等式。实测上游那一列与 `gmt_modified_time` 逐秒相同、会话时区 +08:00，
///   跟窗口是同一个墙钟，不需要留时差余量。
///
/// ⚠️ **`IS NULL` 放行不是兜底**：那一列 `DEFAULT NULL`，`NULL >= ?` 求值为 NULL 会
/// 让这行**悄悄消失**（连 `run_failure` 都不会有，因为这个群压根没进过本轮的名单）。
/// 放行的代价是一次 `fs::metadata`：position 为 0 走 `Empty`，本地已齐走 `Skip`，
/// 两条都零字节。用一次 stat 换掉一条静默丢群的路径。
pub(super) async fn list_month_files(pool: &MySqlPool, w: &Window) -> Result<Vec<MonthFile>> {
    let months = ingest::months(w);

    // sqlx 0.9 拦住一切非 `&'static str` 的 SQL，要显式声明审计过。
    // **这里唯一动态的东西是占位符本身**（`?, ?`），条数由 `months.len()` 决定，
    // 月份和窗口起点的值走下面的 `bind` —— 没有任何外部数据参与拼接。
    let mut q = sqlx::query(sqlx::AssertSqlSafe(build_sql(months.len())));
    for m in &months {
        q = q.bind(m);
    }
    q = q.bind(w.since());
    q.fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            Ok(MonthFile {
                corp: r.try_get("corp_id")?,
                room: r.try_get("official_room_id")?,
                month: r.try_get("file_month")?,
                object_key: r.try_get("ndjson_object_key")?,
                position: r.try_get("ndjson_position")?,
                record_count: r.try_get("ndjson_record_count")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 占位符个数 = 月份数 + 1（窗口起点），且窗口起点那个 `?` 排在月份之后 ——
    /// **顺序即 bind 顺序**，错了是一次整轮失败，而这个文件跑不了离线集成测试。
    #[test]
    fn placeholders_match_the_bind_order() {
        for n in 1..=3 {
            let sql = build_sql(n);
            assert_eq!(sql.matches('?').count(), n + 1, "{sql}");
            let months_at = sql.find("IN (").expect("月份的 IN 子句");
            let since_at = sql
                .find("ndjson_last_append_time >= ?")
                .expect("窗口起点的比较");
            assert!(months_at < since_at, "月份必须先 bind：{sql}");
        }
    }

    /// 三道筛选缺一条都是静默的错数据：漏 `file_status` 会读冻结的文件，
    /// 漏 `is_deleted` 会读软删除的，漏时间会白拉一整个月的静默群。
    #[test]
    fn all_three_filters_are_present() {
        let sql = build_sql(2);
        for clause in [
            "file_status = 0",
            "is_deleted = 0",
            "ndjson_last_append_time >= ?",
        ] {
            assert!(sql.contains(clause), "缺 `{clause}`：{sql}");
        }
    }
}
