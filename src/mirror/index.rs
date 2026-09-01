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

/// 索引表列名 —— `index_sql!` 的 SELECT 列表和取值点引用**同一组常量**，
/// 打错一个字母是编译错误（跟 `ingest/read.rs` 的 COL_* 同一个理由）。
const COL_CORP_ID: &str = "corp_id";
const COL_ROOM_ID: &str = "official_room_id";
const COL_FILE_MONTH: &str = "file_month";
const COL_OBJECT_KEY: &str = "ndjson_object_key";
const COL_POSITION: &str = "ndjson_position";
const COL_RECORD_COUNT: &str = "ndjson_record_count";

/// 上游字段名只允许出现在这里。
///
/// **为什么是 `macro_rules!` 而不是 `const`**：跟 `ingest/read.rs` 的 `select_sql!`
/// 同一个理由，见那边的注释 —— `format!` 在编译期校验占位符。
macro_rules! index_sql {
    () => {
        r#"
SELECT {corp_id}, {room_id}, {file_month}, {object_key},
       {position}, {record_count}
FROM b_wecom_group_message_month_file
WHERE is_deleted = 0 AND {file_month} IN ({months})
ORDER BY {corp_id}, {room_id}, {file_month}
"#
    };
}

/// 窗口覆盖的月份里登记的全部月文件。跨月时同一个群会有两行。
///
/// 月份是我们自己 `%Y%m` 格式化出来的，仍然走 `?` 绑定 —— 没有理由为「反正拼不坏」
/// 破一次例。`is_deleted` 是上游的软删除/冻结标记，1 = 不该再读。
pub(super) async fn list_month_files(pool: &MySqlPool, w: &Window) -> Result<Vec<MonthFile>> {
    let months = ingest::months(w);
    let sql = format!(
        index_sql!(),
        months = vec!["?"; months.len()].join(", "),
        corp_id = COL_CORP_ID,
        room_id = COL_ROOM_ID,
        file_month = COL_FILE_MONTH,
        object_key = COL_OBJECT_KEY,
        position = COL_POSITION,
        record_count = COL_RECORD_COUNT,
    );

    // sqlx 0.9 拦住一切非 `&'static str` 的 SQL，要显式声明审计过。
    // **这里唯一动态的东西是占位符本身**（`?, ?`），条数由 `months.len()` 决定，
    // 月份的值走下面的 `bind` —— 没有任何外部数据参与拼接。
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for m in &months {
        q = q.bind(m);
    }
    q.fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            Ok(MonthFile {
                corp: r.try_get(COL_CORP_ID)?,
                room: r.try_get(COL_ROOM_ID)?,
                month: r.try_get(COL_FILE_MONTH)?,
                object_key: r.try_get(COL_OBJECT_KEY)?,
                position: r.try_get(COL_POSITION)?,
                record_count: r.try_get(COL_RECORD_COUNT)?,
            })
        })
        .collect()
}
