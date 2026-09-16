//! HTTP 入口 —— 应用状态、路由与四个 handler。
//!
//! 全部 `GET`、无登录版（内网可达即可看）。请求形状在 `params`，限额与错误映射在
//! `budget`，取数与 SQL 在 `query`，应用状态在 `state`；这里只负责把它们接起来。
//!
//! **本文件一条 SQL 都没有。** 曾经有四段：混版守卫的 `EXISTS`、群日记录的
//! `JSON_OBJECT`、明细分页的手写 bind 循环、单事件和原文的两次主键查找 ——
//! 为此 `query.rs` 得把 `EVENT_SELECT`（一条裸 SQL 常量）和 `Bind` 导出来给 handler
//! 拼，于是它顶上那句「只读 SQL 全在这里」是假的。现在每个 handler 都是同一个形状：
//! acquire → snapshot → 参数定界 → `read_*` → commit → `bounded_json`。

use super::{
    budget::{ResponseBudget, WebError, admit, bounded_json, too_large},
    cache::{Cache, cached},
    config::WebLimits,
    params::{Grouping, Paging, Period, Sla},
    query::{
        available_range, count_events, has_mixed_version, read_agents, read_categories, read_event,
        read_event_page, read_filters, read_group_days, read_meta, read_rooms,
        read_source_messages, read_summary, snapshot,
    },
    roster::Roster,
    state::WebState,
};
use crate::{stage::classify::CURRENT_VERSION, stage::store};
use axum::{
    Router,
    extract::{Path as Id, Query, State},
    http::{StatusCode, header},
    middleware,
    response::Response,
    routing::get,
};
use sqlx::MySqlPool;
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Semaphore;

pub async fn serve(
    pool: MySqlPool,
    corp: String,
    address: SocketAddr,
    limits: WebLimits,
    roster: Arc<Roster>,
) -> crate::Result<()> {
    assert!(!corp.is_empty(), "corpid 不可为空");
    store::check_schema(&pool).await?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(address = %listener.local_addr()?, corp, "只读工作台启动");
    axum::serve(
        listener,
        router(WebState {
            pool,
            corp,
            requests: Arc::new(Semaphore::new(limits.concurrency)),
            cache: Arc::new(Cache::new(limits.cache_bytes)),
            limits,
            roster,
        }),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

pub(super) fn router(state: WebState) -> Router {
    Router::new()
        .route("/api/meta", get(meta))
        .route("/api/dataset", get(dataset))
        .route("/api/summary", get(summary))
        .route("/api/rooms", get(rooms))
        .route("/api/agents", get(agents))
        .route("/api/categories", get(categories))
        .route("/api/events", get(events))
        .route("/api/event/{id}", get(event))
        .route("/api/event/{id}/messages", get(messages))
        // 缓存在准入之内：命中也占一个名额（只有一条戳查询，几毫秒），
        // 于是名额仍然是 MySQL 连接峰值的上界，`concurrency` 的算法不变。
        .layer(middleware::from_fn_with_state(state.clone(), cached))
        .layer(middleware::from_fn_with_state(state.clone(), admit))
        .layer(middleware::map_response(
            |mut response: Response| async move {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                response
            },
        ))
        .with_state(state)
}

/// 筛选器选项 ＋ **外部名册回填 —— 全仓唯一的注入点**。
///
/// `read_filters` 只产出「待解析的 ID」（`easyUserId, officialUserId?`），姓名那一跳在
/// 这里补：只读 SQL 模块因此保持**零 HTTP**，而前端所有位置的人名都来自筛选器元数据
/// 构建的那两张查找表 —— 按群聚合 / 按客服聚合 / 事件明细 / 事件抽屉一个字都不用改。
///
/// ⚠️ **补齐必须在响应生成之前完成。** 先出一份「显示 ID」的半成品，它会被响应缓存
/// 钉住直到名册 TTL 到期才翻身 —— 那不会自愈。
///
/// ⚠️ 补齐时**手上还握着那条只读连接**（`dataset` 后面还要用同一个一致快照）。
/// 代价是有界的：名册命中时零往返；未命中时一把锁串行化，后到的请求醒来已经不缺了，
/// 所以最坏是**一次**往返的等待，不是每人一次。
async fn filters(
    state: &WebState,
    tx: &mut sqlx::MySqlConnection,
    since: chrono::NaiveDate,
    until: chrono::NaiveDate,
) -> Result<(Vec<serde_json::Value>, Vec<serde_json::Value>), WebError> {
    let (rooms, agents) = read_filters(tx, &state.corp, since, until, &state.limits).await?;
    let wanted = agents
        .iter()
        .flat_map(|(_, account)| account.clone())
        .collect();
    let names = state.roster.employees(&wanted).await;
    Ok((
        rooms,
        agents
            .into_iter()
            .map(|(agent, account)| agent_option(agent, account, &names))
            .collect(),
    ))
}

/// 一个客服选项 —— **三跳回落**：姓名 →（查不到）账号 →（也没有）16 位 `easyUserId`。
///
/// 最后那一跳由前端接：`alias` 为 `null` 时它显示 `agent` 本身。
///
/// `alias_is_authoritative` 沿用群那一支的形状（per-项优先、回落全局）：
/// **只有真姓名才算权威**。回落到账号的这一位必须是 `false` —— 否则页面等于宣称
/// `zhang.san` 是个姓名，而那正是接名册之前就挂着免责说明的原因。
pub(super) fn agent_option(
    agent: String,
    account: Option<String>,
    names: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let name = account.as_deref().and_then(|account| names.get(account));
    serde_json::json!({
        "agent": agent,
        "alias": name.or(account.as_ref()),
        "alias_is_authoritative": name.is_some(),
    })
}

async fn meta(State(state): State<WebState>) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    let meta = read_meta(&mut tx, &state.corp, &state.limits).await?;
    // 这个接口没有窗口参数，用**默认窗口**填筛选器 —— 页面随后拉 `/api/dataset`
    // 时会拿到按实际窗口算的那一份，两者形状相同。
    // 关键是它**不再扫全历史**：此前群与客服名单是无日期条件的全表扫描。
    let (since, until) = Period::default().bounds(&meta.range)?;
    let (rooms, agents) = filters(&state, &mut tx, since, until).await?;
    tx.commit().await?;
    bounded_json(&meta.with_filters(rooms, agents), &state.limits)
}

/// 页面的**上下文**：meta ＋ 群日记录。**不含事件明细。**
///
/// ⚠️ **事件明细已经从这里拿掉了。** 它是唯一一个「行数 = 群数 × 天数 × 每群每天事件数」
/// 的集合（1000 群 7 天约 11 万行，撞 `max_rows` 直接 413），而页面要它只是为了在浏览器里
/// 现算指标 —— 那些指标现在由 `/api/summary`、`/api/rooms`、`/api/agents`、
/// `/api/categories` 在数据库里算完只送数字，明细则由 `/api/events` 一页一页翻。
///
/// 留下的群日记录是 `群数 × 天数`（1000 群 7 天 = 7000 行），**本来就不会爆**，
/// 而且它是唯一能回答「这个格子是真的 0 还是抽取失败」的东西 —— 前端的覆盖度、
/// 消息量、热力图缺口全靠它，聚合接口替代不了。
async fn dataset(
    State(state): State<WebState>,
    Query(period): Query<Period>,
) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    let meta = read_meta(&mut tx, &state.corp, &state.limits).await?;
    let (since, until) = period.bounds(&meta.range)?;
    // 筛选器选项跟着窗口走（见 `read_filters`），不再扫全历史。
    let (rooms, agents) = filters(&state, &mut tx, since, until).await?;
    let meta = meta.with_filters(rooms, agents);
    // 混版守卫 —— **一条 `EXISTS` 就够，不用把事件拉下来比对**。
    // 窗口里只要有一条已发布标签的事件用的不是当前词表，整页就不能渲染：
    // 两个版本的分类混在一张表上，每个数字都看起来正常，但没有一个能对。
    let mixed =
        has_mixed_version(&mut tx, &state.corp, since, until, meta.taxonomy_version).await?;
    if mixed {
        return Err(WebError(
            StatusCode::CONFLICT,
            "事件与当前词表版本不一致，请完成重打标".into(),
        ));
    }
    // 群日记录是**数据库已经渲染好的 JSON**，直推字节缓冲不解析；`meta` 是 Rust 侧
    // 构造的，走序列化。两者共用同一份预算与同一个缓冲（见 `budget` 的模块文档）。
    let mut budget = ResponseBudget::new(&state.limits);
    budget.frame("{\"meta\":")?;
    budget.value(&meta)?;
    budget.frame(",\"groupDaily\":[")?;
    read_group_days(&mut tx, &state.corp, since, until, &mut budget).await?;
    budget.frame("]}")?;
    tx.commit().await?;
    Ok(budget.finish())
}

/// 概览 KPI —— **数据库算完只送数字**，回几十行，且不随窗口变大。
///
/// 这是 `/api/dataset` 那条路的替代：那边要把窗口内**每一条**事件拉齐才能算指标
/// （1000 群 7 天是 11 万行，撞 `max_rows` 直接 413），这边只回 1 行 KPI。
///
/// ⚠️ **新老并存期必须逐个数字对拍** —— 同一批数据、同一个 `sla`，
/// 这里的每个字段都要和前端 `aggregate()` 算出来的一模一样。对不上就是口径搬错了，
/// 而那是静默的：页面照样显示一个看起来合理的数字。
async fn summary(
    State(state): State<WebState>,
    Query(sla): Query<Sla>,
) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    // ⚠️ **只取日期两端，不造 `Meta`** —— 这个接口用不到词表。
    // 走 `read_meta` 的话每个请求都要 `read_taxonomy` 全表读一遍再 `check_types`，
    // 而结果原样丢掉；页面点一下筛选就是四个聚合接口，于是四遍。见 `available_range`。
    let range = available_range(&mut tx, &state.corp, &state.limits).await?;
    let (since, until) = sla.period.bounds(&range)?;
    let value = read_summary(
        &mut tx,
        &state.corp,
        since,
        until,
        sla.sla_sec(),
        &sla.filters,
        &sla.buckets()?,
    )
    .await?;
    tx.commit().await?;
    bounded_json(&value, &state.limits)
}

/// 按群 / 按客服的聚合行。**行数封顶在「群数」「客服数」上，与窗口多宽无关。**
///
/// 两个 handler 形状一样，只差调用哪个取数函数 —— 但**没有合并成一个泛型**：
/// 两条 SQL 的分组键、口径和绑定顺序都不同，共用一层壳只会把差异藏起来。
async fn rooms(
    State(state): State<WebState>,
    Query(grouping): Query<Grouping>,
) -> Result<Response, WebError> {
    // `groups` 在这里只决定「主要事件类型」那排标签按什么分，和 `/api/categories` 同一套。
    let groups = grouping.groups()?;
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    // ⚠️ **只取日期两端，不造 `Meta`** —— 这个接口用不到词表。
    // 走 `read_meta` 的话每个请求都要 `read_taxonomy` 全表读一遍再 `check_types`，
    // 而结果原样丢掉；页面点一下筛选就是四个聚合接口，于是四遍。见 `available_range`。
    let range = available_range(&mut tx, &state.corp, &state.limits).await?;
    let (since, until) = grouping.sla.period.bounds(&range)?;
    let rows = read_rooms(
        &mut tx,
        &state.corp,
        since,
        until,
        grouping.sla.sla_sec(),
        &grouping.sla.filters,
        &groups,
    )
    .await?;
    tx.commit().await?;
    if rows.len() > state.limits.max_rows {
        return Err(too_large());
    }
    bounded_json(&rows, &state.limits)
}

async fn agents(
    State(state): State<WebState>,
    Query(sla): Query<Sla>,
) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    // ⚠️ **只取日期两端，不造 `Meta`** —— 这个接口用不到词表。
    // 走 `read_meta` 的话每个请求都要 `read_taxonomy` 全表读一遍再 `check_types`，
    // 而结果原样丢掉；页面点一下筛选就是四个聚合接口，于是四遍。见 `available_range`。
    let range = available_range(&mut tx, &state.corp, &state.limits).await?;
    let (since, until) = sla.period.bounds(&range)?;
    let rows = read_agents(
        &mut tx,
        &state.corp,
        since,
        until,
        sla.sla_sec(),
        &sla.filters,
    )
    .await?;
    tx.commit().await?;
    if rows.len() > state.limits.max_rows {
        return Err(too_large());
    }
    bounded_json(&rows, &state.limits)
}

/// 分类汇总 —— 行数是**类型数**（二级）或**父类数**（一级），几十行。
///
/// 一级和二级走同一个 handler：差别只是前端给不给 `groups`。
/// **一级的分位数必须在这里按父类现算**，不能由二级的合并 —— 分位数不可加。
async fn categories(
    State(state): State<WebState>,
    Query(grouping): Query<Grouping>,
) -> Result<Response, WebError> {
    let groups = grouping.groups()?;
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    // ⚠️ **只取日期两端，不造 `Meta`** —— 这个接口用不到词表。
    // 走 `read_meta` 的话每个请求都要 `read_taxonomy` 全表读一遍再 `check_types`，
    // 而结果原样丢掉；页面点一下筛选就是四个聚合接口，于是四遍。见 `available_range`。
    let range = available_range(&mut tx, &state.corp, &state.limits).await?;
    let (since, until) = grouping.sla.period.bounds(&range)?;
    let rows = read_categories(
        &mut tx,
        &state.corp,
        since,
        until,
        grouping.sla.sla_sec(),
        &grouping.sla.filters,
        &groups,
    )
    .await?;
    tx.commit().await?;
    if rows.len() > state.limits.max_rows {
        return Err(too_large());
    }
    bounded_json(&rows, &state.limits)
}

/// 事件明细的一页 —— **不参与任何指标**，纯粹是给人翻的表。
///
/// 走 [`event_page_select`] 的延迟关联：内层只在覆盖索引里数够偏移量、不回表，
/// 外层才拿那几个 id 回表。页码上限由 [`Paging::window`] 拦，越界是 400 不是截断。
///
/// ⚠️ **和 `/api/dataset` 里的 `events` 是两条路**：那一条要拉齐整个窗口来算指标
/// （所以有 `max_rows` 那个天花板），这一条只取一页。群数上去之后，
/// 明细表应该改读这个接口，把 `dataset` 留给聚合。
async fn events(
    State(state): State<WebState>,
    Query(paging): Query<Paging>,
) -> Result<Response, WebError> {
    let mut connection = state.pool.acquire().await?;
    let mut tx = snapshot(&mut connection).await?;
    // ⚠️ **只取日期两端，不造 `Meta`** —— 这个接口用不到词表。
    // 走 `read_meta` 的话每个请求都要 `read_taxonomy` 全表读一遍再 `check_types`，
    // 而结果原样丢掉；页面点一下筛选就是四个聚合接口，于是四遍。见 `available_range`。
    let range = available_range(&mut tx, &state.corp, &state.limits).await?;
    let (since, until) = paging.period.bounds(&range)?;
    // 行、总数、页数、截断标志**全在这一个响应里** —— 前端不再借 `/api/summary`
    // 的事件总数推算分页（那是另一个集合，见 `count_events`）。
    let mut budget = ResponseBudget::new(&state.limits);
    budget.frame("{\"rows\":[")?;
    read_event_page(&mut tx, &state.corp, since, until, &paging, &mut budget).await?;
    // 计数与取行在**同一个只读快照事务**里，总数与行必然同集合。
    let total = count_events(&mut tx, &state.corp, since, until, &paging).await?;
    tx.commit().await?;
    let (pages, truncated) = paging.pages(total)?;
    budget.frame("],\"total\":")?;
    budget.value(&total)?;
    budget.frame(",\"pages\":")?;
    budget.value(&pages)?;
    budget.frame(",\"truncated\":")?;
    budget.value(&truncated)?;
    budget.frame("}")?;
    Ok(budget.finish())
}

/// 深链接指向的单个事件。**混版判定由 SQL 出一列**（见 [`read_event`]），
/// 这里不再把文档解析出来、用字符串下标去摸 `taxonomy_version` ——
/// 那种写法字段改名时编译器管不着，而错了是静默的。
async fn event(State(state): State<WebState>, Id(id): Id<u64>) -> Result<Response, WebError> {
    let (document, stale) = read_event(&state.pool, &state.corp, id, CURRENT_VERSION)
        .await?
        .ok_or_else(|| WebError(StatusCode::NOT_FOUND, "事件不存在或已被重新抽取".into()))?;
    if stale {
        return Err(WebError(
            StatusCode::CONFLICT,
            "事件与当前词表版本不一致，请完成重打标".into(),
        ));
    }
    let mut budget = ResponseBudget::new(&state.limits);
    budget.document(&document)?;
    Ok(budget.finish())
}

/// 来源消息原文 —— **只读 `source_messages` 一列，不碰文件系统。**
///
/// 渲染快照在抽取时就写进去了（`extract::assemble`），所以这里没有 DuckDB 扫描、
/// 没有 `spawn_blocking`、也没有单独的扫描名额：一次主键查找而已。
/// raw 区的保留期从此只约束跑批的输入，**不再是下钻的可见范围**。
///
/// 列里存的字符串**就是响应体**，原样回给前端不做转换 —— 形状由
/// `extract::SourceMessage` 定义，改那边就是改前端契约。
pub(super) async fn messages(
    State(state): State<WebState>,
    Id(id): Id<u64>,
) -> Result<Response, WebError> {
    let row = read_source_messages(&state.pool, &state.corp, id).await?;
    let Some(rendered) = row else {
        return Err(WebError(
            StatusCode::NOT_FOUND,
            "事件不存在或已被重新抽取".into(),
        ));
    };
    // NULL = 这一行在加这一列之前就抽取过了，原文确实**永久**取不到 —— 410 在这里
    // 说的是实话。此前那个 410 把「镜像没同步 / raw_root 配错」也说成过保留期，
    // 于是一个可修的运维故障长得跟正常的数据过期一模一样。
    let Some(rendered) = rendered else {
        return Err(WebError(
            StatusCode::GONE,
            "该事件早于原文留存，取不到原文".into(),
        ));
    };
    let mut budget = ResponseBudget::new(&state.limits);
    budget.document(&rendered)?;
    Ok(budget.finish())
}
