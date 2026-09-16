//! 只读取数 —— 一致快照、meta、日期区间与 event 的 SELECT。
//!
//! **全部是 `SELECT`，且事务显式声明 `READ ONLY`**：webUI 是只读旁路，
//! 写库 SQL 一条都不许出现在这个文件里（`store/` 才是 MySQL 的唯一写入方）。
//!
//! **请求参数的形状不在这里，在 `params.rs`。** 那些类型此前分散在本文件的两端、
//! 中间隔着一千行 SELECT；`Filters` / `Bind` / `DateRange` 现在是从那边 import 回来的。

use super::{
    budget::{ResponseBudget, WebError, too_large},
    config::WebLimits,
    params::{Bind, DEFAULT_SLA_SEC, DateRange, Filters, Paging},
    scope::Scope,
};
use crate::{
    stage::classify::{self, CURRENT_VERSION, TaxonomyType},
    stage::store,
};
use axum::http::StatusCode;
use chrono::NaiveDate;
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Connection, MySql, MySqlConnection, MySqlPool, Row, Transaction};

/// 显式钉住隔离级别，部署机改变默认值也不影响跨表快照；数据库拒绝事务内写入。
pub(super) async fn snapshot(
    connection: &mut MySqlConnection,
) -> Result<Transaction<'_, MySql>, sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *connection)
        .await?;
    connection
        .begin_with("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
        .await
}

/// 前端契约里的 `meta`。**形状不变，变的是 `rooms` / `agents` 怎么算出来。**
///
/// ⚠️ **`rooms` / `agents` 现在跟着查询窗口走**（见 [`read_filters`]），
/// 由 handler 通过 [`Meta::with_filters`] 填进来。此前它们在这里直接查，
/// 而且**没有日期条件** —— 每开一次页面都要把这个企业的全部历史扫一遍，
/// 且 `/api/meta` 与 `/api/dataset` 各调一次，等于扫两遍。那个代价只跟
/// 「库里攒了多久」有关，跟用户选了几天毫无关系，是会随时间无声变糟的那一类。
///
/// ⚠️ **`days` 仍然是全历史**，不能改成窗口内的天 —— 前端拿它填**日期选择器的
/// 可选范围**（`ContextBar` / `RoomFilters` 取首尾、显示「全部 N 天」）。
/// 换成窗口内的天，日期选择器就选不出窗口以外的日子了。
#[derive(Serialize)]
pub(super) struct Meta {
    corpid: String,
    pub(super) days: Vec<String>,
    /// 可用日期的两端。`days` 由它展开而来，[`Period::bounds`] 也用它夹边界。
    /// 不进 JSON —— 前端读的是 `days`，多一份同义字段只会让两边有机会不一致。
    #[serde(skip)]
    pub(super) range: DateRange,
    rooms: Vec<Value>,
    agents: Vec<Value>,
    taxonomy: Vec<TaxonomyType>,
    pub(super) taxonomy_version: &'static str,
    /// **per-项标志缺席时的回落**，由 [`Meta::with_filters`] 算出。
    /// `rooms` / `agents` 两支今天都逐项带自己的标志，所以它只剩客服页那句
    /// 免责说明一个消费者。
    alias_is_authoritative: bool,
}

impl Meta {
    /// 把跟窗口走的那两项填进来。**两个 handler 各自决定用哪个窗口**：
    /// `/api/dataset` 用用户选的，`/api/meta` 用默认窗口（它没有窗口参数）。
    ///
    /// 顺手定下全局的 `alias_is_authoritative` —— 它是 per-项标志缺席时的回落，
    /// 今天前端只剩一个消费者：客服页那句「尚未接入权威名册」的免责说明。
    ///
    /// ⚠️ **算出来，不写死 `true`。** 名册取不到（上游故障、接口还没接上、或者这个
    /// 窗口里的人一个都没有账号映射）时写死 `true` 就是让页面宣称接上了权威名册，
    /// 而每一行其实都回落着显示账号 —— 那句免责说明本来就是为这个状态写的。
    pub(super) fn with_filters(mut self, rooms: Vec<Value>, agents: Vec<Value>) -> Self {
        self.alias_is_authoritative = agents
            .iter()
            .any(|agent| agent["alias_is_authoritative"] == true);
        self.rooms = rooms;
        self.agents = agents;
        self
    }
}

pub(super) async fn available_range(
    connection: &mut MySqlConnection,
    corp: &str,
    limits: &WebLimits,
) -> Result<DateRange, WebError> {
    // MIN/MAX 各自走 (corpid, dt) / (corpid, occurred_on) 索引的两端，与表多大无关。
    let (since, until): (Option<NaiveDate>, Option<NaiveDate>) = sqlx::query_as(
        "SELECT MIN(since), MAX(until) FROM (SELECT MIN(dt) since, MAX(dt) until FROM b_merchant_group_metric_daily WHERE corpid = ? \
         UNION ALL SELECT MIN(occurred_on), MAX(occurred_on) FROM b_merchant_group_event WHERE corpid = ?) dates"
    ).bind(corp).bind(corp).fetch_one(&mut *connection).await?;
    let (Some(since), Some(until)) = (since, until) else {
        return Err(WebError(
            StatusCode::CONFLICT,
            "该企业尚无已落库的群日或事件".into(),
        ));
    };
    if (until - since).num_days() as usize >= limits.max_rows {
        return Err(too_large());
    }
    Ok(DateRange { since, until })
}

pub(super) async fn read_meta(
    connection: &mut MySqlConnection,
    corp: &str,
    limits: &WebLimits,
) -> Result<Meta, WebError> {
    let range = available_range(&mut *connection, corp, limits).await?;
    let (since, until) = (range.since, range.until);
    let taxonomy = store::read_taxonomy(&mut *connection, CURRENT_VERSION).await?;
    if CURRENT_VERSION != "v0" {
        classify::check_types(&taxonomy)?;
    }
    Ok(Meta {
        corpid: corp.into(),
        days: since
            .iter_days()
            .take_while(|day| *day <= until)
            .map(|d| d.to_string())
            .collect(),
        range,
        rooms: Vec::new(),
        agents: Vec::new(),
        taxonomy,
        taxonomy_version: CURRENT_VERSION,
        alias_is_authoritative: false,
    })
}

/// 筛选器选项 —— **窗口内出现过的群与客服**，两条查询都带日期条件。
///
/// 列一个窗口内没有任何记录的群 / 客服，选中之后必然是空结果 —— 所以「跟着窗口走」
/// 不只是省扫描，也是更对的语义。
///
/// ⚠️ **群名单不查 `b_merchant_group_event`。** 抽取成功与失败都会写一行群日
/// （`store::write_room` 同一个事务里写两张表），所以「有事件的群」是「有群日行的群」
/// 的子集 —— 多查一路只是把最大的那张表再扫一遍。实测：只在 event 里、不在
/// `metric_daily` 里的群 **0 个**。
///
/// ⚠️ **客服那一支只产出「待解析的 ID」，不产出最终别名。** 返回的是
/// `(easyUserId, officialUserId?)` —— 账号到姓名那一跳是 HTTP（外部名册），而
/// 「只读 SQL 全在这一个文件」的前提是**这个文件里零 HTTP**。回填在 handler 层做，
/// 见 `serve::filters`。链条是：`easyUserId →(这里的 SQL)→ officialUserId →(HTTP)→ 姓名`。
///
/// ⚠️ **客服名单只能从 `event.agents` 展开，不能改查 `b_merchant_group_agent_metric_daily`。**
/// 那张表按 `metrics::Attribution::FirstResponder` 只记首响人，而 `agents` 是**全部
/// 参与者** —— 实测同一批数据 22 人 vs 20 人。换过去会让「参与过但从没首响过」的人
/// 从筛选器里静默消失。JSON 列进不了索引，这条只能靠窗口把行数压住。
pub(super) async fn read_filters(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    limits: &WebLimits,
) -> Result<(Vec<Value>, Vec<(String, Option<String>)>), WebError> {
    // 历史群仍读取已删除配置；商家 ID 转字符串，避免前端丢失 BIGINT 精度。
    //
    // ⚠️ **失败那一支按 `window_since/window_until` 收敛，不是 `run_date`。**
    // `run_date` 是**跑批日**，`since/until` 是**数据日** —— T+2 之下跑批日恒比任何
    // 可选的 `until` 晚两天以上，拿它去夹数据窗口这一支**永远匹配不到**。而它要捞的
    // 恰恰是「只写了 `run_failure`、连 `metric_daily` 行都没有」的整群跳过：那些群
    // 进不了 `meta.rooms`，前端 `coverage()` 就数不出 `missing`，横幅照样报
    // 「当前窗口抽取完整」—— 偏小且看起来正常，承重不变量 5 点名的那个形状。
    //
    // `window_since IS NULL` 是加这两列之前的历史行，按「影响全历史」保守处理，
    // 与 `KNOWN_OK_DAYS` / `read_group_days` 同一条规矩。
    let (sql, binds) = Scope::new()
        .push(
            "SELECT r.roomid, NULLIF(g.group_name, ''), CAST(g.merchant_id AS CHAR) FROM (\
             SELECT DISTINCT roomid FROM b_merchant_group_metric_daily \
             WHERE corpid = ? AND dt BETWEEN ? AND ? \
             UNION SELECT DISTINCT roomid FROM b_merchant_group_run_failure \
             WHERE corpid = ? AND (window_since IS NULL OR (window_since <= ? AND window_until >= ?))) r \
             LEFT JOIN b_wecom_merchant_group g ON g.official_room_id = r.roomid AND g.corp_id = ? \
             ORDER BY r.roomid LIMIT ?",
            [
                Bind::Str(corp.to_owned()),
                Bind::Date(since),
                Bind::Date(until),
                Bind::Str(corp.to_owned()),
                // 窗口重叠：失败窗口的起点不晚于查询终点、终点不早于查询起点。
                Bind::Date(until),
                Bind::Date(since),
                Bind::Str(corp.to_owned()),
                Bind::Num(limits.max_rows as u64 + 1),
            ],
        )
        .finish();
    let rooms: Vec<(String, Option<String>, Option<String>)> =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_all(&mut *connection)
            .await?;
    if rooms.len() > limits.max_rows {
        return Err(too_large());
    }
    let agents: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT a.agent FROM b_merchant_group_event e \
         JOIN JSON_TABLE(e.agents, '$[*]' COLUMNS(agent VARCHAR(16) PATH '$')) a \
         WHERE e.corpid = ? AND e.occurred_on BETWEEN ? AND ? ORDER BY a.agent LIMIT ?",
    )
    .bind(corp)
    .bind(since)
    .bind(until)
    .bind(limits.max_rows as u64 + 1)
    .fetch_all(&mut *connection)
    .await?;
    if agents.len() > limits.max_rows {
        return Err(too_large());
    }
    let accounts = read_agent_accounts(&mut *connection, corp, since, until, limits).await?;
    Ok((
        rooms
            .into_iter()
            .map(|(roomid, alias, merchant_id)| {
                json!({"roomid": roomid, "alias_is_authoritative": alias.is_some(),
                       "alias": alias, "merchant_id": merchant_id})
            })
            .collect(),
        agents
            .into_iter()
            .map(|(agent,)| {
                let account = accounts.get(&agent).cloned();
                (agent, account)
            })
            .collect(),
    ))
}

/// `easyUserId` -> `officialUserId`，外部名册那一跳的**入参**。
///
/// ⚠️ **这不是姓名，是账号。** 它是链条的第一跳，第二跳（账号 → 姓名）由
/// `web::roster` 走 HTTP 完成。**这个函数保留不动**：外部名册是**串联**在它后面，
/// 不是替换它 —— 上游对 `INTERNAL` 角色的发言人才采集账号且允许缺失，
/// 因此一部分客服本就没有 `officialUserId`，他们在第一跳就断了、只能回落到
/// `easyUserId`。那是既有事实，接名册只是让它显形。
///
/// ⚠️ **数据源是 `b_merchant_group_metric_daily.agent_accounts`，不是
/// `b_merchant_group_agent_metric_daily.official_user_id`。** 后者按
/// `Attribution::FirstResponder` 只记首响人，「参与过但从没首响过」的人在那张表上
/// 一行都没有 —— 换过去正是这些人拿不到别名，而他们本来就是最难认的那批。
/// 这跟上面客服名单不查那张表是同一条理由。
///
/// **JSON 拉成字符串在 Rust 里解**，不在 SQL 里 `JSON_TABLE` ——
/// MySQL 的 `JSON_TABLE` 没有「遍历对象的键」这一档，绕过去要
/// `JSON_KEYS` + `JSON_EXTRACT(CONCAT(...))` 拼路径。而 `store::labels` 的
/// `publish_classification` 读同一列走的就是「`CAST(... AS CHAR)` + serde」，跟着它走。
///
/// 行数是「群数 × 天数」，与 `/api/metric/group` 本来就要拉的那批同量级，
/// 不是新的扫描风险。`ORDER BY dt` 让**后面的日期覆盖前面的** —— 同一个人换过账号时
/// 取窗口内最新的那个，与 `publish_classification` 的口径一致。
///
/// ⚠️ **代价形状是已知的，本轮有意不改。** 它把窗口内「群数 × 天数」份 JSON 拉回来
/// 逐个解析，只为产出**恒定几百个**别名 —— 输入随窗口内群日行数增长，产出不增长；
/// 而且页面加载时 `/api/meta` 与 `/api/dataset` 各做一遍。
/// 它在响应缓存后面（`cache.rs`），白天一组筛选只付一次；重写取数的收益还没量过，
/// 所以先把形状写在这里，让下一个看到它的人知道这不是没人注意到。
async fn read_agent_accounts(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    limits: &WebLimits,
) -> Result<std::collections::BTreeMap<String, String>, WebError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT CAST(agent_accounts AS CHAR) FROM b_merchant_group_metric_daily \
         WHERE corpid = ? AND dt BETWEEN ? AND ? AND agent_accounts IS NOT NULL \
         ORDER BY dt LIMIT ?",
    )
    .bind(corp)
    .bind(since)
    .bind(until)
    .bind(limits.max_rows as u64 + 1)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() > limits.max_rows {
        return Err(too_large());
    }
    let mut out = std::collections::BTreeMap::new();
    for (json,) in rows {
        out.extend(serde_json::from_str::<
            std::collections::BTreeMap<String, String>,
        >(&json)?);
    }
    Ok(out)
}

/// 事件文档的十七个字段 —— **单事件查询与明细分页查询共用这一份**。
///
/// 两处此前各内联一份**字节完全相同**的 `JSON_OBJECT`。给事件加一列时只改一处，
/// 明细表和深链接就会从此显示不同的列，而页面照样渲染 —— 那正是这个项目里
/// 最贵的那类错：没有任何东西会报错。
///
/// 标签两列（`event_type` / `taxonomy_version`）在本群打标未完成时
/// 一律出 `NULL`：批次已更新但本群尚未完成时，事实照常可见，标签等到分类指标发布后
/// 一起展示（承重不变量 4 —— 未完成是 NULL，不是 `__untyped__`，更不是 0）。
const EVENT_DOCUMENT: &str = "CAST(JSON_OBJECT('id', e.id, 'corpid', e.corpid, 'roomid', e.roomid, \
         'source_msg_ids', e.source_msg_ids, 'first_msg_time', DATE_FORMAT(e.first_msg_time, '%Y-%m-%d %H:%i:%s'), \
         'last_msg_time', DATE_FORMAT(e.last_msg_time, '%Y-%m-%d %H:%i:%s'), \
         'first_agent_reply_time', DATE_FORMAT(e.first_agent_reply_time, '%Y-%m-%d %H:%i:%s'), \
         'occurred_on', DATE_FORMAT(e.occurred_on, '%Y-%m-%d'), 'asker', e.asker, 'asker_role', e.asker_role, \
         'agents', e.agents, 'first_responder', e.first_responder, 'summary', e.summary, \
         'last_msg_role', e.last_msg_role, 'followup_wait_max_sec', e.followup_wait_max_sec, \
         'event_type', IF(g.classification_status IN ('pending','failed'), NULL, e.event_type), \
         'taxonomy_version', IF(g.classification_status IN ('pending','failed'), NULL, e.taxonomy_version)) AS CHAR) AS document";

/// 事件明细的一页 —— **手动回表（延迟关联）**，不是直接 `LIMIT 偏移, 条数`。
///
/// `LIMIT 100000, 20` 的字面意思是「取 100020 行，扔掉前 100000 行」，而 MySQL
/// 真的会去取 —— **且每一行都要回表**把 `summary` / `agents` / `source_msg_ids`
/// 这些不在索引里的列读出来，只为了立刻扔掉。翻得越深越慢，最后撞超时。
///
/// 这里把它拆成两步：内层只在 `idx_overview`（覆盖索引）里数够偏移量，
/// **一次都不回表**，只吐出 20 个 `id`；外层拿这 20 个 id 回表 20 次。
/// 偏移量的扫描代价还在，但从「读 N 整行」降成「读 N 个索引条目」。
///
/// ⚠️ **排序键必须唯一**，且**内外两层必须同序**：内层挑出这一页的 id，外层回表把它们
/// 读出来；两边排得不一样的话，页码是对的、页内顺序是乱的。片段由
/// [`Paging::order_by`] 生成，恒定以 `e.id` 收尾保证唯一。
///
/// ⚠️ **仍然要有最大页码护栏。** 延迟关联把常数压小了一个数量级，但没有把
/// 「偏移量越大扫得越多」这件事消掉。翻到第几千页本来就没人在看数据了。
fn event_page_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    paging: &Paging,
) -> Result<(String, Vec<Bind>), WebError> {
    let (offset, limit) = paging.window()?;
    let order = paging.order_by()?;
    // 筛选片段进**内层**（覆盖索引那一层），外层只按 id 回表 —— 筛掉的行连回表都省了。
    // 偏移量与条数是**后缀绑定**，排在窗口与筛选之后。
    Ok(Scope::new()
        .push(
            &format!("SELECT {EVENT_DOCUMENT} FROM (SELECT e.id FROM b_merchant_group_event e"),
            [],
        )
        .window(corp, since, until)
        .filters(
            &paging.filters,
            paging.sla_sec.unwrap_or(DEFAULT_SLA_SEC),
            until,
        )
        .push(
            &format!(
                " ORDER BY {order} LIMIT ?, ?) page \
                 JOIN b_merchant_group_event e ON e.id = page.id \
                 LEFT JOIN b_merchant_group_metric_daily g \
                 ON g.corpid = e.corpid AND g.roomid = e.roomid AND g.dt = e.occurred_on \
                 ORDER BY {order}"
            ),
            [Bind::Num(offset), Bind::Num(limit)],
        )
        .finish())
}

/// **已知成功群日** —— 聚合的分母边界。术语条目见 `CONTEXT.md`「术语」一节；
/// 前端那份实现是 `webui/src/domain/metrics.ts` 的 `groupDayStatus`（返回 `"ok"`）。
///
/// 三条判据，与术语条目逐条对应：
///   1. **抽取成功** —— `extraction_status = 'ok'`；
///   2. **有事实完成凭据** —— `fact_completed_time IS NOT NULL`；
///   3. **该群日所属窗口内没有晚于该凭据的抽取失败** —— 下面那个 `HAVING`。
///
/// 抽取失败或重跑后未确认的群日**照样能查到旧事实**（核实用），但不能计进指标 ——
/// 那样会把一个残缺的数字算成完整的（承重不变量 5）。
///
/// ⚠️ **失败只毒化它自己那个窗口。** 连接条件带 `window_since/until` 的夹子，
/// 因为 `run_failure` 的影响面就是那次跑批的数据窗口。没有这个夹子的时候，
/// 今天一次失败会让这个群**全部历史**掉出分母 —— 含冻结区里早已成功的天，
/// 而冻结区不会再被重抽，那个 unknown 是永久的。`window_since IS NULL` 是
/// 加这两列之前的历史行，仍按「影响全历史」保守处理（不变量 5：历史未知不能推断成已知）。
///
/// ⚠️ **`mx IS NULL OR mx < fct` 不能写成 `NOT (mx >= fct)`。** 从没失败过的群
/// 左连接不出行，`MAX` 是 `NULL`，`NULL >= fct` 求值为 `NULL`，`NOT NULL` 还是 `NULL`，
/// 在 `HAVING` 里当假 —— **恰好把最健康的那批群日全部排除掉**，而指标只是变小，
/// 不会报错。前端那边是 JavaScript 的 `undefined >= x === false`，语义相反。
///
/// ⚠️ **一个群日可能匹配多条窗口不同的失败记录**，所以先连接、再按群日
/// `MAX` 取最后一次失败时间 —— 直接拿连接出来的每一行去比是错的（任何一条早于凭据
/// 的失败都会让这个群日通过）。这一步等价于此前那个相关子查询，
/// 由 `mysql_known_ok_days_rewrite_matches_the_previous_definition` 钉住。
///
/// 这一版把**逐字写了两遍**（一次判空、一次比较）的相关子查询合成一次：MySQL 不保证
/// 对相关子查询做公共子表达式消除，此前每个候选群日行都要做两次失败记录的索引查找。
pub(super) const KNOWN_OK_DAYS: &str = "SELECT g.roomid, g.dt FROM b_merchant_group_metric_daily g \
     LEFT JOIN b_merchant_group_run_failure f \
       ON f.corpid = g.corpid AND f.roomid = g.roomid AND f.stage = 'extract' \
      AND (f.window_since IS NULL OR g.dt BETWEEN f.window_since AND f.window_until) \
     WHERE g.corpid = ? AND g.dt BETWEEN ? AND ? AND g.extraction_status = 'ok' \
       AND g.fact_completed_time IS NOT NULL \
     GROUP BY g.roomid, g.dt, g.fact_completed_time \
     HAVING MAX(f.gmt_created_time) IS NULL OR MAX(f.gmt_created_time) < g.fact_completed_time";

/// 改写之前的那一版，**只给测试用** —— 它是这次改写唯一可运行的证据：
/// `mysql_known_ok_days_rewrite_matches_the_previous_definition` 拿四种群日
/// 断言新旧两版选出的群日集合完全相等。删掉它，那条测试就只能自证。
#[cfg(test)]
pub(super) const LEGACY_KNOWN_OK_DAYS: &str = "SELECT g.roomid, g.dt FROM b_merchant_group_metric_daily g \
     WHERE g.corpid = ? AND g.dt BETWEEN ? AND ? AND g.extraction_status = 'ok' \
       AND g.fact_completed_time IS NOT NULL \
       AND ((SELECT MAX(f.gmt_created_time) FROM b_merchant_group_run_failure f \
             WHERE f.corpid = g.corpid AND f.roomid = g.roomid AND f.stage = 'extract' \
               AND (f.window_since IS NULL OR g.dt BETWEEN f.window_since AND f.window_until)) IS NULL \
            OR (SELECT MAX(f.gmt_created_time) FROM b_merchant_group_run_failure f \
                WHERE f.corpid = g.corpid AND f.roomid = g.roomid AND f.stage = 'extract' \
                  AND (f.window_since IS NULL OR g.dt BETWEEN f.window_since AND f.window_until)) \
               < g.fact_completed_time)";

/// 把 [`KNOWN_OK_DAYS`] 交给测试 —— 它是四个聚合接口的分母边界，
/// 值得有一条测试直接对着它断言，而不是隔着一整个 HTTP 响应去推。
#[cfg(test)]
pub(super) fn ok_days_sql_for_test() -> &'static str {
    KNOWN_OK_DAYS
}

/// 顶部那排 KPI —— 一次扫描出全部计数。
///
/// 每个数都和前端 `aggregate()` 一一对应，**口径不能自己发挥**：
///   * 首响相关的分母一律是**商家发起的事件**（`asker_role = 'EXTERNAL'`），不是事件总数；
///   * `overdue` **把未回复的也算进去**（前端 `isOverdue`：`null` 或超过阈值都算）；
///   * 首响秒数取 `GREATEST(0, ...)`，对应前端的 `Math.max(0, ...)` ——
///     时间倒挂时给 0 而不是负数。
///
/// 首响秒数 —— **工作时段口径**（`[08:30, 21:00)`），不是墙钟差。
///
/// 表达式由 [`crate::worktime::sql_between`] 从 Rust 那份常量拼出来，**不在本文件
/// 手抄数字**：手抄的那份改了 `WORK_CLOSE_SEC` 不会跟着变，而口径漂了只会让秒数
/// 悄悄变小，没有任何东西会报错。⑥ 写进 `metric_daily.first_reply_p*_sec` 的那份
/// 走同一个模块，两边同一个定义。
///
/// 是 `LazyLock<String>` 不是 `const` —— 它得调函数才能拼出来。取值恒定，进程内算一次。
pub(super) static FIRST_REPLY_SEC: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    crate::worktime::sql_between("e.first_msg_time", "e.first_agent_reply_time")
});

/// ⚠️ **每个 `SUM` 都套 `CAST(... AS SIGNED)`**：MySQL 的 `SUM(布尔)` 返回 `DECIMAL`
/// 而不是整数，直接按 `i64` 取会在运行期报「类型不兼容」。踩过一次。
static SUMMARY_COUNTS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "SELECT COUNT(*) AS events, COUNT(DISTINCT e.roomid) AS rooms, \
     CAST(SUM(e.asker_role = 'EXTERNAL') AS SIGNED) AS merchant, \
     CAST(SUM(e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NOT NULL) AS SIGNED) AS replied, \
     CAST(SUM(e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NULL) AS SIGNED) AS unreplied, \
     CAST(SUM(e.asker_role <> 'EXTERNAL') AS SIGNED) AS push, \
     CAST(SUM(e.asker_role = 'EXTERNAL' AND (e.first_agent_reply_time IS NULL \
         OR {sec} > ?)) AS SIGNED) AS overdue, \
     CAST(SUM(e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NULL AND e.occurred_on < ?) AS SIGNED) AS backlog, \
     CAST(SUM(DATE(e.last_msg_time) <> e.occurred_on) AS SIGNED) AS cross_day \
     FROM b_merchant_group_event e",
        sec = *FIRST_REPLY_SEC
    )
});

/// 首响分位数 —— **必须逐字复刻前端 `quantile()` 的定义，不能用「标准分位数」公式。**
///
/// 前端：`i = min(len - 1, floor(len * p))`，0-based 下标，取那一个**原始值**
/// （不插值）。翻成 1-based 的行号就是 `LEAST(n, FLOOR(n * p) + 1)`。
///
/// 换成别的分位数定义，同一份数据在页面和报表上会出两个不同的数 ——
/// **那比慢糟得多**：没有人会发现，而两边都"看起来对"。
/// 落地时必须有一条测试拿同一批数据对拍两边。
///
/// 空集合返回 `NULL` 而不是 0（承重不变量 4 的形状：没有已回复事件时算不出分位数）。
static SUMMARY_QUANTILES: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "SELECT \
     MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.5) + 1) THEN secs END) AS p50, \
     MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.9) + 1) THEN secs END) AS p90 \
     FROM (SELECT {sec} AS secs, \
                  ROW_NUMBER() OVER (ORDER BY {sec}) AS rn, \
                  COUNT(*) OVER () AS n \
           FROM b_merchant_group_event e",
        sec = *FIRST_REPLY_SEC
    )
});

/// [`SUMMARY_COUNTS`] 那条 SELECT 的九列，顺序即列顺序。
///
/// ⚠️ **后七个是 `Option`**：`SUM` 在空集合上返回 `NULL`。这里的 `NULL` 语义是
/// 「窗口内确实没有这类事件」= 0（`COUNT(*)` 已经把「有没有事件」这件事说清楚了），
/// 与承重不变量 4 的「NULL = 没算出来」是两回事 —— 别把这两种 NULL 混起来读。
type SummaryCounts = (
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// `by_day` 那条 SELECT 的七列，顺序即列顺序。
/// 后四个是 `Option`：`SUM` 在空组上返回 `NULL`，分位数没有已回复事件时也是 `NULL`。
type DayAgg = (
    NaiveDate,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// 概览 KPI ＋ 按天趋势 ＋ 类型分布。**只回几十行，且不随窗口变大。**
///
/// 四条查询各扫一遍窗口，都靠 `idx_overview` 覆盖（`agents` 那条例外 ——
/// JSON 列进不了索引，必须回表，是已知且有意的）。
/// 送出去的行数：KPI 1 行 ＋ 分位数 1 行 ＋ 天数 ＋ 类型数。
///
/// ⚠️ **每个数字都必须和前端 `aggregate()` 对得上**。这是新老并存期最重要的验收：
/// 同一批数据、同一组筛选，两边逐个数字相等才算搬对了。
pub(super) async fn read_summary(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    buckets: &[u32],
) -> Result<Value, WebError> {
    // `backlog` 的边界是窗口最后一天，与前端 `isBacklog(e, lastDay)` 同一个 `lastDay`。
    // **七条查询各自建一个 [`Scope`]** —— 它们是独立语句，共用文本但不共用绑定。
    let (sql, binds) = summary_counts_sql(corp, since, until, sla_sec, filters);
    let counts: SummaryCounts = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_one(&mut *connection)
        .await?;
    // 分位数只看**已回复的商家事件**，所以筛选片段排在那两个固定条件之前。
    let (sql, binds) = summary_quantiles_sql(corp, since, until, sla_sec, filters);
    let (p50, p90): (Option<i64>, Option<i64>) =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_one(&mut *connection)
            .await?;
    // 客服人数：全部事件的 `agents` 并集（不只商家发起的那些），对齐前端 `people`。
    //
    // ⚠️ **`JSON_TABLE` 只能引用同一层 `FROM` 里的表，不能引用派生表的列**
    // （报 1210 Incorrect arguments），所以展开的 `JOIN` 必须夹在分母边界与 `WHERE`
    // 之间 —— [`Scope`] 把分母边界、窗口、筛选拆成三个预置片段，正好插得进去。
    let (sql, binds) = summary_agents_sql(corp, since, until, sla_sec, filters);
    let (agents,): (i64,) = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_one(&mut *connection)
        .await?;
    // 来源消息数 —— 「当前匹配事件一共引用了多少条原文」。按 (企业, 群, msg_id) 去重：
    // 同一条消息被多个事件引用只算一次，所以它**不是**各事件 `source_msg_ids` 的长度之和。
    //
    // ⚠️ 和上面那条一样，`JSON_TABLE` 必须和基表同层 JOIN。
    let (sql, binds) = summary_source_messages_sql(corp, since, until, sla_sec, filters);
    let (source_messages,): (i64,) = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_one(&mut *connection)
        .await?;
    // 按天趋势 —— 行数 = 窗口天数，几十行。
    //
    // ⚠️ **分位数必须按天现算，不能由窗口的 p50 摊下去，也不能取每日 p50 的平均** ——
    // 分位数不可加，这是硬口径。群抽屉的「每日首响」两条折线读的就是这两列：
    // 它带 `room=` 筛选打这个接口一次，而不是每天各打一次。
    //
    // ⚠️ **只出事件级的列**：消息数 / 发言人数在群日表里，前端拿 `groupDaily`
    // 按天求和即可，在这里重算一遍只会多一处口径。
    let (sql, binds) = summary_by_day_sql(corp, since, until, sla_sec, filters);
    let by_day: Vec<DayAgg> = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_all(&mut *connection)
        .await?;
    // 事件到达节奏 —— 24 行封顶。按**首条消息**的小时归入，与前端
    // `e.first_msg_time.slice(11, 13)` 同一个取法（库里的 DATETIME 即业务本地时间）。
    // `overdue` 的口径与 KPI 那个完全一致：未回复也算超时。
    let (sql, binds) = summary_by_hour_sql(corp, since, until, sla_sec, filters);
    let by_hour: Vec<(i64, i64, Option<i64>)> =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_all(&mut *connection)
            .await?;
    let reply_buckets = read_reply_buckets(
        &mut *connection,
        corp,
        since,
        until,
        sla_sec,
        filters,
        buckets,
    )
    .await?;

    let (events, rooms, merchant, replied, unreplied, push, overdue, backlog, cross_day) = counts;
    // `SUM` 在空集合上返回 NULL；这里的语义是「窗口内确实没有这类事件」= 0，
    // 而不是「没算出来」—— 空窗口在 `events = 0` 那个数上已经表达清楚了。
    let n = |v: Option<i64>| v.unwrap_or(0);
    let (merchant, replied) = (n(merchant), n(replied));
    // 比率的分母为零时给 null 不给 0（承重不变量 4 的形状），与前端一致。
    let rate = |x: i64| (merchant > 0).then(|| x as f64 / merchant as f64);
    Ok(json!({
        "events": events, "rooms": rooms, "agents": agents,
        "sourceMessages": source_messages,
        "merchant": merchant, "replied": replied, "unreplied": n(unreplied),
        "push": n(push), "p50": p50, "p90": p90,
        "overdue": n(overdue), "overdueRate": rate(n(overdue)),
        "unrepliedRate": rate(n(unreplied)),
        "backlog": n(backlog), "crossDay": n(cross_day),
        // 缺的那些天由前端补零 —— 后端只出「确实有事件的天」，不替前端造空行。
        "byDay": by_day.into_iter().map(|(day, events, merchant, unreplied, overdue, p50, p90)| {
            let m = n(merchant);
            json!({"day": day.to_string(), "events": events,
                   "merchant": m, "unreplied": n(unreplied), "overdue": n(overdue),
                   // 分母为零给 null 不给 0，与顶部那个超时率同一条规矩。
                   "overdueRate": (m > 0).then(|| n(overdue) as f64 / m as f64),
                   "p50": p50, "p90": p90})
        }).collect::<Vec<_>>(),
        // 同理：没有事件的小时不出行，前端补零。
        "byHour": by_hour.into_iter().map(|(hour, events, overdue)| {
            json!({"hour": hour, "events": events, "overdue": n(overdue)})
        }).collect::<Vec<_>>(),
        "replyBuckets": reply_buckets,
    }))
}

/// 首响时长分布 —— **桶边界由前端给**（`RESPONSE_BINS`），后端不自带一份。
///
/// 自带一份就是第二处口径：前端改了分桶而后端没改，两边的直方图会不一样，
/// 而两边都「看起来对」。边界只进 `?`，拼进 SQL 的只有 `WHEN` 分支的**个数**。
///
/// 区间语义逐字复刻前端：第一个桶是闭区间 `[0, b0]`（`secs` 有 `GREATEST(0, ...)`
/// 兜底，不会为负），其余是左开右闭 `(b[i-1], b[i]]`，最后一个是 `(b[last], ∞)`。
/// `CASE` 顺序求值，正好就是这个语义。
///
/// 只统计**已回复的商家事件**，与前端 `replied` 那一层过滤相同。
async fn read_reply_buckets(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    buckets: &[u32],
) -> Result<Vec<i64>, WebError> {
    if buckets.is_empty() {
        return Ok(Vec::new());
    }
    let (sql, binds) = reply_buckets_sql(corp, since, until, sla_sec, filters, buckets);
    let rows: Vec<(i64, i64)> = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_all(&mut *connection)
        .await?;
    // 空桶不出行，这里补零 —— 桶的个数是前端给的，形状必须原样回去。
    let mut out = vec![0i64; buckets.len() + 1];
    for (bucket, n) in rows {
        if let Some(slot) = usize::try_from(bucket).ok().and_then(|i| out.get_mut(i)) {
            *slot = n;
        }
    }
    Ok(out)
}

/// `read_rooms` 那条 SELECT 的八列，顺序即列顺序。
/// 后六个是 `Option`：`SUM` 在空组上返回 `NULL`，分位数在没有已回复事件时也是 `NULL`。
type RoomAgg = (
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// `read_agents` 那条 SELECT 的九列，顺序即列顺序。
///
/// ⚠️ 这里**没有 `unreplied`，是删掉的不是漏掉的**：它此前在 `inv` 上算
/// `first_agent_reply_time IS NULL`，而 `extract::assemble` 保证「无平台回复 ⟹
/// `agents` 为空」，那些事件被 `JSON_TABLE` 展开成 0 行 —— 这一列**结构上恒为 0**。
/// 一个永远是 0 的数字比没有这个数字更糟：它看起来像「这个人没有欠回复的事件」。
/// 团队口径的无响应数在 `/api/summary` 的 `unreplied`，那条是对的。
type AgentAgg = (
    String,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// 把 [`Filters::clause`] 的绑定值按序接到查询上。**顺序即返回顺序**，不要重排。
fn bind_all<'a, O>(
    mut q: sqlx::query::QueryAs<'a, MySql, O, sqlx::mysql::MySqlArguments>,
    binds: Vec<Bind>,
) -> sqlx::query::QueryAs<'a, MySql, O, sqlx::mysql::MySqlArguments> {
    for b in binds {
        q = match b {
            Bind::Str(v) => q.bind(v),
            Bind::Num(v) => q.bind(v),
            Bind::Date(v) => q.bind(v),
        };
    }
    q
}

/// 调用方给的父类分组 → `CASE` 表达式。**后端不认识词表**，只按给的分组数数。
///
/// ⚠️ **必须有 `ELSE`。** 落不进任何分组的 `event_type`（词表外的历史编码、
/// `__untyped__`）在没有 `ELSE` 时是 `NULL`，会被 `WHERE k IS NOT NULL` **整个丢掉**
/// —— 事件占比合计悄悄少一块，而「未归类」那一行显示 0。兜底桶的下标固定是
/// 「组数」，调用方按它解释成「未归类」。
///
/// 动态拼进 SQL 的只有 `IN (?, ?, …)` 的**占位符个数**与组下标，
/// type_id 全走 [`group_binds`]（顺序与这里逐字对应，两个函数必须一起改）。
fn group_key_expr(groups: &[Vec<String>]) -> String {
    if groups.is_empty() {
        return "e.event_type".to_owned();
    }
    let arms = groups
        .iter()
        .enumerate()
        .map(|(i, types)| {
            let holes = vec!["?"; types.len()].join(", ");
            format!("WHEN e.event_type IN ({holes}) THEN '{i}' ")
        })
        .collect::<String>();
    format!("CASE {arms}ELSE '{}' END", groups.len())
}

/// [`group_key_expr`] 那串 `?` 按出现顺序要的值。
fn group_binds(groups: &[Vec<String>]) -> impl Iterator<Item = Bind> + use<'_> {
    groups.iter().flatten().map(|t| Bind::Str(t.clone()))
}

/// 分位数的分组版 —— `PARTITION BY` 换成分组键，其余与 [`SUMMARY_QUANTILES`] 逐字相同。
///
/// 同一条 `LEAST(n, FLOOR(n * p) + 1)` 复刻前端 `quantile()` 的 `min(len-1, floor(len*p))`。
/// **两处必须一起改** —— 一处改了另一处没改，同一份数据在总览和明细上会出两个数。
fn grouped_quantiles(key: &str, source: &str) -> String {
    format!(
        "SELECT {key}, MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.5) + 1) THEN secs END) AS p50, \
         MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.9) + 1) THEN secs END) AS p90 \
         FROM (SELECT {key}, secs, ROW_NUMBER() OVER (PARTITION BY {key} ORDER BY secs) AS rn, \
                      COUNT(*) OVER (PARTITION BY {key}) AS n \
               FROM {source}) w GROUP BY {key}"
    )
}

/// 按群一行 —— **1000 个群就是 1000 行，不随窗口变大**。
///
/// ⚠️ **只出「必须从明细算」的那几项**。`msg_count` / `sender_count` / 每日事件数 /
/// 各种覆盖率天数，`b_merchant_group_metric_daily` 里全都有，而群日行是
/// 「群数 × 天数」= 1000 群 7 天才 7000 行，本来就不会爆 —— 让前端拿群日表拼，
/// 比在这里重算一遍便宜，也不会出现两处口径打架。
///
/// 真正非算不可的只有四项：**分位数不可加**（不能对每天的 p50 求平均，这是硬口径），
/// `overdue` 依赖调用方给的超时线（群日表没存），`backlog` 依赖窗口最后一天，
/// 以及三个计数（顺手一起出，省前端一次遍历）。
pub(super) async fn read_rooms(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    groups: &[Vec<String>],
) -> Result<Vec<Value>, WebError> {
    let (sql, binds) = rooms_sql(corp, since, until, sla_sec, filters);
    let rows: Vec<RoomAgg> = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_all(&mut *connection)
        .await?;
    // 群 × 日事件数 —— 概览的热力图要它。行数是 `群数 × 天数`（1000 群 7 天 = 7000），
    // 和群日表同量级，**不随每群事件数变多而变大**。
    //
    // ⚠️ **只出「确实有事件的格子」**：某天没事件不出行。前端拿 `groupDaily` 判
    // 那个格子是「真的 0」还是「抽取失败 / 无记录」—— 那个判断只有群日表答得了，
    // 在这里补零会把失败的格子伪装成 0。
    let (sql, binds) = room_cells_sql(corp, since, until, sla_sec, filters);
    let cells: Vec<(String, NaiveDate, i64)> =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_all(&mut *connection)
            .await?;
    let mut series: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for (roomid, day, events) in cells {
        series
            .entry(roomid)
            .or_default()
            .push(json!({"day": day.to_string(), "events": events}));
    }
    let mut top = read_room_top_groups(
        &mut *connection,
        corp,
        since,
        until,
        sla_sec,
        filters,
        groups,
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(roomid, events, merchant, unreplied, overdue, backlog, p50, p90)| {
                let (m, u, o) = (
                    merchant.unwrap_or(0),
                    unreplied.unwrap_or(0),
                    overdue.unwrap_or(0),
                );
                let days = series.remove(&roomid).unwrap_or_default();
                let groups = top.remove(&roomid).unwrap_or_default();
                // 分母为零给 null 不给 0 —— 与前端一致，「没有商家事件」不能读成「零超时率」。
                json!({"roomid": roomid, "events": events, "merchant": m, "unreplied": u,
                   "unrepliedRate": (m > 0).then(|| u as f64 / m as f64),
                   "overdue": o, "overdueRate": (m > 0).then(|| o as f64 / m as f64),
                   "backlog": backlog.unwrap_or(0), "p50": p50, "p90": p90,
                   "series": days, "topGroups": groups})
            },
        )
        .collect())
}

/// 每群事件最多的前四个一级分类 —— 群表上那排类型标签。
///
/// ⚠️ **不能改成「先取全部 (群 × 分类) 再在前端截前四」**：那个集合的行数是
/// 「实际出现过的组合数」，没有上界。`ROW_NUMBER` 在数据库里截断，
/// 回来的行数**恒等于 `群数 × 4`**。
///
/// 分组键与 [`read_categories`] 用的是同一套 `groups`（调用方给的父类分组），
/// 后端一样不认识词表；不给 `groups` 时按 `event_type` 分。
async fn read_room_top_groups(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    groups: &[Vec<String>],
) -> Result<std::collections::BTreeMap<String, Vec<Value>>, WebError> {
    let (sql, binds) = room_top_groups_sql(corp, since, until, sla_sec, filters, groups);
    let rows: Vec<(String, String, i64)> =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_all(&mut *connection)
            .await?;
    let mut out: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
    for (roomid, key, count) in rows {
        out.entry(roomid)
            .or_default()
            .push(json!({"key": key, "count": count}));
    }
    Ok(out)
}

/// 按客服一行 —— 行数是客服数，几百。
///
/// ⚠️ **这里混着两个口径，前端 `agentRollup` 就是这么定的，不能自作主张统一：**
///   * **参与**（`involved` / `rooms`）—— 事件的 `agents` 数组里有他；
///   * **首响归属**（`owned` / `merchantOwned` / 分位数 / `overdue`）——
///     `first_responder = 他`。一个事件只归属一个人，但可以有多个参与者。
///
/// 混用会静默改数：把参与当归属，工作量会被重复计到每个参与者头上，
/// `SUM(owned) > 事件总数`。
///
/// ⚠️ **`JSON_TABLE` 必须和基表同一层 `JOIN`**，不能引用 CTE 或派生表的列
/// （MySQL 报 1210）—— 所以展开写在 CTE 内部而不是外面。
/// 这也是唯一要回表的一条：`agents` 是 JSON 列，进不了 `idx_overview`。
///
/// ⚠️ **`first_responder = agent` 必须显式 `COLLATE`。** `JSON_TABLE` 吐出的列跟
/// **连接的默认 collation**（8.0 是 `utf8mb4_0900_ai_ci`），而表列是建表时定的
/// `utf8mb4_general_ci`（见 `schema.sql`）—— 直接比较报 1267「Illegal mix of
/// collations」。踩过一次。改建表 collation 之前，这里跟着表走。
pub(super) async fn read_agents(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> Result<Vec<Value>, WebError> {
    // SLA 是**后缀绑定**：它在最外层的分组统计里，排在 CTE 与筛选片段之后。
    let (sql, binds) = agents_sql(corp, since, until, sla_sec, filters);
    let rows: Vec<AgentAgg> = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_all(&mut *connection)
        .await?;
    // 客服 × 日 —— 抽屉里的两条折线。行数是 `客服数 × 天数`，几百。
    // 缺的天不出行：那天是「真的 0」还是「群日不完整」由前端拿 `groupDaily` 判。
    let (sql, binds) = agent_cells_sql(corp, since, until, sla_sec, filters);
    let cells: Vec<(String, NaiveDate, i64, Option<i64>)> =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_all(&mut *connection)
            .await?;
    // 客服参与过的群 —— 抽屉的分群明细与「数据完整性」那一列要它。
    //
    // ⚠️ **不用 `GROUP_CONCAT` 塞进主查询**：那个有 `group_concat_max_len`（默认 1KB）
    // 的**静默截断**，群多起来会无声丢掉一部分群，而页面照样渲染。宁可多一条查询。
    let (sql, binds) = agent_memberships_sql(corp, since, until, sla_sec, filters);
    let memberships: Vec<(String, String)> =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_all(&mut *connection)
            .await?;
    let mut series: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for (agent, day, involved, owned) in cells {
        series.entry(agent).or_default().push(
            json!({"day": day.to_string(), "involved": involved, "owned": owned.unwrap_or(0)}),
        );
    }
    let mut room_ids: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (agent, roomid) in memberships {
        room_ids.entry(agent).or_default().push(roomid);
    }
    Ok(rows
        .into_iter()
        .map(
            |(agent, involved, rooms, owned, merchant_owned, reply_samples, overdue, p50, p90)| {
                let (mo, o) = (merchant_owned.unwrap_or(0), overdue.unwrap_or(0));
                let days = series.remove(&agent).unwrap_or_default();
                let mine = room_ids.remove(&agent).unwrap_or_default();
                json!({"agent": agent, "involved": involved, "rooms": rooms,
                       "owned": owned.unwrap_or(0), "merchantOwned": mo,
                       "replySamples": reply_samples.unwrap_or(0), "overdue": o,
                       "overdueRate": (mo > 0).then(|| o as f64 / mo as f64),
                       "p50": p50, "p90": p90,
                       "series": days, "roomIds": mine})
            },
        )
        .collect())
}

/// `read_categories` 那条 SELECT 的六列，顺序即列顺序。
/// 后四个是 `Option`：`SUM` 在空组上返回 `NULL`，分位数在没有已回复事件时也是 `NULL`。
type CategoryAgg = (
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// 分类汇总 —— **后端不认识词表，只按调用方给的分组键数数**。
///
/// 二级（不给 `groups`）按 `event_type` 分组，`key` 就是 `type_id`。
/// 一级（给 `groups`）按调用方给的分组分，`key` 是**组下标**，前端自己映射回父类名。
///
/// ⚠️ **父类映射为什么不在这里 join 词表**：词表已经随 `/api/meta` 到了前端，
/// 在这里再 join 一次就多出一处能和前端打架的口径（`Filters` 的 `level1`
/// 不下推是同一条理由）。后端在这里只当一个「按你给的分组算」的计算器。
///
/// ⚠️ **一级分位数必须在这里算，不能由二级的合并出来** —— 分位数不可加。
/// 这正是 `groups` 存在的理由：前端没法把几个二级的 p50 合成一级的 p50。
///
/// `event_type IS NULL`（打标未完成）的事件**整个排除**，与前端 `categoryRollup`
/// 里那句 `if (e.event_type === null) continue` 一致。占比的分母是事件总数，
/// 由前端拿 `/api/summary` 的 `events` 去除 —— 那个数在那边已经有了。
pub(super) async fn read_categories(
    connection: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    groups: &[Vec<String>],
) -> Result<Vec<Value>, WebError> {
    let (sql, binds) = categories_sql(corp, since, until, sla_sec, filters, groups);
    let rows: Vec<CategoryAgg> = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_all(&mut *connection)
        .await?;
    // 分类 × 日 —— 事件洞察那一排小折线。行数是 `分类数 × 天数`，几百。
    // 与群 / 客服的 series 一样，**没有事件的格子不出行**，缺口由前端按覆盖度断。
    let (sql, binds) = category_cells_sql(corp, since, until, sla_sec, filters, groups);
    let cells: Vec<(String, NaiveDate, i64)> =
        bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
            .fetch_all(&mut *connection)
            .await?;
    let mut series: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for (key, day, events) in cells {
        series
            .entry(key)
            .or_default()
            .push(json!({"day": day.to_string(), "events": events}));
    }
    Ok(rows
        .into_iter()
        .map(|(key, count, merchant, unreplied, p50, p90)| {
            let (m, u) = (merchant.unwrap_or(0), unreplied.unwrap_or(0));
            let days = series.remove(&key).unwrap_or_default();
            json!({"key": key, "count": count, "merchant": m, "unreplied": u,
                   "unrepliedRate": (m > 0).then(|| u as f64 / m as f64),
                   "p50": p50, "p90": p90, "series": days})
        })
        .collect())
}

// ─────────────────────────────────────────────────────────────────────────────
// 这四个函数是从 `serve.rs` 收回来的 —— 它们的 SQL 曾经写在 handler 里，
// 而本文件顶上那句「写库 SQL 一条都不许出现在这个文件里」的对偶（只读 SQL 全在这里）
// 因此是假的：事件文档的那串 `JSON_OBJECT` 和 `Bind` 得导出去给 handler 拼，SQL 拼装的一半在那边。
// 收回来之后「webUI 打了哪些表」是一个文件回答得了的问题。
// ─────────────────────────────────────────────────────────────────────────────

/// 窗口里有没有**已发布标签、但用的不是当前词表**的事件。
///
/// **一条 `EXISTS` 就够，不用把事件拉下来比对**：两个版本的分类混在一张表上，
/// 每个数字都看起来正常，但没有一个能对，所以整页拒绝渲染。
pub(super) async fn has_mixed_version(
    tx: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    version: &str,
) -> Result<bool, WebError> {
    let (mixed,): (i64,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM b_merchant_group_event e \
         LEFT JOIN b_merchant_group_metric_daily g \
           ON g.corpid = e.corpid AND g.roomid = e.roomid AND g.dt = e.occurred_on \
         WHERE e.corpid = ? AND e.occurred_on BETWEEN ? AND ? \
           AND (g.classification_status IS NULL OR g.classification_status NOT IN ('pending','failed')) \
           AND e.taxonomy_version IS NOT NULL AND e.taxonomy_version <> ?)",
    )
    .bind(corp)
    .bind(since)
    .bind(until)
    .bind(version)
    .fetch_one(&mut *tx)
    .await?;
    Ok(mixed != 0)
}

/// 群日记录 —— `群数 × 天数`（1000 群 7 天 = 7000 行），**本来就不会爆**。
/// 它是唯一能回答「这个格子是真的 0 还是抽取失败」的东西，聚合接口替代不了。
pub(super) async fn read_group_days(
    tx: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    budget: &mut ResponseBudget,
) -> Result<(), WebError> {
    let mut rows = sqlx::query(
        "SELECT CAST(JSON_OBJECT('corpid', g.corpid, 'roomid', g.roomid, 'dt', DATE_FORMAT(g.dt, '%Y-%m-%d'), \
         'msg_count', g.msg_count, 'sender_count', g.sender_count, 'event_count', g.event_count, \
         'merchant_event_count', g.merchant_event_count, 'unreplied_count', g.unreplied_count, \
         'first_reply_p50_sec', g.first_reply_p50_sec, 'first_reply_p90_sec', g.first_reply_p90_sec, \
         'extraction_status', g.extraction_status, 'classification_status', g.classification_status, \
         'freshness', IF(g.fact_completed_time IS NULL OR \
         (SELECT MAX(f.gmt_created_time) FROM b_merchant_group_run_failure f WHERE f.corpid=g.corpid AND f.roomid=g.roomid AND f.stage='extract' \
           AND (f.window_since IS NULL OR g.dt BETWEEN f.window_since AND f.window_until)) \
         >= g.fact_completed_time, 'unknown', 'known')) AS CHAR) AS document \
         FROM b_merchant_group_metric_daily g WHERE g.corpid = ? AND g.dt BETWEEN ? AND ? ORDER BY g.dt, g.roomid"
    ).bind(corp).bind(since).bind(until).fetch(&mut *tx);
    // 数据库已经把每一行渲染成最终 JSON 了 —— **直推字节，不解析**（见 `budget` 的模块文档）。
    // 数组的逗号在这里出，因为只有这里知道还有没有下一行。
    let mut first = true;
    while let Some(row) = rows.try_next().await? {
        if !std::mem::take(&mut first) {
            budget.frame(",")?;
        }
        budget.document(&row.try_get::<String, _>("document")?)?;
    }
    Ok(())
}

/// 明细一页。筛选片段进**内层**（覆盖索引那一层），外层只按 id 回表。
///
/// **收整个 [`Paging`]，不收拆开的 clause / order / binds** —— 绑定顺序
/// （`corp` → `since` → `until` → 动态 binds → `offset` → `limit`）是这条 SQL 的内部
/// 契约，拆开传等于把它抬到调用方的签名上，而 handler 一个字都不该知道。
///
/// ⚠️ 中间那段展开**不能复用 [`bind_all`]** —— 那个收的是 `QueryAs`（聚合接口要
/// `fetch` 成元组），这里是裸 `Query`（结果是一列 JSON 文本）。sqlx 没给这两者
/// 一个共同的 trait，硬造一个泛型包装比多写四行贵。两份都在本文件里，改 [`Bind`]
/// 时看得见彼此 —— 此前另一份在 `serve.rs` 的 handler 里。
pub(super) async fn read_event_page(
    tx: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    paging: &Paging,
    budget: &mut ResponseBudget,
) -> Result<(), WebError> {
    let (sql, binds) = event_page_sql(corp, since, until, paging)?;
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for b in binds {
        q = match b {
            Bind::Str(v) => q.bind(v),
            Bind::Num(v) => q.bind(v),
            Bind::Date(v) => q.bind(v),
        };
    }
    let mut rows = q.fetch(&mut *tx);
    let mut first = true;
    while let Some(row) = rows.try_next().await? {
        if !std::mem::take(&mut first) {
            budget.frame(",")?;
        }
        budget.document(&row.try_get::<String, _>("document")?)?;
    }
    Ok(())
}

/// 这一页所属集合的**总数** —— 用内层那条 `WHERE` 单独跑一次，不带排序不带 `LIMIT`，
/// 由调用方放在同一个只读快照事务里，于是总数与行必然同集合。
///
/// ⚠️ **不按已知成功群日过滤**，与 [`read_event_page`] 同一条件 —— 明细是核实工具，
/// 抽取失败的群日上抽出了什么正是要看的。此前前端借 `/api/summary` 的 `events`
/// 当总数，而那个数只算已知成功群日：窗口里一有失败或凭据未知的群日，两个数就不等，
/// 前端取较小值那步把人夹在更早的页码上，**尾部的行永远翻不到，页面看起来一切正常**。
///
/// ⚠️ **这是新增成本。** 内层扫描上界原本是「最大页码 × 每页上限」个索引条目，
/// 加计数之后要扫整个筛选后的窗口。撞服务端执行上限时返回既有的 504
/// 「查询超时，请缩小日期范围」—— **不抬超时**，那个值是护栏不是产能规划
/// （见 `WebLimits::query_timeout_secs`）。
pub(super) async fn count_events(
    tx: &mut MySqlConnection,
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    paging: &Paging,
) -> Result<u64, WebError> {
    let (sql, binds) = count_events_sql(corp, since, until, paging);
    let (total,): (i64,) = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_one(&mut *tx)
        .await?;
    Ok(total.max(0) as u64)
}

/// 单个事件的**预渲染文档 ＋ 词表是否混版**。
/// **打 pool 不走快照事务** —— 一次主键查找，不需要跨表一致性。
///
/// ⚠️ **混版判定是旁边一列，不塞进文档里。** 塞进去要么动明细分页那条脆弱的绑定顺序
/// （两处共用 [`EVENT_DOCUMENT`]），要么让读取端把文档解析回来用字符串下标摸字段 ——
/// 而那正是 ⑤ 要消掉的那棵值树。一列布尔回来，读取端一个 JSON 都不用解析。
///
/// 判定逐条对应此前 `serve.rs` 里那两句：**已发布标签**（本群打标未完成时文档里的
/// `taxonomy_version` 是 NULL，这里用 `classification_status` 复现同一条件；
/// 没有群日行时 `COALESCE` 给空串，与文档里 `NULL IN (...)` 求值为假一致）
/// **且版本不是当前版本**。未打标不算不一致。
///
/// 当前版本**走绑定不拼字面量** —— 不是安全考虑（`CURRENT_VERSION` 是编译期常量），
/// 是「哪些拼哪些绑」不要多一条例外。
pub(super) async fn read_event(
    pool: &MySqlPool,
    corp: &str,
    id: u64,
    version: &str,
) -> Result<Option<(String, bool)>, WebError> {
    // 文本与绑定成对进出（见 `scope.rs`）—— 版本在 SELECT 列表里、排在 corpid / id
    // 之前这件事因此不用写在注释里，也不会有人在中间插一段忘了配绑定。
    let (sql, binds) = Scope::new()
        .push(
            &format!(
                "SELECT {EVENT_DOCUMENT}, \
                 (COALESCE(g.classification_status, '') NOT IN ('pending','failed') \
                  AND e.taxonomy_version IS NOT NULL AND e.taxonomy_version <> ?) AS stale \
                 FROM b_merchant_group_event e LEFT JOIN b_merchant_group_metric_daily g \
                 ON g.corpid = e.corpid AND g.roomid = e.roomid AND g.dt = e.occurred_on"
            ),
            [Bind::Str(version.to_owned())],
        )
        .push(
            " WHERE e.corpid = ? AND e.id = ?",
            [Bind::Str(corp.to_owned()), Bind::Num(id)],
        )
        .finish();
    let row: Option<(String, i64)> = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binds)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(document, stale)| (document, stale != 0)))
}

/// 来源消息原文 —— **只读 `source_messages` 一列，不碰文件系统。**
/// 外层 `Option` = 事件在不在，内层 `Option` = 这一行有没有这列（NULL 是永久取不到）。
pub(super) async fn read_source_messages(
    pool: &MySqlPool,
    corp: &str,
    id: u64,
) -> Result<Option<Option<String>>, WebError> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT source_messages FROM b_merchant_group_event WHERE corpid = ? AND id = ?",
    )
    .bind(corp)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(rendered,)| rendered))
}

/// [`count_events`] 的语句 —— 与 [`event_page_sql`] 的内层用**同一条** `WHERE`。
fn count_events_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    paging: &Paging,
) -> (String, Vec<Bind>) {
    Scope::new()
        .push("SELECT COUNT(*) FROM b_merchant_group_event e", [])
        .window(corp, since, until)
        .filters(
            &paging.filters,
            paging.sla_sec.unwrap_or(DEFAULT_SLA_SEC),
            until,
        )
        .finish()
}
// ─────────────────────────────────────────────────────────────────────────────
// 语句构造 —— 每条聚合语句的文本与绑定由一个 [`Scope`] 一次产出。
//
// 抽成具名函数**是为了能离线断言**：`Scope::finish` 的 `assert!` 在这里就跑得起来，
// 不用连数据库（见本文件末尾的 `binding_tests`）。此前 SQL 文本与绑定在两处分别
// 组装、靠七条「顺序错了不会报错，只会算错」的注释保持同步 —— 那些注释已经删了，
// 它们守的东西现在由 `scope.rs` 的 interface 守着。
// ─────────────────────────────────────────────────────────────────────────────

fn summary_counts_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    Scope::new()
        .push(
            &SUMMARY_COUNTS,
            [Bind::Num(sla_sec.into()), Bind::Date(until)],
        )
        .known_ok_days(corp, since, until)
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .finish()
}

fn summary_quantiles_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    Scope::new()
        .push(&SUMMARY_QUANTILES, [])
        .known_ok_days(corp, since, until)
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .push(
            " AND e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NOT NULL) q",
            [],
        )
        .finish()
}

fn summary_agents_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    Scope::new()
        .push(
            "SELECT COUNT(DISTINCT a.agent) FROM b_merchant_group_event e",
            [],
        )
        .known_ok_days(corp, since, until)
        .push(
            " JOIN JSON_TABLE(e.agents, '$[*]' COLUMNS(agent VARCHAR(16) PATH '$')) a",
            [],
        )
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .finish()
}

fn summary_source_messages_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    Scope::new()
        .push(
            "SELECT COUNT(DISTINCT e.corpid, e.roomid, m.msg_id) FROM b_merchant_group_event e",
            [],
        )
        .known_ok_days(corp, since, until)
        .push(
            " JOIN JSON_TABLE(e.source_msg_ids, '$[*]' COLUMNS(msg_id VARCHAR(64) PATH '$')) m",
            [],
        )
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .finish()
}
fn summary_by_day_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    let day_quantiles = grouped_quantiles(
        "occurred_on",
        "ev WHERE asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL",
    );
    Scope::new()
    .push(
        &format!(
            "WITH ev AS (SELECT e.occurred_on, e.asker_role, e.first_agent_reply_time, \
               {sec} AS secs FROM b_merchant_group_event e",
            sec = *FIRST_REPLY_SEC
        ),
        [],
    )
    .known_ok_days(corp, since, until)
    .window(corp, since, until)
    .filters(filters, sla_sec, until)
    .push(
        &format!(
            ") SELECT c.occurred_on, c.events, c.merchant, c.unreplied, c.overdue, p.p50, p.p90 \
             FROM (SELECT occurred_on, COUNT(*) AS events, \
                     CAST(SUM(asker_role = 'EXTERNAL') AS SIGNED) AS merchant, \
                     CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL) AS SIGNED) AS unreplied, \
                     CAST(SUM(asker_role = 'EXTERNAL' AND (first_agent_reply_time IS NULL \
                         OR secs > ?)) AS SIGNED) AS overdue \
                   FROM ev GROUP BY occurred_on) c \
             LEFT JOIN ({day_quantiles}) p ON p.occurred_on = c.occurred_on \
             ORDER BY c.occurred_on"
        ),
        [Bind::Num(sla_sec.into())],
    )
    .finish()
}

fn summary_by_hour_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    Scope::new()
        .push(
            &format!(
                "SELECT HOUR(e.first_msg_time) AS h, COUNT(*) AS events, \
             CAST(SUM(e.asker_role = 'EXTERNAL' AND (e.first_agent_reply_time IS NULL \
                 OR {sec} > ?)) AS SIGNED) AS overdue \
             FROM b_merchant_group_event e",
                sec = *FIRST_REPLY_SEC
            ),
            [Bind::Num(sla_sec.into())],
        )
        .known_ok_days(corp, since, until)
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .push(" GROUP BY h ORDER BY h", [])
        .finish()
}

fn reply_buckets_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    buckets: &[u32],
) -> (String, Vec<Bind>) {
    let arms = buckets
        .iter()
        .enumerate()
        .map(|(i, _)| format!("WHEN {sec} <= ? THEN {i} ", sec = *FIRST_REPLY_SEC))
        .collect::<String>();
    let last = buckets.len();
    Scope::new()
        .push(
            &format!(
                "SELECT b.bucket, COUNT(*) AS n FROM (\
             SELECT CASE {arms}ELSE {last} END AS bucket \
             FROM b_merchant_group_event e"
            ),
            buckets.iter().map(|e| Bind::Num(u64::from(*e))),
        )
        .known_ok_days(corp, since, until)
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .push(
            " AND e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NOT NULL) b \
         GROUP BY b.bucket ORDER BY b.bucket",
            [],
        )
        .finish()
}

fn rooms_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    let quantiles = grouped_quantiles(
        "roomid",
        "ev WHERE asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL",
    );
    Scope::new()
    .push(
        &format!(
            "WITH ev AS (SELECT e.roomid, e.asker_role, e.first_agent_reply_time, e.occurred_on, \
               {sec} AS secs FROM b_merchant_group_event e",
            sec = *FIRST_REPLY_SEC
        ),
        [],
    )
    .known_ok_days(corp, since, until)
    .window(corp, since, until)
    .filters(filters, sla_sec, until)
    .push(
        &format!(
            ") SELECT c.roomid, c.events, c.merchant, c.unreplied, c.overdue, c.backlog, p.p50, p.p90 \
             FROM (SELECT roomid, COUNT(*) AS events, \
                     CAST(SUM(asker_role = 'EXTERNAL') AS SIGNED) AS merchant, \
                     CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL) AS SIGNED) AS unreplied, \
                     CAST(SUM(asker_role = 'EXTERNAL' AND (first_agent_reply_time IS NULL OR secs > ?)) AS SIGNED) AS overdue, \
                     CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL AND occurred_on < ?) AS SIGNED) AS backlog \
                   FROM ev GROUP BY roomid) c \
             LEFT JOIN ({quantiles}) p ON p.roomid = c.roomid ORDER BY c.roomid"
        ),
        [Bind::Num(sla_sec.into()), Bind::Date(until)],
    )
    .finish()
}

fn room_cells_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    Scope::new()
        .push(
            "SELECT e.roomid, e.occurred_on, COUNT(*) AS events FROM b_merchant_group_event e",
            [],
        )
        .known_ok_days(corp, since, until)
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .push(
            " GROUP BY e.roomid, e.occurred_on ORDER BY e.roomid, e.occurred_on",
            [],
        )
        .finish()
}

fn room_top_groups_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    groups: &[Vec<String>],
) -> (String, Vec<Bind>) {
    let key_expr = group_key_expr(groups);
    Scope::new()
        .push(
            &format!(
                "SELECT roomid, k, n FROM (\
               SELECT c.roomid, c.k, c.n, \
                 ROW_NUMBER() OVER (PARTITION BY c.roomid ORDER BY c.n DESC, c.k) AS rn \
               FROM (SELECT e.roomid, {key_expr} AS k, COUNT(*) AS n \
                     FROM b_merchant_group_event e"
            ),
            group_binds(groups),
        )
        .known_ok_days(corp, since, until)
        .window(corp, since, until)
        .push(" AND e.event_type IS NOT NULL", [])
        .filters(filters, sla_sec, until)
        .push(
            " GROUP BY e.roomid, k) c WHERE c.k IS NOT NULL) r \
         WHERE r.rn <= 4 ORDER BY r.roomid, r.rn",
            [],
        )
        .finish()
}
/// 三条按客服的查询共用的 CTE —— **共用文本，不共用绑定**：它们是三条独立语句，
/// 每条各建一个 [`Scope`]，文本与绑定一起再走一遍。
///
/// ⚠️ **`JSON_TABLE` 必须和基表同一层 `JOIN`**，不能引用 CTE 或派生表的列
/// （MySQL 报 1210）—— 所以展开夹在分母边界与窗口之间。
fn agent_inv_scope(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> Scope {
    Scope::new()
        .push(
            &format!(
                "WITH inv AS (SELECT a.agent, e.id, e.roomid, e.occurred_on, e.asker_role, \
                   e.first_agent_reply_time, \
                   (e.first_responder = a.agent COLLATE utf8mb4_general_ci) AS owned, \
                   {sec} AS secs FROM b_merchant_group_event e",
                sec = *FIRST_REPLY_SEC
            ),
            [],
        )
        .known_ok_days(corp, since, until)
        .push(
            " JOIN JSON_TABLE(e.agents, '$[*]' COLUMNS(agent VARCHAR(16) PATH '$')) a",
            [],
        )
        .window(corp, since, until)
        .filters(filters, sla_sec, until)
        .push(") ", [])
}

fn agents_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    let quantiles = grouped_quantiles(
        "agent",
        "inv WHERE owned AND asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL",
    );
    agent_inv_scope(corp, since, until, sla_sec, filters)
    .push(
        &format!(
            "SELECT c.agent, c.involved, c.rooms, c.owned, c.merchant_owned, c.reply_samples, \
                    c.overdue, p.p50, p.p90 \
             FROM (SELECT agent, COUNT(DISTINCT id) AS involved, COUNT(DISTINCT roomid) AS rooms, \
                     CAST(SUM(owned) AS SIGNED) AS owned, \
                     CAST(SUM(owned AND asker_role = 'EXTERNAL') AS SIGNED) AS merchant_owned, \
                     CAST(SUM(owned AND asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL) AS SIGNED) AS reply_samples, \
                     CAST(SUM(owned AND asker_role = 'EXTERNAL' \
                         AND (first_agent_reply_time IS NULL OR secs > ?)) AS SIGNED) AS overdue \
                   FROM inv GROUP BY agent) c \
             LEFT JOIN ({quantiles}) p ON p.agent = c.agent ORDER BY c.agent"
        ),
        [Bind::Num(sla_sec.into())],
    )
    .finish()
}

fn agent_cells_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    agent_inv_scope(corp, since, until, sla_sec, filters)
        .push(
            "SELECT agent, occurred_on, COUNT(DISTINCT id) AS involved, \
         CAST(SUM(owned) AS SIGNED) AS owned \
         FROM inv GROUP BY agent, occurred_on ORDER BY agent, occurred_on",
            [],
        )
        .finish()
}

fn agent_memberships_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
) -> (String, Vec<Bind>) {
    agent_inv_scope(corp, since, until, sla_sec, filters)
        .push(
            "SELECT DISTINCT agent, roomid FROM inv ORDER BY agent, roomid",
            [],
        )
        .finish()
}
/// 两条分类查询共用的 CTE（汇总一条、按日序列一条）—— **共用文本，不共用绑定**。
///
/// 分组的分支条件是**前缀绑定**：它在 CTE 的 SELECT 列表里，排在分母边界之前。
/// 分组键住在 CTE 的一个列里，外面一律用 `k` —— `CASE` 里的 `?` 因此只出现一次，
/// 不用在 `SELECT` / `PARTITION BY` / `GROUP BY` 三个地方各绑一遍。
fn category_ev_scope(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    groups: &[Vec<String>],
) -> Scope {
    let key_expr = group_key_expr(groups);
    Scope::new()
        .push(
            &format!(
                "WITH ev AS (SELECT {key_expr} AS k, e.occurred_on, e.asker_role, \
                   e.first_agent_reply_time, {sec} AS secs FROM b_merchant_group_event e",
                sec = *FIRST_REPLY_SEC
            ),
            group_binds(groups),
        )
        .known_ok_days(corp, since, until)
        .window(corp, since, until)
        .push(" AND e.event_type IS NOT NULL", [])
        .filters(filters, sla_sec, until)
        .push(") ", [])
}

fn categories_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    groups: &[Vec<String>],
) -> (String, Vec<Bind>) {
    let quantiles = grouped_quantiles(
        "k",
        "ev WHERE asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL",
    );
    category_ev_scope(corp, since, until, sla_sec, filters, groups)
        .push(
            &format!(
                "SELECT c.k, c.count, c.merchant, c.unreplied, p.p50, p.p90 \
                 FROM (SELECT k, COUNT(*) AS count, \
                         CAST(SUM(asker_role = 'EXTERNAL') AS SIGNED) AS merchant, \
                         CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL) AS SIGNED) AS unreplied \
                       FROM ev WHERE k IS NOT NULL GROUP BY k) c \
                 LEFT JOIN ({quantiles}) p ON p.k = c.k ORDER BY c.k"
            ),
            [],
        )
        .finish()
}

fn category_cells_sql(
    corp: &str,
    since: NaiveDate,
    until: NaiveDate,
    sla_sec: u32,
    filters: &Filters,
    groups: &[Vec<String>],
) -> (String, Vec<Bind>) {
    category_ev_scope(corp, since, until, sla_sec, filters, groups)
        .push(
            "SELECT k, occurred_on, COUNT(*) AS events \
             FROM ev WHERE k IS NOT NULL GROUP BY k, occurred_on ORDER BY k, occurred_on",
            [],
        )
        .finish()
}

#[cfg(test)]
mod binding_tests {
    //! **占位符个数 == 绑定个数** —— 每条聚合语句一条，**不需要数据库**。
    //!
    //! 断言本身在 [`Scope::finish`] 里（`assert!`，不是 `debug_assert!`）：
    //! 这里只是把每条语句**造一遍**，让它在默认测试集里跑得到。此前这条不变量
    //! 既不报错也不在默认覆盖内，而它错了只会算错、不会报错。
    //!
    //! 覆盖四类接缝：前缀绑定（分组分支条件 · 直方图桶边界）· 共用 CTE ·
    //! 后缀绑定（SLA 与积压边界）· `JSON_TABLE` 展开夹在分母边界与窗口之间。
    use super::*;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, d).unwrap()
    }

    /// 每条筛选都给上值，`?` 最多的那一版才是最容易错位的那一版。
    fn busy_filters() -> Filters {
        Filters {
            room: Some("R1".into()),
            agent: Some("A1".into()),
            responder: Some("A2".into()),
            types: Some("t1,t2".into()),
            types_exclude: Some("t3".into()),
            status: Some("backlog".into()),
            overdue_only: Some(true),
            q: Some("改期".into()),
        }
    }

    fn groups() -> Vec<Vec<String>> {
        vec![
            vec!["t1".to_owned(), "t2".to_owned()],
            vec!["t3".to_owned()],
        ]
    }

    #[test]
    fn every_aggregate_statement_binds_exactly_its_placeholders() {
        for filters in [Filters::default(), busy_filters()] {
            for groups in [Vec::new(), groups()] {
                let (c, s, u, sla) = ("C", day(1), day(31), 1800);
                // ① 概览的七条
                summary_counts_sql(c, s, u, sla, &filters);
                summary_quantiles_sql(c, s, u, sla, &filters);
                summary_agents_sql(c, s, u, sla, &filters);
                summary_source_messages_sql(c, s, u, sla, &filters);
                summary_by_day_sql(c, s, u, sla, &filters);
                summary_by_hour_sql(c, s, u, sla, &filters);
                reply_buckets_sql(c, s, u, sla, &filters, &[60, 300, 900]);
                // ② 按群三条
                rooms_sql(c, s, u, sla, &filters);
                room_cells_sql(c, s, u, sla, &filters);
                room_top_groups_sql(c, s, u, sla, &filters, &groups);
                // ③ 按客服三条
                agents_sql(c, s, u, sla, &filters);
                agent_cells_sql(c, s, u, sla, &filters);
                agent_memberships_sql(c, s, u, sla, &filters);
                // ④ 分类两条
                categories_sql(c, s, u, sla, &filters, &groups);
                category_cells_sql(c, s, u, sla, &filters, &groups);
            }
        }
    }

    /// 明细那两条走的是同一条内层 `WHERE`，总数与行因此必然同集合。
    #[test]
    fn the_detail_page_and_its_count_bind_exactly_their_placeholders() {
        for (label, paging) in [
            ("默认", Paging::for_test(1, 20, None, Filters::default())),
            (
                "满筛选",
                Paging::for_test(3, 50, Some("wait"), busy_filters()),
            ),
        ] {
            let (page, _) = event_page_sql("C", day(1), day(31), &paging).unwrap();
            let (count, _) = count_events_sql("C", day(1), day(31), &paging);
            // 两条语句的筛选片段必须逐字相同 —— 不同就是两个集合。
            let inner = |sql: &str| {
                sql.split_once("WHERE e.corpid = ?")
                    .unwrap()
                    .1
                    .split(" ORDER BY ")
                    .next()
                    .unwrap()
                    .to_owned()
            };
            assert_eq!(inner(&page), inner(&count), "{label}");
        }
    }

    /// 占位符与绑定**数量对得上还不够，位置也要对** —— 这条盯的是位置：
    /// 分母边界的三个 `?` 必须排在外层窗口那三个之前。
    #[test]
    fn the_denominator_boundary_binds_before_the_outer_window() {
        let (sql, _) = summary_quantiles_sql("C", day(1), day(31), 1800, &Filters::default());
        let boundary = sql.find("b_merchant_group_run_failure").unwrap();
        let outer = sql.find("WHERE e.corpid = ?").unwrap();
        assert!(boundary < outer, "分母边界必须排在外层窗口之前");
    }
}
