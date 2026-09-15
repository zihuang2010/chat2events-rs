//! **只读取数** —— 五个读者：每轮开跑取一次词表 · `taxonomy` 归纳 ·
//! `recompute` 重打标 · `daily::recover` 补齐未完成分类 · `daily::retry` 挑出
//! 还没修好的失败群。
//!
//! 独立打标按群读取已保存事件；channel 不搬运正文。
//! 全部是 `SELECT`，一条写语句都不许出现在这个文件里。

use super::sql::{EVENT_FACT_COLS, Shard, T_EVENT, T_FAILURE, T_GROUP, T_TAXONOMY, TAXONOMY_COLS};
use crate::{
    BoxError,
    stage::classify::{Label, TaxonomyType},
    stage::extract::Event,
    stage::ingest::Role,
};
use chrono::{NaiveDate, NaiveDateTime};
use futures_util::TryStreamExt;
use sqlx::{MySqlPool, Row};
use std::collections::BTreeMap;

/// 从群日状态发现待恢复项，零事件同样在内；流式读取避免历史待办全部驻留。
pub fn unfinished_days(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> impl futures_util::Stream<Item = Result<(String, String, NaiveDate, u32), sqlx::Error>> + '_ {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT corpid, roomid, dt, event_count FROM {T_GROUP} \
        WHERE extraction_status = 'ok' AND classification_status IN ('pending','failed') AND dt BETWEEN ? AND ? \
        ORDER BY corpid, roomid, dt"
    )))
    .bind(since)
    .bind(until)
    .fetch(pool)
}

/// 按 `run_failure` 找出**尚未修复**的抽取失败群窗口 —— `daily::retry` 的挑活查询。
///
/// 返回 `(待重跑的 (roomid, window_since, window_until)，跳过的历史行数)`，
/// 前者按窗口排序，调用方顺序扫一遍就能分组，不需要 HashMap。
///
/// ⚠️ **「尚未修复」必须查出来，不能拿整张表重跑。** `run_failure` 是**纯追加表**
/// （全仓没有一条 `DELETE` / `UPDATE`），重跑成功**不删旧失败行** —— 靠
/// [`write_room`](super::write_room) 推进的 `fact_completed_time` 晚于
/// `gmt_created_time` 让它自然失效。所以直接读全表会把历史上失败过的**全部**群
/// 重抽一遍：白烧 token，而且窗口一旦早于日常窗口就写穿冻结区（承重不变量 1）。
///
/// 下面那段 `NOT EXISTS` 是 `web::query::KNOWN_OK_DAYS` 后两条判据的**补集**
/// （有事实完成凭据 · 凭据晚于落在该窗口里的失败）。**两处口径分家是静默的** ——
/// 那边把群日算进聚合分母，这边就不该再去重抽它；反过来这边漏判，那边的
/// unknown 就永远修不好。改一处必须看另一处。
///
/// ⚠️ **`window_since IS NULL` 的历史行一律排除，但要数出来。** 那是加这两列之前
/// 写的行，按「影响全历史」保守处理（`docs/deploy.md`「失败影响面升级」明确
/// 不回填），重跑它们等于把这个群的**整个历史**重抽一遍。静默吞掉同样不行：
/// 那会让运维以为「retry 查不到东西 = 都好了」，而库里还躺着一批永远修不到的群。
///
/// **不返回 `corpid`** —— `mirror::sync` 的 `pick` 只按 roomid 挑（roomid 就是文件名），
/// 多带一列只会造出「按 corp 精确挑」的假象。
///
/// **收集成 `Vec` 不流式**（与 [`unfinished_days`] 不同）：调用方要先按窗口分组才能
/// 决定跑几趟，本来就得全部拿到手，流式换不来任何东西。
pub async fn unrepaired_extract_failures(
    pool: &MySqlPool,
    since: NaiveDate,
) -> Result<(Vec<(String, NaiveDate, NaiveDate)>, i64), BoxError> {
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT f.roomid, f.window_since, f.window_until FROM {T_FAILURE} f \
        WHERE f.run_date >= ? AND f.stage = 'extract' AND f.window_since IS NOT NULL \
          AND NOT EXISTS (SELECT 1 FROM {T_GROUP} g \
                          WHERE g.corpid = f.corpid AND g.roomid = f.roomid \
                            AND g.dt BETWEEN f.window_since AND f.window_until \
                            AND g.extraction_status = 'ok' \
                            AND g.fact_completed_time > f.gmt_created_time) \
        ORDER BY f.window_since, f.window_until, f.roomid"
    )))
    .bind(since)
    .fetch_all(pool)
    .await?;
    let legacy: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {T_FAILURE} \
        WHERE run_date >= ? AND stage = 'extract' AND window_since IS NULL"
    )))
    .bind(since)
    .fetch_one(pool)
    .await?;
    Ok((rows, legacy))
}

/// 打标失败那一支的窗口**并集**。`None` = 这段跑批日里没有打标失败。
///
/// ⚠️ **并集在这里是安全的，换成抽取那支就不是。** `daily::recover` 的窗口是
/// **筛选范围** —— 它内部靠 [`unfinished_days`] 按群日精确挑 `pending` / `failed`，
/// 窗口给宽了只是多扫几天索引，不会多改一行。而 `run_span` 的窗口是
/// **删重写范围**（`write_room` 按 `occurred_on BETWEEN` 整段删重写），给宽了
/// 就是重抽已经成功的天、写穿冻结区。两支的窗口策略不同不是疏漏。
///
/// 所以这里只要两个端点，具体补哪些群日由 `recover` 自己定 ——
/// 也因此**不需要**判「这条失败是不是已经修复了」：已经打上标的群日
/// 根本不在 `unfinished_days` 的结果里。
pub async fn classify_failure_span(
    pool: &MySqlPool,
    since: NaiveDate,
) -> Result<Option<(NaiveDate, NaiveDate)>, BoxError> {
    let (min, max): (Option<NaiveDate>, Option<NaiveDate>) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT MIN(window_since), MAX(window_until) FROM {T_FAILURE} \
            WHERE run_date >= ? AND stage = 'classify' AND window_since IS NOT NULL"
        )))
        .bind(since)
        .fetch_one(pool)
        .await?;
    Ok(min.zip(max))
}

pub async fn read_event_labels(
    pool: &MySqlPool,
    shard: Shard<'_>,
    classifier: &crate::stage::classify::Classifier,
) -> Result<BTreeMap<u64, Option<Label>>, BoxError> {
    let (corp, room, since, until) = shard.parts();
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, event_type, taxonomy_version \
        FROM {T_EVENT} WHERE corpid=? AND roomid=? AND occurred_on BETWEEN ? AND ?"
    )))
    .bind(corp)
    .bind(room)
    .bind(since)
    .bind(until)
    .fetch(pool);
    let mut labels = BTreeMap::new();
    while let Some(row) = rows.try_next().await? {
        labels.insert(
            row.try_get(0)?,
            classifier.saved_labels(row.try_get(1)?, row.try_get(2)?)?,
        );
    }
    Ok(labels)
}

/// 取指定版本的词表；版本和内容由 Classifier 统一校验。
/// 只有显式 v0 允许空表，正式版本查不到行在构造时失败。
///
/// `ORDER BY type_id` 不是装饰：词表会被渲染成 system prompt，顺序变了 prompt 就变了，
/// 而 ⑤ 的确定性要求同样的词表给出同样的答案。库里的行序没有保证，这里钉死它。
pub async fn read_taxonomy<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::MySql>,
    version: &str,
) -> Result<Vec<TaxonomyType>, BoxError> {
    let sql =
        format!("SELECT {TAXONOMY_COLS} FROM {T_TAXONOMY} WHERE version = ? ORDER BY type_id");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(version)
        .fetch_all(executor)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(TaxonomyType {
                type_id: r.try_get(0)?,
                parent_name: r.try_get(1)?,
                name: r.try_get(2)?,
                description: r.try_get(3)?,
            })
        })
        .collect()
}

/// 归纳的输入：一段时间内**去重后**的 summary 及其出现次数，高频在前。
///
/// **`GROUP BY` 下推给 MySQL**（硬规则：过滤/投影/分组尽量下推）—— 归纳只关心
/// 「有哪些不同的说法」，把几万条原样拉进内存再在进程里去重等于白读一遍。
/// 次数留给 `review.md` 排序：先让人看高频的那几类对不对。
pub async fn read_summary_counts(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<Vec<(String, i64)>, BoxError> {
    let sql = format!(
        "SELECT summary, COUNT(*) AS n FROM {T_EVENT} WHERE occurred_on BETWEEN ? AND ? \
         GROUP BY summary ORDER BY n DESC, summary"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(since)
        .bind(until)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// `recompute` 要扫哪些群。**按群分批读是硬规则**（不把大数据集读进内存）：
/// 重打标一个季度、上千个群，一次 `read_events` 全量会把几百万个 `Event` 拉进内存。
pub async fn read_event_rooms(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<Vec<(String, String)>, BoxError> {
    let sql = format!(
        "SELECT DISTINCT corpid, roomid FROM {T_EVENT} WHERE occurred_on BETWEEN ? AND ? \
         ORDER BY corpid, roomid"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(since)
        .bind(until)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// 一个群一段时间内的全部 event，还原成领域类型。
///
/// 两个 JSON 列走 `CAST(... AS CHAR)`：sqlx 没开 `json` feature（写入侧是
/// `bind(to_string(..))`，本来就不需要），读回来当字符串再 `from_str` 是同一条路的反向。
///
/// `asker_role` 复用 [`Role::parse`]，不在这里再写一遍 `== "INTERNAL"` ——
/// 那条契约只该成立一次。库里存的正是 `Role::as_str` 写下去的字面量，
/// 认不出说明有人手改过库，该显式报错。
pub async fn read_events(
    pool: &MySqlPool,
    shard: Shard<'_>,
) -> Result<(Vec<u64>, Vec<Event>), BoxError> {
    let (corp, room, since, until) = shard.parts();
    // 列表从 [`EVENT_FACT_COLS`] 派生，不手抄一遍 —— 抄一遍就会和写入侧漂。
    // 两个 JSON 列套 CAST；`agents` 这个词在别的列名里不出现（`first_agent_reply_time`
    // 是 `agent_` 不是 `agents`），所以按名字替换是安全的。
    let cols = EVENT_FACT_COLS
        .replace("source_msg_ids", "CAST(source_msg_ids AS CHAR)")
        .replace("agents", "CAST(agents AS CHAR)");
    // `id` **接在末尾**，不放开头：前 13 个下标是按 `EVENT_FACT_COLS` 的顺序取的，
    // 插在前面会让它们整体错位一格，而 8 列都是字符串、错位类型兼容、编译通过 ——
    // 正是 `ingest` 那边论证过的那种静默错法。
    //
    // ⚠️ **`id` 是 `store` 与 `recompute` 之间的搬运物，不进 [`Event`]。**
    // `Event` 的契约是「只装事实列」，而 `id` 是数据库的行标识不是业务事实；
    // 它单独一路并排传给 [`retag_room`]，跟 `events` / `types` 那对平行切片同一个形态。
    let sql = format!(
        "SELECT {cols}, id FROM {T_EVENT} \
         WHERE corpid = ? AND roomid = ? AND occurred_on BETWEEN ? AND ? \
         ORDER BY first_msg_time, summary"
    );
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(corp)
        .bind(room)
        .bind(since)
        .bind(until)
        .fetch(pool);
    // 逐行还原，避免 SQL 原始结果、配对临时集合与 Event 全量同时驻留。
    let (mut ids, mut events) = (Vec::new(), Vec::new());
    let mut bytes = 0usize;
    while let Some(r) = rows.try_next().await? {
        let role: String = r.get(8);
        let last_role: Option<String> = r.get(12);
        let id: u64 = r.get(14);
        let event = Event {
            corpid: r.get(0),
            roomid: r.get(1),
            source_msg_ids: serde_json::from_str(&r.get::<String, _>(2))?,
            first_msg_time: r.get::<NaiveDateTime, _>(3),
            last_msg_time: r.get::<NaiveDateTime, _>(4),
            first_agent_reply_time: r.get(5),
            occurred_on: r.get(6),
            asker: r.get(7),
            asker_role: Role::parse(&role)
                .ok_or_else(|| format!("库里的 asker_role「{role}」不是 INTERNAL/EXTERNAL"))?,
            agents: serde_json::from_str(&r.get::<String, _>(9))?,
            first_responder: r.get(10),
            summary: r.get(11),
            // 与 `asker_role` 同一条契约、同一个解析点。`None` 只可能是加这一列之前
            // 抽取的历史行；有值却认不出，说明有人手改过库，该显式报错。
            last_msg_role: last_role
                .map(|s| {
                    Role::parse(&s)
                        .ok_or_else(|| format!("库里的 last_msg_role「{s}」不是 INTERNAL/EXTERNAL"))
                })
                .transpose()?,
            followup_wait_max_sec: r.get(13),
            // **故意不读。** 这条路径服务的是 ⑤ 打标和 `recompute` 重打标，两者只用
            // `summary`；把正文一起读回来，`recompute` 的全历史扫描就会把整个正文
            // 语料拉进内存。`source_messages` 因此不在 `EVENT_FACT_COLS` 里 ——
            // 上面那个 `cols` 正是从它派生的，所以这里少一列不是漏，是设计。
            // 空 `Vec` 在这里只意味着「没读」（见 `Event::source_messages` 的注释）。
            source_messages: Vec::new(),
        };
        bytes += std::mem::size_of::<Event>()
            + event.summary.len()
            + event.corpid.len()
            + event.roomid.len()
            + event.asker.len()
            + event.first_responder.as_ref().map_or(0, String::len)
            + event
                .source_msg_ids
                .iter()
                .chain(&event.agents)
                .map(|s| std::mem::size_of::<String>() + s.len())
                .sum::<usize>();
        if bytes > 32 * 1024 * 1024 {
            return Err("单群事件超过 32 MiB 读取预算，请缩小重打标日期范围".into());
        }
        ids.push(id);
        events.push(event);
    }
    Ok((ids, events))
}
