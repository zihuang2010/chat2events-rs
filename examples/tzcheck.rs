//! 时区诊断 —— **只读**，一条 SELECT 都不写库、一个 token 都不花。
//!
//! 「库里的时间少了 8 小时」有三个互不相同的成因，这个工具一次把三处都摊开：
//!
//! 1. **看错了列** —— `gmt_created_time` / `gmt_modified_time` 是 MySQL 自己的
//!    `CURRENT_TIMESTAMP`，本仓库一个字都不写。服务器 `time_zone` 是 UTC 的话，
//!    它们本来就比北京时间少 8 小时，而业务时间列是对的。
//! 2. **表结构漂移** —— `schema.sql` 里业务时间列是 `DATETIME`（MySQL 不做任何时区
//!    转换）。线上若被建成 `TIMESTAMP`，MySQL 会按会话时区做「写入转 UTC、读出转回」，
//!    换个时区的客户端去查就会看到位移。`store::check_schema` 只查列名不查类型，
//!    正好是它的盲区。
//! 3. **真的写错了** —— 那 `first_msg_time` 会等于 UTC 而不是本地墙钟。
//!
//! ```sh
//! cargo run --example tzcheck
//! ```

use chat2events_rs::{Result, config};
use sqlx::Row;

const TABLE: &str = "b_merchant_group_event";

#[tokio::main]
async fn main() -> Result<()> {
    let (cfg, secrets) = config::load_from_dir(&config::dir_from_args());
    let pool = config::mysql_pool(&cfg.mysql, &secrets.mysql.url).await?;

    // ① 会话/服务器时区，以及 MySQL 自己的两个「现在」。
    //    两者差 8 小时 = 服务器在 UTC，`CURRENT_TIMESTAMP` 默认列会比北京时间早 8 小时。
    let r = sqlx::query("SELECT @@global.time_zone, @@session.time_zone, NOW(), UTC_TIMESTAMP()")
        .fetch_one(&pool)
        .await?;
    println!("── MySQL 时区 ──");
    println!("  @@global.time_zone  = {}", r.get::<String, _>(0));
    println!("  @@session.time_zone = {}", r.get::<String, _>(1));
    println!(
        "  NOW()               = {}",
        r.get::<chrono::NaiveDateTime, _>(2)
    );
    println!(
        "  UTC_TIMESTAMP()     = {}",
        r.get::<chrono::NaiveDateTime, _>(3)
    );
    println!(
        "  本机本地时间         = {}",
        chrono::Local::now().naive_local()
    );

    // ② 业务时间列的**实际**类型。DATETIME = 不转换（期望）；TIMESTAMP = 会按会话时区转。
    println!("\n── {TABLE} 的时间列实际类型 ──");
    let rows = sqlx::query(
        "SELECT column_name, data_type, column_default \
         FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = ? \
           AND data_type IN ('datetime','timestamp') \
         ORDER BY ordinal_position",
    )
    .bind(TABLE)
    .fetch_all(&pool)
    .await?;
    if rows.is_empty() {
        println!("  （查不到列，先确认表在不在、连的是不是同一个库）");
    }
    for r in &rows {
        let (name, ty) = (r.get::<String, _>(0), r.get::<String, _>(1));
        let note = match ty.as_str() {
            "timestamp" => "  ⚠️ 与 schema.sql 不符（那边是 DATETIME）—— 位移就出在这里",
            _ => "",
        };
        println!("  {name:<24} {ty}{note}");
    }

    // ③ 四张表各有多少行 —— 「时间不对」之前先确认「有没有行」。
    println!("\n── 各表行数 ──");
    for t in [
        "b_merchant_group_event",
        "b_merchant_group_metric_daily",
        "b_merchant_group_agent_metric_daily",
        "b_merchant_group_run_failure",
    ] {
        let n: i64 = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {t}")))
            .fetch_one(&pool)
            .await?
            .get(0);
        println!("  {t:<38} {n}");
    }

    // ④ 最近三行的实际值。business 列该是**业务本地墙钟**（消息发生时的北京时间），
    //    gmt_created_time 是 MySQL 写入那一刻的服务器时间 —— 两者本来就可能差 8 小时，
    //    那不是 bug。
    println!("\n── 最近 3 行（first_msg_time 应为消息发生时的北京时间）──");
    let rows = sqlx::query(
        "SELECT occurred_on, first_msg_time, last_msg_time, gmt_created_time \
         FROM b_merchant_group_event ORDER BY id DESC LIMIT 3",
    )
    .fetch_all(&pool)
    .await?;
    if rows.is_empty() {
        println!("  （表里没有行）");
    }
    for r in &rows {
        println!(
            "  occurred_on={} first_msg_time={} last_msg_time={} gmt_created_time={}",
            r.get::<chrono::NaiveDate, _>(0),
            r.get::<chrono::NaiveDateTime, _>(1),
            r.get::<chrono::NaiveDateTime, _>(2),
            r.get::<chrono::NaiveDateTime, _>(3),
        );
    }
    Ok(())
}
