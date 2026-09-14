//! HTTP 请求的形状 —— query string 收哪些参数、怎么反序列化、怎么校验。
//!
//! 这一整件事此前散在三个地方：`Paging` / `Period` 在 `query.rs` 的第 255~396 行，
//! `Filters` / `Bind` / `query_num` 在同一个文件的 1389~1553 行，中间隔着**一千行
//! SELECT**；而 `Sla` / `Grouping` 在 `serve.rs`，它们 flatten 的正是前两组。
//! `query.rs` 那 309 行内联测试**每一条测的都是这里的东西**，一并搬了过来。
//!
//! 收成一个文件之后，`query.rs` 只剩 SQL 与取数，`serve.rs` 从 `query` import 的
//! 名字少了一半 —— 而 `query_num` 那个坑（serde flatten 下 `/api/rooms` 与
//! `/api/categories` 曾经对每一个带 `sla_sec` 的请求返回 400）从此和它要修的四个
//! 类型挨在同一屏里。
//!
//! **一条 SQL 都不许出现在这个文件里** —— 拼 SQL 的 `Filters::clause` 是例外，
//! 它产出的是 `WHERE` 片段和一串 `Bind`，值一律走 `?`，不做字符串拼接。

use super::{budget::WebError, query::FIRST_REPLY_SEC};
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde::Deserialize;

/// SLA 超时线的缺省值。**此前是两份独立的硬编码** —— `Sla::sla_sec()` 一份、
/// `serve::events` 里 `paging.sla_sec.unwrap_or(1800)` 另一份，而它们服务的是
/// 同一个看板滑块：改了一处另一处不会跟着变，且错法是静默的（明细和汇总各按
/// 各的线算超时，页面上两个数对不上，没有任何东西会报错）。
pub(super) const DEFAULT_SLA_SEC: u32 = 1800;

/// 库里有数据的日期两端。**只有这一条 MIN/MAX**，两端各走索引，与表多大无关。
///
/// 它从 [`read_meta`] 里拆出来，因为**五个聚合接口只要这两个日期**：
/// 它们唯一用到 `Meta` 的地方是 `Period::bounds`，而 `bounds` 只读两端。
/// 此前每个聚合请求都得先造一个完整 `Meta` —— 连带 `read_taxonomy` 全表读一遍、
/// 再 `check_types` 校验一遍，**结果原样丢掉**。页面点一下筛选就是四个接口，
/// 于是四遍。接口上写着 `Meta`、实际只要两个日期，代价就长在这个差里。
#[derive(Clone, Copy)]
pub(super) struct DateRange {
    pub(super) since: NaiveDate,
    pub(super) until: NaiveDate,
}

/// 不给日期时默认看几天。
///
/// ⚠️ **它是「一次拉多少行明细」的直接乘数**，不是审美偏好：
/// 实测每群每天约 15.8 个事件（2026-09-08，5 群 2 天样本），而 `max_rows` 是 20000 ——
/// 也就是说 `群数 × 天数 × 15.8` 一超就是 413。7 天时临界群数是 181，1 天时是 1266。
/// 群数往上走之前，要么调小它，要么把概览改成只取聚合（后者才是根治）。
const DEFAULT_DAYS: i64 = 7;

/// 明细翻页的上限 —— **护栏，不是产能规划**。
///
/// 延迟关联把每页的常数压小了一个数量级，但没有消掉「偏移量越大扫得越多」。
/// 翻到第 200 页（4000 条）还在翻的人，要的其实是筛选或导出，不是翻页 ——
/// 让它明确说「请缩小范围」，比让它慢慢跑到超时好。
pub(super) const MAX_PAGE: u64 = 200;
pub(super) const PAGE_SIZE_MAX: u64 = 100;

#[derive(Default, Deserialize)]
pub(super) struct Period {
    from: Option<String>,
    to: Option<String>,
}

impl Period {
    pub(super) fn bounds(&self, range: &DateRange) -> Result<(NaiveDate, NaiveDate), WebError> {
        let parse = |value: &str| {
            if value.len() != 10 {
                return Err(WebError(
                    StatusCode::BAD_REQUEST,
                    "日期须为有效的 YYYY-MM-DD".into(),
                ));
            }
            value
                .parse::<NaiveDate>()
                .map_err(|_| WebError(StatusCode::BAD_REQUEST, "日期须为有效的 YYYY-MM-DD".into()))
        };
        let until = match self.to.as_deref() {
            Some(value) => parse(value)?,
            None => range.until,
        };
        let since = match self.from.as_deref() {
            Some(value) => parse(value)?,
            None => (until - chrono::Duration::days(DEFAULT_DAYS - 1)).max(range.since),
        };
        if since > until {
            return Err(WebError(StatusCode::BAD_REQUEST, "日期范围倒挂".into()));
        }
        Ok((since, until))
    }
}

/// `/api/events` 的翻页参数。页码 1-based，与前端 `useFilters` 的 `page` 一致。
#[derive(Deserialize)]
pub(super) struct Paging {
    #[serde(flatten)]
    pub(super) period: Period,
    #[serde(flatten)]
    pub(super) filters: Filters,
    #[serde(default, deserialize_with = "query_num")]
    pub(super) sla_sec: Option<u32>,
    #[serde(default, deserialize_with = "query_num")]
    page: Option<u64>,
    #[serde(default, deserialize_with = "query_num")]
    page_size: Option<u64>,
    /// 排序键，取值见 [`Paging::order_by`] 的白名单。不给就按归属日。
    sort: Option<String>,
    /// `asc` / `desc`。不给按 `asc`。
    dir: Option<String>,
}

impl Paging {
    /// 返回 `(offset, limit)`。**越界一律显式报错，不静默夹到边界上** ——
    /// 夹住的话用户翻到第 300 页会看到第 200 页的内容，还以为那就是全部。
    pub(super) fn window(&self) -> Result<(u64, u64), WebError> {
        let page = self.page.unwrap_or(1);
        let size = self.page_size.unwrap_or(20);
        if page == 0 || page > MAX_PAGE {
            return Err(WebError(
                StatusCode::BAD_REQUEST,
                format!("页码须在 1~{MAX_PAGE} 之间，再深请缩小日期范围或加筛选"),
            ));
        }
        if size == 0 || size > PAGE_SIZE_MAX {
            return Err(WebError(
                StatusCode::BAD_REQUEST,
                format!("每页条数须在 1~{PAGE_SIZE_MAX} 之间"),
            ));
        }
        Ok(((page - 1) * size, size))
    }

    /// 总数 → `(页数, 是否被 [`MAX_PAGE`] 护栏夹过)`。
    ///
    /// **后端算好**：前端不做任何算术，也不需要知道 `MAX_PAGE` 这个常量存在。
    /// 此前它借 `/api/summary` 的事件总数自己 `ceil` 再 `min` —— 而那个数只算
    /// **已知成功群日**上的事件，和明细表翻的集合不是一个，于是窗口里一有抽取失败
    /// 的群日，人就被夹在更早的页码上，尾部的行永远翻不到。
    ///
    /// 页码越过页数不是错误：`LIMIT 偏移, 条数` 自然吐空集合，前端显示空表。
    /// 校验仍然只有 [`Self::window`] 那一处 —— 越过 `MAX_PAGE` 才是 400。
    pub(super) fn pages(&self, total: u64) -> Result<(u64, bool), WebError> {
        let (_, size) = self.window()?;
        let needed = total.div_ceil(size);
        Ok((needed.min(MAX_PAGE), needed > MAX_PAGE))
    }

    /// 排序片段。**只开放 `idx_overview` 里有的列。**
    ///
    /// ⚠️ **不是所有列都该能排。** `summary` / `last_msg_role` / `followup_wait_max_sec`
    /// 都不在 `idx_overview` 里（见 `schema.sql` 那几条注释）—— 按它们排，内层那个
    /// 「只数索引条目、一次都不回表」的分页会退化成**整个窗口逐行回表**，
    /// 而页面上只是多了个能点的表头，慢下来没有任何东西提示。想排它们，
    /// 先把列加进索引。**在这里悄悄放行才是错的做法。**
    ///
    /// ⚠️ **排序键必须唯一** —— 所以恒定以 `e.id` 收尾。不唯一的话同一行可能在两页里
    /// 都出现、或者一页都不在，而翻页的人看不出来。
    ///
    /// ⚠️ **NULL 一律排最后，两个方向都是。** `first_agent_reply_time` 的 NULL 是
    /// 「没回复」、`secs` 的 NULL 跟着它 —— 那不是「很小」，让它冒到升序头部会把
    /// 「最快响应」那一屏变成一堆根本没响应的。
    ///
    /// 拼进 SQL 的只有**白名单里的常量**，用户给的字符串只用来查表。
    pub(super) fn order_by(&self) -> Result<String, WebError> {
        let Some(key) = self
            .sort
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            // 默认与索引前缀同序：`(corpid, occurred_on, ...)`，连 filesort 都不用。
            return Ok("e.occurred_on, e.id".into());
        };
        let expr: &str = match key {
            "time" => "e.first_msg_time",
            "reply" => "e.first_agent_reply_time",
            "wait" => &FIRST_REPLY_SEC,
            "room" => "e.roomid",
            "last" => "e.last_msg_time",
            other => {
                return Err(WebError(
                    StatusCode::BAD_REQUEST,
                    format!("不支持按 {other:?} 排序；可用：time / reply / wait / room / last"),
                ));
            }
        };
        let dir = match self.dir.as_deref() {
            None | Some("asc") => "ASC",
            Some("desc") => "DESC",
            other => {
                return Err(WebError(
                    StatusCode::BAD_REQUEST,
                    format!("排序方向须是 asc / desc，收到 {other:?}"),
                ));
            }
        };
        Ok(format!("({expr}) IS NULL, {expr} {dir}, e.id"))
    }
}

/// 给 `query.rs` 的绑定测试造一个 [`Paging`] —— 那边看不见本文件的私有字段，
/// 而那些字段（页码 · 每页条数 · 排序）正是明细那条语句的后缀绑定来源。
#[cfg(test)]
impl Paging {
    pub(super) fn for_test(
        page: u64,
        page_size: u64,
        sort: Option<&str>,
        filters: Filters,
    ) -> Self {
        Self {
            period: Period::default(),
            filters,
            sla_sec: None,
            page: Some(page),
            page_size: Some(page_size),
            sort: sort.map(str::to_owned),
            dir: sort.map(|_| "desc".to_owned()),
        }
    }
}

/// 动态筛选的一个绑定值。**不拼进 SQL，一律走 `?`。**
pub(super) enum Bind {
    Str(String),
    /// 无符号整数：SLA 秒数 · 直方图桶边界 · `LIMIT` 的偏移量与条数。
    Num(u64),
    Date(NaiveDate),
}

/// 看板上的筛选条件 —— 四个聚合接口和明细接口**收同一组**，
/// 于是「图表和表格用的是同一批事件」这条保证从前端搬到了 SQL 里。
///
/// ⚠️ **`level1`（父类）不在这里。** 父类到子类的映射住在词表，而词表已经在前端手上
/// （`/api/meta` 就带着它）—— 让前端把父类展开成 `types=a,b,c` 传上来，
/// 比在每条 SQL 里再 join 一次词表便宜，也不会多出一处可以和前端打架的口径。
///
/// ⚠️ **`q` 只匹配 `summary`。** 前端那个搜索框还会匹配群名 / 客服名 / 类型名，
/// 那些都是**前端的标签映射**，SQL 里没有 —— 由前端把命中的 id 集合解析出来，
/// 走 `rooms` / `agents` / `types` 参数传上来。`LIKE '%词%'` 前导通配符任何索引都用不上，
/// 靠日期窗口把行数压住。
#[derive(Default, Deserialize)]
pub(super) struct Filters {
    pub(super) room: Option<String>,
    pub(super) agent: Option<String>,
    /// **首响归属**给这个人的事件。与 `agent`（参与过）是两个口径，可以同时给。
    pub(super) responder: Option<String>,
    /// 逗号分隔。空串按「没给」处理 —— 前端展开父类得到空集合时不该筛成零结果。
    pub(super) types: Option<String>,
    /// 逗号分隔的**排除**集合。「未归类」这个一级分类只能这么表达：
    /// 它是「词表里所有 type_id 之外的那些」，正着列不出来（词表外的历史编码没有名单）。
    pub(super) types_exclude: Option<String>,
    /// `unreplied` / `replied` / `push` / `backlog`，与前端 `StatusFilter` 同名。
    pub(super) status: Option<String>,
    #[serde(default, deserialize_with = "query_bool")]
    pub(super) overdue_only: Option<bool>,
    pub(super) q: Option<String>,
}

impl Filters {
    fn list(value: &Option<String>) -> Vec<String> {
        value
            .iter()
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// 追加到事实查询 `WHERE` 后面的片段，以及**按出现顺序**的绑定值。
    ///
    /// ⚠️ **不要自己 bind** —— 走 `scope::Scope::filters`，它把片段和这串绑定
    /// 一起追加进去。手工分开绑的那条路已经没有了：顺序错了 MySQL 不报错，只会算错。
    /// 唯一动态拼进 SQL 的是 `IN (?, ?, …)` 的**占位符个数**，值全走绑定。
    pub(super) fn clause(&self, sla_sec: u32, last_day: NaiveDate) -> (String, Vec<Bind>) {
        let (mut sql, mut binds) = (String::new(), Vec::new());
        if let Some(room) = &self.room {
            sql.push_str(" AND e.roomid = ?");
            binds.push(Bind::Str(room.clone()));
        }
        if let Some(agent) = &self.agent {
            // JSON 列进不了索引，这一条必然回表 —— 已知且有意，窗口把行数压住。
            sql.push_str(" AND JSON_CONTAINS(e.agents, JSON_QUOTE(?))");
            binds.push(Bind::Str(agent.clone()));
        }
        if let Some(responder) = &self.responder {
            // ⚠️ 与 `agent` **不是**同一件事：那个是「参与过」，这个是「首响归属」。
            // 未回复的事件 `first_responder` 是 NULL，自然不会命中任何人 —— 正确。
            sql.push_str(" AND e.first_responder = ?");
            binds.push(Bind::Str(responder.clone()));
        }
        let types = Self::list(&self.types);
        if !types.is_empty() {
            // `?, ?, …` —— **唯一动态拼进 SQL 的东西是占位符个数**，值全走绑定。
            let holes = vec!["?"; types.len()].join(", ");
            sql.push_str(&format!(" AND e.event_type IN ({holes})"));
            binds.extend(types.into_iter().map(Bind::Str));
        }
        let excluded = Self::list(&self.types_exclude);
        if !excluded.is_empty() {
            // `event_type IS NULL`（打标未完成）不算「未归类」—— 那是两回事，
            // 前者是还没算，后者是算了但归不上去。这里显式排掉。
            let holes = vec!["?"; excluded.len()].join(", ");
            sql.push_str(&format!(
                " AND e.event_type IS NOT NULL AND e.event_type NOT IN ({holes})"
            ));
            binds.extend(excluded.into_iter().map(Bind::Str));
        }
        // 状态是互斥的四选一，与前端 `matches()` 里那四个分支逐条对应。
        match self.status.as_deref() {
            Some("unreplied") => {
                sql.push_str(" AND e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NULL")
            }
            Some("replied") => sql.push_str(
                " AND e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NOT NULL",
            ),
            Some("push") => sql.push_str(" AND e.asker_role <> 'EXTERNAL'"),
            Some("backlog") => {
                sql.push_str(
                    " AND e.asker_role = 'EXTERNAL' AND e.first_agent_reply_time IS NULL \
                     AND e.occurred_on < ?",
                );
                binds.push(Bind::Date(last_day));
            }
            _ => {}
        }
        if let Some(only) = self.overdue_only {
            // 前端 `isOverdue`：**未回复的也算超时**。取反时这两半都要反过来。
            let overdue = format!(
                "(e.asker_role = 'EXTERNAL' AND (e.first_agent_reply_time IS NULL \
                 OR {sec} > ?))",
                sec = *FIRST_REPLY_SEC
            );
            sql.push_str(&format!(" AND {}{overdue}", if only { "" } else { "NOT " }));
            binds.push(Bind::Num(sla_sec.into()));
        }
        if let Some(q) = self.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            sql.push_str(" AND e.summary LIKE ?");
            // `%` / `_` 是 LIKE 的元字符，用户搜它们时应当按字面匹配。
            let escaped = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            binds.push(Bind::Str(format!("%{escaped}%")));
        }
        (sql, binds)
    }
}

/// 超时线由调用方给 —— 它是**看板上的一个滑块**，不是配置项，所以进 query 参数。
#[derive(serde::Deserialize)]
pub(super) struct Sla {
    #[serde(flatten)]
    pub(super) period: Period,
    #[serde(flatten)]
    pub(super) filters: Filters,
    /// ⚠️ **必须走 `query_num`**：`Grouping` 又把本结构 flatten 一层，
    /// 两层之后 serde 不再把 `"300"` 转成 `u32`，`/api/rooms` 与 `/api/categories`
    /// 会对每个带 `sla_sec` 的请求返回 400。理由全文见 `query_num`。
    #[serde(default, deserialize_with = "query_num")]
    sla_sec: Option<u32>,
    /// 首响直方图的桶边界，逗号分隔的秒数。只有 `/api/summary` 读它。
    buckets: Option<String>,
}

impl Sla {
    /// 默认值必须等于前端的 `DEFAULT_SLA_SEC`（`domain/definitions.ts`，当前 1800）——
    /// 两边不一致时，不传 `sla_sec` 的请求会算出和页面不同的超时数。
    /// 上限只为挡住溢出，不是业务约束。
    pub(super) fn sla_sec(&self) -> u32 {
        self.sla_sec.unwrap_or(DEFAULT_SLA_SEC).min(86_400 * 30)
    }

    /// 桶边界。**没给就不出直方图**（返回空数组），不自带一份默认分桶 ——
    /// 自带的那份会在前端改了 `RESPONSE_BINS` 之后继续沉默地按老边界分。
    ///
    /// 非法值一律 400，不跳过：`buckets=60,abc,900` 被吞成两个桶的话，
    /// 页面会画出一张少一根柱子的图，而没有任何东西提示它错了。
    pub(super) fn buckets(&self) -> Result<Vec<u32>, WebError> {
        let Some(raw) = self
            .buckets
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            return Ok(Vec::new());
        };
        let edges = raw
            .split(',')
            .map(|v| {
                v.trim().parse::<u32>().map_err(|_| {
                    WebError(
                        StatusCode::BAD_REQUEST,
                        format!("桶边界须是非负整数秒，收到 {v:?}"),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // 上限只是挡住「拼出一条几千个 WHEN 的 CASE」，不是业务约束。
        if edges.len() > 32 {
            return Err(WebError(StatusCode::BAD_REQUEST, "桶边界最多 32 个".into()));
        }
        if edges.windows(2).any(|w| w[0] >= w[1]) {
            return Err(WebError(
                StatusCode::BAD_REQUEST,
                "桶边界必须严格升序".into(),
            ));
        }
        Ok(edges)
    }
}

/// 分类汇总的参数。比 [`Sla`] 多一个 `groups` —— **前端把词表的父类分组传上来**，
/// 后端不认识词表（理由见 [`read_categories`]）。
#[derive(serde::Deserialize)]
pub(super) struct Grouping {
    #[serde(flatten)]
    pub(super) sla: Sla,
    /// `t1|t2,t3|t4` —— 逗号分隔组，竖线分隔组内 `type_id`。不给就按 `event_type` 分。
    groups: Option<String>,
}

impl Grouping {
    /// 空组直接拒绝：`groups=t1,,t3` 里那个空组会变成一个恒不命中的分支，
    /// 前端却按下标去映射父类名 —— 名字会错位到另一个父类上。
    pub(super) fn groups(&self) -> Result<Vec<Vec<String>>, WebError> {
        let Some(raw) = self
            .groups
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            return Ok(Vec::new());
        };
        let groups = raw
            .split(',')
            .map(|group| {
                group
                    .split('|')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        if groups.iter().any(Vec::is_empty) {
            return Err(WebError(
                StatusCode::BAD_REQUEST,
                "分组不能为空，下标会与前端的父类名错位".into(),
            ));
        }
        if groups.iter().map(Vec::len).sum::<usize>() > 1000 {
            return Err(WebError(
                StatusCode::BAD_REQUEST,
                "分组里的类型总数最多 1000 个".into(),
            ));
        }
        Ok(groups)
    }
}

/// query string 里的布尔。**serde 不认 `"true"` 这个字符串** —— URL 参数没有类型，
/// 全是字符串，直接用 `Option<bool>` 会报「invalid type: string」。
///
/// 非法取值**显式报错**（→ 400），不静默当 false：`overdue_only=yes` 被吞掉的话，
/// 用户以为在看超时事件，实际看到的是全部。
/// 查询串里的数字。**不能直接写 `Option<u32>`。**
///
/// `Query` 拿到的值一律是字符串。只嵌一层 `#[serde(flatten)]` 时 serde 还肯把
/// 字符串转成整数，**嵌两层就不肯了**（`Grouping` → `Sla` → `sla_sec`）：
/// 请求会被拒成 `invalid type: string "300", expected u32`。
///
/// ⚠️ **这不是假设。** `/api/rooms` 与 `/api/categories` 曾经对**每一个**带
/// `sla_sec` 的请求返回 400，而前端每次都带（`client.ts` 的 `params()`）——
/// 概览的群表和分类汇总在真接口下整片报错。旁边那个 `query_bool` 是同一个坑的
/// 前一半：布尔同样活不过 flatten。
///
/// 所以查询串里的数字**一律走这里**，不依赖「今天嵌了几层」这种会变的事。
pub(super) fn query_num<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    Option::<String>::deserialize(d)?
        .map(|value| value.trim().parse::<T>().map_err(serde::de::Error::custom))
        .transpose()
}

fn query_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|value| match value.as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "overdue_only 须是 true / false，收到 {other:?}"
            ))),
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;

    /// `bounds` 现在**只吃日期两端**，不再要一个完整的 `Meta` ——
    /// 这个助手从 9 行缩到 4 行，正是那次收窄在测试面上的样子。
    fn range(since: &str, until: &str) -> DateRange {
        DateRange {
            since: since.parse().unwrap(),
            until: until.parse().unwrap(),
        }
    }

    fn paging(page: Option<u64>, size: Option<u64>) -> Paging {
        Paging {
            period: Period::default(),
            filters: Filters::default(),
            sla_sec: None,
            page,
            page_size: size,
            sort: None,
            dir: None,
        }
    }

    fn sorted(sort: Option<&str>, dir: Option<&str>) -> Paging {
        Paging {
            sort: sort.map(str::to_owned),
            dir: dir.map(str::to_owned),
            ..paging(None, None)
        }
    }

    const DAY: NaiveDate = match NaiveDate::from_ymd_opt(2026, 8, 26) {
        Some(d) => d,
        None => unreachable!(),
    };

    /// **每一条筛选查询的正确性都压在这条性质上**：`clause` 把 SQL 片段和绑定值
    /// 拆成两半交出去，调用方按顺序接回去。个数一旦对不上，MySQL 报的是
    /// 「参数个数不对」或者干脆算错 —— 而算错是静默的。
    ///
    /// 这里不需要数据库：数 `?`，数 `Vec<Bind>`，两者必须相等。
    #[test]
    fn every_filter_binds_exactly_as_many_values_as_it_adds_placeholders() {
        let one = |f: Filters| {
            let (sql, binds) = f.clause(1800, DAY);
            assert_eq!(
                sql.matches('?').count(),
                binds.len(),
                "占位符与绑定值个数不一致：{sql}"
            );
            (sql, binds)
        };

        // 空筛选：什么都不加
        let (sql, binds) = one(Filters::default());
        assert!(sql.is_empty() && binds.is_empty());

        // 逐个单独来一遍
        for f in [
            Filters {
                room: Some("R1".into()),
                ..Default::default()
            },
            Filters {
                agent: Some("A1".into()),
                ..Default::default()
            },
            Filters {
                responder: Some("A1".into()),
                ..Default::default()
            },
            Filters {
                types: Some("a,b,c".into()),
                ..Default::default()
            },
            Filters {
                types_exclude: Some("x,y".into()),
                ..Default::default()
            },
            Filters {
                status: Some("backlog".into()),
                ..Default::default()
            },
            Filters {
                overdue_only: Some(true),
                ..Default::default()
            },
            Filters {
                overdue_only: Some(false),
                ..Default::default()
            },
            Filters {
                q: Some("加单".into()),
                ..Default::default()
            },
        ] {
            one(f);
        }

        // 全都给上 —— 占位符最多的那一条路
        let (sql, binds) = one(Filters {
            room: Some("R1".into()),
            agent: Some("A1".into()),
            responder: Some("A2".into()),
            types: Some("a,b,c".into()),
            types_exclude: Some("x,y".into()),
            status: Some("backlog".into()),
            overdue_only: Some(true),
            q: Some("加单".into()),
        });
        // room 1 + agent 1 + responder 1 + types 3 + exclude 2 + backlog 1 + sla 1 + q 1
        assert_eq!(binds.len(), 11, "{sql}");
    }

    /// 空串按「没给」处理 —— 前端把父类展开成空集合时不该被筛成零结果。
    /// 而 `status` 认不出的取值静默忽略是**有意**的（四选一，别的值不加条件）。
    #[test]
    fn empty_and_blank_filter_values_add_nothing() {
        for f in [
            Filters {
                types: Some(String::new()),
                ..Default::default()
            },
            Filters {
                types: Some(" , , ".into()),
                ..Default::default()
            },
            Filters {
                types_exclude: Some(",".into()),
                ..Default::default()
            },
            Filters {
                q: Some("   ".into()),
                ..Default::default()
            },
        ] {
            let (sql, binds) = f.clause(1800, DAY);
            assert!(sql.is_empty() && binds.is_empty(), "不该加条件：{sql}");
        }
    }

    /// `%` / `_` 是 LIKE 的元字符 —— 用户搜它们时必须按字面匹配，
    /// 否则搜一个 `%` 会命中全部行。
    #[test]
    fn search_escapes_like_metacharacters() {
        let (_, binds) = Filters {
            q: Some(r"100%_a\b".into()),
            ..Default::default()
        }
        .clause(1800, DAY);
        let Some(Bind::Str(pattern)) = binds.first() else {
            panic!("q 应当绑一个字符串");
        };
        assert_eq!(pattern, r"%100\%\_a\\b%");
    }

    /// 越界一律显式 400，**不静默夹到边界上** —— 夹住的话用户翻到第 300 页
    /// 会看到第 200 页的内容，还以为那就是全部。
    #[test]
    fn paging_rejects_out_of_range_instead_of_clamping() {
        assert_eq!(paging(None, None).window().unwrap(), (0, 20));
        assert_eq!(paging(Some(3), Some(10)).window().unwrap(), (20, 10));
        assert_eq!(
            paging(Some(MAX_PAGE), Some(PAGE_SIZE_MAX))
                .window()
                .unwrap(),
            ((MAX_PAGE - 1) * PAGE_SIZE_MAX, PAGE_SIZE_MAX)
        );
        for bad in [
            paging(Some(0), None),
            paging(Some(MAX_PAGE + 1), None),
            paging(None, Some(0)),
            paging(None, Some(PAGE_SIZE_MAX + 1)),
        ] {
            assert_eq!(
                bad.window().unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "越界必须报错，不能夹"
            );
        }
    }

    /// 排序白名单同时是**两道闸**：SQL 注入的（拼进语句的只有白名单里的常量），
    /// 以及索引安全的（放行 `idx_overview` 之外的列，会让延迟关联退化成整窗口回表，
    /// 而页面上只是多了个能点的表头）。放行任何一个新键之前，先把列加进索引。
    #[test]
    fn sort_whitelist_is_closed_and_every_key_stays_unique() {
        // 默认与索引前缀同序，连 filesort 都不用
        assert_eq!(
            sorted(None, None).order_by().unwrap(),
            "e.occurred_on, e.id"
        );

        for key in ["time", "reply", "wait", "room", "last"] {
            let asc = sorted(Some(key), None).order_by().unwrap();
            let desc = sorted(Some(key), Some("desc")).order_by().unwrap();
            // 排序键必须唯一 —— 不唯一的话同一行可能在两页里都出现、或者一页都不在
            assert!(asc.ends_with(", e.id"), "{key} 没以 e.id 收尾：{asc}");
            assert!(desc.ends_with(", e.id"), "{key} 没以 e.id 收尾：{desc}");
            // NULL 一律排最后，两个方向都是 —— 「没回复」不是「很快回复」
            assert!(asc.contains(") IS NULL,"), "{key} 少了 NULL 靠后：{asc}");
            assert!(desc.contains(") IS NULL,"), "{key} 少了 NULL 靠后：{desc}");
            assert!(
                asc.contains(" ASC,") && desc.contains(" DESC,"),
                "{asc} / {desc}"
            );
        }

        // 按等待时长排序必须用**工作时段**那份表达式，跟筛选、分位数同一个口径 ——
        // 换成墙钟差的话，同一份数据在排序和数字上会给出两种顺序。
        assert!(
            sorted(Some("wait"), None)
                .order_by()
                .unwrap()
                .contains(&*FIRST_REPLY_SEC)
        );

        // 不在 idx_overview 里的列必须被拒 —— 在这里悄悄放行才是错的做法
        for bad in [
            "summary",
            "followup_wait_max_sec",
            "last_msg_role",
            "id",
            "'",
        ] {
            assert_eq!(
                sorted(Some(bad), None).order_by().unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "{bad} 不该被放行"
            );
        }
        assert_eq!(
            sorted(Some("time"), Some("sideways"))
                .order_by()
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        // 空的 sort 按「没给」处理，不是错误
        assert_eq!(
            sorted(Some("  "), None).order_by().unwrap(),
            "e.occurred_on, e.id"
        );
    }

    #[test]
    fn default_window_is_the_last_seven_days_and_reversed_ranges_are_rejected() {
        let m = range("2026-08-01", "2026-08-26");

        // 都不给：最后 DEFAULT_DAYS 天，含右端
        let (since, until) = Period::default().bounds(&m).unwrap();
        assert_eq!(
            (since.to_string(), until.to_string()),
            ("2026-08-20".to_owned(), "2026-08-26".to_owned())
        );
        assert_eq!((until - since).num_days() + 1, DEFAULT_DAYS);

        // 只给 to：从它往回数
        let p = Period {
            from: None,
            to: Some("2026-08-10".into()),
        };
        assert_eq!(p.bounds(&m).unwrap().0.to_string(), "2026-08-04");

        // 默认窗口不会越过库里最早的一天
        let narrow = range("2026-08-25", "2026-08-26");
        assert_eq!(
            Period::default().bounds(&narrow).unwrap().0.to_string(),
            "2026-08-25"
        );

        for bad in [
            ("2026-08-26", "2026-08-25"), // 倒挂
            ("2026-8-1", "2026-08-26"),   // 长度不对
            ("not-a-date", "2026-08-26"),
            ("2026-02-30", "2026-08-26"), // 有格式没这天
        ] {
            let p = Period {
                from: Some(bad.0.into()),
                to: Some(bad.1.into()),
            };
            assert_eq!(
                p.bounds(&m).unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "{bad:?} 应当被拒"
            );
        }
    }

    /// `overdue_only=false` 是「**排除**超时」，不是「不筛」—— 取反时未回复那一半
    /// 也要跟着反过来，否则未回复的事件会同时落进「超时」和「没超时」两边。
    #[test]
    fn overdue_filter_negates_the_unreplied_half_too() {
        let (yes, _) = Filters {
            overdue_only: Some(true),
            ..Default::default()
        }
        .clause(1800, DAY);
        let (no, _) = Filters {
            overdue_only: Some(false),
            ..Default::default()
        }
        .clause(1800, DAY);
        assert!(yes.contains("first_agent_reply_time IS NULL"));
        assert_eq!(no, yes.replacen(" AND ", " AND NOT ", 1));
    }

    fn sla(sla_sec: Option<u32>, buckets: Option<&str>) -> Sla {
        Sla {
            period: Period::default(),
            filters: Filters::default(),
            sla_sec,
            buckets: buckets.map(str::to_owned),
        }
    }

    fn grouping(groups: Option<&str>) -> Grouping {
        Grouping {
            sla: sla(None, None),
            groups: groups.map(str::to_owned),
        }
    }

    /// 查询串里的数字必须活过**两层** `#[serde(flatten)]`。
    ///
    /// ⚠️ 这条钉的是一个真实故障，不是理论：`/api/rooms` 与 `/api/categories` 收
    /// `Grouping`，它 flatten 了 `Sla`，`Sla` 又 flatten 了 `Period` 和 `Filters`。
    /// 两层之后 serde 不再把 `"300"` 转成 `u32`，于是**每一个**带 `sla_sec` 的请求
    /// 都被拒成 `400 invalid type: string "300", expected u32` —— 而前端每次都带
    /// （`client.ts` 的 `params()`），概览的群表与分类汇总在真接口下整片报错。
    ///
    /// 走的是**真的 `Query` 提取器**，不是手搓结构体 —— 手搓绕过的正是出错那一步。
    #[test]
    fn numeric_query_params_survive_two_levels_of_flatten() {
        fn parse<T: serde::de::DeserializeOwned>(q: &str) -> Result<T, String> {
            let uri: axum::http::Uri = format!("http://x/api?{q}").parse().unwrap();
            Query::<T>::try_from_uri(&uri)
                .map(|Query(v)| v)
                .map_err(|e| e.to_string())
        }

        // 一层 flatten：/api/summary · /api/agents
        let s: Sla = parse("from=2026-08-25&sla_sec=300&buckets=60,300").unwrap();
        assert_eq!(s.sla_sec(), 300);
        assert_eq!(s.buckets().unwrap(), vec![60, 300]);

        // 两层 flatten：/api/rooms · /api/categories —— 曾经在这里 400
        let g: Grouping = parse("room=R1&sla_sec=300&groups=a|b,c").unwrap();
        assert_eq!(g.sla.sla_sec(), 300);
        assert_eq!(
            g.groups().unwrap(),
            vec![vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()]]
        );

        // 明细翻页的三个数字同理（`Paging` 也带 flatten）
        let p: Paging = parse("page=3&page_size=10&sla_sec=60").unwrap();
        assert_eq!(p.window().unwrap(), (20, 10));

        // 不是数字仍然要拒 —— 不能静默当成「没给」，那会悄悄退回默认 sla
        for bad in ["sla_sec=abc", "sla_sec=", "sla_sec=-1"] {
            assert!(parse::<Grouping>(bad).is_err(), "{bad} 应当被拒");
        }
        assert!(parse::<Paging>("page=1.5").is_err());

        // 没给就是没给，走默认
        assert_eq!(parse::<Sla>("room=R1").unwrap().sla_sec(), 1800);
    }

    /// ⚠️ 默认值必须等于前端的 `DEFAULT_SLA_SEC`（`webui/src/domain/definitions.ts`）——
    /// 两边不一致时，不传 `sla_sec` 的请求会算出和页面不同的超时数，而两个数
    /// 都「看起来合理」。上限只为挡住溢出，不是业务约束。
    #[test]
    fn default_sla_matches_the_frontend_and_is_capped() {
        assert_eq!(sla(None, None).sla_sec(), 1800);
        assert_eq!(sla(Some(300), None).sla_sec(), 300);
        assert_eq!(sla(Some(u32::MAX), None).sla_sec(), 86_400 * 30);

        let ts = include_str!("../../webui/src/domain/definitions.ts");
        assert!(
            ts.contains("DEFAULT_SLA_SEC = 1800"),
            "前端的 DEFAULT_SLA_SEC 变了，后端这个默认值要跟着改"
        );
    }

    /// 桶边界由前端给，后端**不自带一份** —— 自带的那份会在前端改了分桶之后
    /// 继续沉默地按老边界分。非法值一律 400、不跳过：`60,abc,900` 被吞成两个桶的话，
    /// 页面会画出一张少一根柱子的图，而没有任何东西提示它错了。
    #[test]
    fn histogram_edges_must_be_strictly_ascending_or_rejected() {
        assert_eq!(sla(None, None).buckets().unwrap(), Vec::<u32>::new());
        assert_eq!(sla(None, Some("  ")).buckets().unwrap(), Vec::<u32>::new());
        assert_eq!(
            sla(None, Some("60, 300 ,900")).buckets().unwrap(),
            vec![60, 300, 900]
        );

        let many = (1..=32)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(sla(None, Some(&many)).buckets().unwrap().len(), 32);
        let too_many = (1..=33)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");

        for bad in ["60,abc", "300,60", "60,60", "-1", "60,,900", &too_many] {
            assert_eq!(
                sla(None, Some(bad)).buckets().unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "{bad:?} 应当被拒"
            );
        }
    }

    /// 空组会变成一个恒不命中的分支，而前端按**下标**去映射父类名 ——
    /// 名字会错位到另一个父类上，页面照常渲染。
    #[test]
    fn parent_groups_reject_empty_groups_that_would_shift_the_labels() {
        assert_eq!(grouping(None).groups().unwrap(), Vec::<Vec<String>>::new());
        assert_eq!(
            grouping(Some("  ")).groups().unwrap(),
            Vec::<Vec<String>>::new()
        );
        assert_eq!(
            grouping(Some("a|b,c")).groups().unwrap(),
            vec![vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()]]
        );

        let huge = (0..1001)
            .map(|n| format!("t{n}"))
            .collect::<Vec<_>>()
            .join("|");
        for bad in ["t1,,t3", "a|b,", "|", &huge] {
            assert_eq!(
                grouping(Some(bad)).groups().unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "{bad:?} 应当被拒"
            );
        }
    }
}
