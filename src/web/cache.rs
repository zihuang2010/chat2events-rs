//! 白天的查询走内存缓存 —— 数据**只在夜里跑批时写**，白天每个请求都把同一个窗口
//! 重算一遍（一页 7 个请求、约 19 条扫窗口的 SQL）是白烧。
//!
//! **失效不靠时间，也不靠跑批来通知**（跑批不知道 webUI 存在）：每个请求先向库要一个
//! [`Stamp`] —— 四张只读表各自的「最后一次写」，全走主键 / 索引，与表多大无关。
//! 戳变了整个缓存作废。戳距现在不足 [`QUIET`] = 跑批可能还在写，这段时间**只查不存**，
//! 于是「跑批结束后才开始缓存」不需要任何人来通知，也不需要知道跑批几点跑。
//!
//! ⚠️ 戳必须在读数据**之前**取（中间件里取，handler 才开快照）：这样戳只可能比数据旧、
//! 不可能比数据新 —— 旧戳配新数据最多多一次 miss，新戳配旧数据才是静默的脏读。
//!
//! 只缓存 `200`：409（混版）/ 413（超预算）/ 400 都是「这次别看」，不是「下次也这样」。
//! 存的是**序列化后的字节**，命中时不再解析 JSON，也不再占 `ReadBudget` 那份内存。

use super::{budget::WebError, state::WebState};
use crate::stage::store::{T_EVENT, T_FAILURE, T_GROUP, T_TAXONOMY};
use axum::{
    body::Bytes,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
};
use chrono::NaiveDateTime;
use sqlx::MySqlPool;
use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

/// 最后一次写距现在不足这么久，视作「跑批还在写」，不缓存。
///
/// 它挡的是秒级时间戳的同秒竞争：`gmt_modified_time` 是 `DATETIME(0)`，缓存在第 T 秒
/// 装入、跑批的最后一笔也落在第 T 秒的话，戳不会变、那条缓存到下一夜都不会作废。
/// 让「距上次写 ≥ 60 秒」作为装入条件，同秒竞争就不存在了。跑批中途停顿超过它
/// （等一个大群的模型响应）只是多一次白装，下一笔写照样让戳变。
const QUIET: chrono::Duration = chrono::Duration::seconds(60);

/// 数据戳自缓存多久。
///
/// 每个只读请求 —— **包括缓存命中** —— 都先向库要一次戳（四张表各自的最后一次写）。
/// 打开一次页面是七个并发请求，于是七次数据库往返，而命中路径本该几乎免费。
///
/// 一秒**远小于** [`QUIET`] 的 60 秒静默期，所以**不改失效语义**：影响面是
/// 「跑批结束后的第一秒」最坏多命中一次旧缓存，下一秒自动纠正。
pub(super) const STAMP_TTL: Duration = Duration::from_secs(1);

/// 四张只读表的「最后一次写」。事件与群日表走 `gmt_modified_time`（`ON UPDATE` 会跟着
/// 打标的 `UPDATE` 变，`REPLACE` / `INSERT` 更不用说）—— 所以这两张表要有
/// `idx_modified (gmt_modified_time)`，`MAX` 才是一次索引尾读；失败表只增不改，
/// `MAX(id)` 走主键；词表几十行，扫一遍无所谓。
///
/// ⚠️ **`b_wecom_merchant_group`（群名）不在戳里** —— 那是别人的表，没有可用的时间列。
/// 群改名要重启 `webui` 或等下一次跑批。
///
/// ⚠️ **外部名册的代数也拼在戳上**（见 [`Cache::stamp`]）。名册活在进程内存里，
/// 它整体过期重建时库里一个字节都没变 —— 不拼进来就是「名册刷新了，页面照旧回旧
/// 响应」，而且那不会自愈。
type Stamp = String;

async fn read_stamp(pool: &MySqlPool) -> Result<(Stamp, bool), sqlx::Error> {
    // ⚠️ 表名走 `store::T_*`，**不抄字面量** —— 抄错在这里是**静默**的：
    // 戳查不到就是永远 quiet，缓存永不装入，页面只是变慢，没有任何东西会报错。
    // （`web/query.rs` 抄错会直接 500，看得见，所以那边的字面量本轮没动。）
    let (event, group, failure, taxonomy, now): (
        Option<NaiveDateTime>,
        Option<NaiveDateTime>,
        Option<u64>,
        Option<NaiveDateTime>,
        NaiveDateTime,
    ) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT (SELECT MAX(gmt_modified_time) FROM {T_EVENT}), \
                (SELECT MAX(gmt_modified_time) FROM {T_GROUP}), \
                (SELECT MAX(id) FROM {T_FAILURE}), \
                (SELECT MAX(gmt_modified_time) FROM {T_TAXONOMY}), \
                NOW()"
    )))
    .fetch_one(pool)
    .await?;
    let latest = [event, group, taxonomy].into_iter().flatten().max();
    let quiet = latest.is_none_or(|t| now.signed_duration_since(t) >= QUIET);
    Ok((
        format!("{event:?}|{group:?}|{failure:?}|{taxonomy:?}"),
        quiet,
    ))
}

/// 条目数上限 —— **护栏，不是容量规划**。
///
/// 字节上限封不住这条路：键是**完整 URI**，它的长度不计进 `max_bytes`，而各 handler
/// 的 `#[serde(flatten)]` 没有 `deny_unknown_fields`，多传一个无关参数照样返回 200
/// 并分叉出一个新键。看板无登录、缓存只在数据戳变化时清（每晚一次），于是
/// 「一百万个不同的键」既不超字节也不会被驱逐。
///
/// 白天真正用到的键是有限几组筛选，正常几十条 —— 一万这个数只是把上面那条路堵死。
/// 满了跟字节超限一样**整个清空**，不做 LRU（同 [`Cache::put`] 那条 `ponytail:`）。
const MAX_ENTRIES: usize = 10_000;

pub(super) struct Cache {
    max_bytes: usize,
    inner: Mutex<Inner>,
    /// 最近一次数据戳与它的读取时刻，见 [`STAMP_TTL`]。
    recent: Mutex<Option<(Instant, Stamp, bool)>>,
}

#[derive(Default)]
struct Inner {
    stamp: Stamp,
    bytes: usize,
    entries: HashMap<String, Bytes>,
}

impl Cache {
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            inner: Mutex::new(Inner::default()),
            recent: Mutex::new(None),
        }
    }

    /// 取数据戳，[`STAMP_TTL`] 之内复用上一次的结果。
    ///
    /// 取数那一步由调用方传进来 —— 于是「一秒内不重复查」这件事可以**离线断言**，
    /// 不必为了测一个计时器去起一个数据库。
    /// ⚠️ **代数在自缓存之外拼上**：库那一截可以缓一秒，名册代数不行 ——
    /// 它一变就必须当场作废，拖一秒就是又一次「明明刷新了却还是旧的」。
    async fn stamp_with<F, Fut>(
        &self,
        generation: u64,
        fetch: F,
    ) -> Result<(Stamp, bool), sqlx::Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(Stamp, bool), sqlx::Error>>,
    {
        // ⚠️ 锁不能跨 `await` —— 先取出再放开，取数在锁外面。
        let (stamp, quiet) = match &*self.recent.lock().unwrap_or_else(PoisonError::into_inner) {
            Some((at, stamp, quiet)) if at.elapsed() < STAMP_TTL => (stamp.clone(), *quiet),
            _ => (String::new(), false),
        };
        if !stamp.is_empty() {
            return Ok((format!("{stamp}|r{generation}"), quiet));
        }
        let (stamp, quiet) = fetch().await?;
        *self.recent.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), stamp.clone(), quiet));
        Ok((format!("{stamp}|r{generation}"), quiet))
    }

    pub(super) async fn stamp(
        &self,
        pool: &MySqlPool,
        generation: u64,
    ) -> Result<(Stamp, bool), sqlx::Error> {
        self.stamp_with(generation, || read_stamp(pool)).await
    }

    fn get(&self, stamp: &str, key: &str) -> Option<Bytes> {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        (inner.stamp == stamp)
            .then(|| inner.entries.get(key).cloned())
            .flatten()
    }

    fn put(&self, stamp: Stamp, key: String, body: Bytes) {
        if body.len() > self.max_bytes {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.stamp != stamp {
            inner.entries.clear();
            inner.bytes = 0;
            inner.stamp = stamp;
        }
        if let Some(old) = inner.entries.remove(&key) {
            inner.bytes -= old.len();
        }
        // ponytail: 满了整个清空，不做 LRU —— 白天的键是有限几组筛选，装不满；
        // 真装满了再换成按最近使用淘汰。
        if inner.bytes + body.len() > self.max_bytes || inner.entries.len() >= MAX_ENTRIES {
            inner.entries.clear();
            inner.bytes = 0;
        }
        inner.bytes += body.len();
        inner.entries.insert(key, body);
    }
}

fn tagged(mut response: Response, tag: &'static str) -> Response {
    response
        .headers_mut()
        .insert("x-cache", HeaderValue::from_static(tag));
    response
}

/// 缓存中间件。键是完整 URI（路径 + 查询串），`X-Cache` 回 `HIT` / `MISS` / `BYPASS`
/// —— `BYPASS` 是戳太新（跑批可能还在写），这次查了但没存。
pub(super) async fn cached(
    State(state): State<WebState>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let key = request.uri().to_string();
    let (stamp, quiet) = match state
        .cache
        .stamp(&state.pool, state.roster.generation())
        .await
    {
        Ok(v) => v,
        Err(e) => return WebError::from(e).into_response(),
    };
    if let Some(body) = state.cache.get(&stamp, &key) {
        return tagged(
            ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
            "HIT",
        );
    }
    let response = next.run(request).await;
    if !quiet {
        return tagged(response, "BYPASS");
    }
    if response.status() != StatusCode::OK {
        return response;
    }
    let (parts, body) = response.into_parts();
    // 响应体在 `bounded_json` 里已经按 `max_response_bytes` 封顶，这里的上限只是复述。
    let bytes = match axum::body::to_bytes(body, state.limits.max_response_bytes).await {
        Ok(b) => b,
        Err(e) => return WebError::from(e).into_response(),
    };
    state.cache.put(stamp, key, bytes.clone());
    tagged(Response::from_parts(parts, bytes.into()), "MISS")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 一秒内只查一次戳 —— 命中路径本该几乎免费，而此前每个请求都要一次往返。
    #[tokio::test]
    async fn the_stamp_is_read_once_per_second() {
        let cache = Cache::new(10);
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(("s1".to_owned(), true)))
        };
        for _ in 0..5 {
            assert_eq!(
                cache.stamp_with(0, fetch).await.unwrap(),
                ("s1|r0".into(), true)
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        // 过期之后再查一次 —— 戳变了整个缓存照旧作废，失效语义不变。
        *cache.recent.lock().unwrap() = None;
        assert_eq!(
            cache.stamp_with(0, fetch).await.unwrap(),
            ("s1|r0".into(), true)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// 名册代数一变，整个响应缓存作废 —— 库里一个字节都没动也要作废，
    /// 因为名册活在进程内存里。**不必起数据库**：取戳那一步是参数。
    ///
    /// 同时钉住代数**在一秒自缓存之外**拼上：库那一截照旧只查一次，
    /// 而代数变化当场生效，不用等自缓存过期。
    #[tokio::test]
    async fn a_new_roster_generation_invalidates_every_cached_response() {
        let cache = Cache::new(64);
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(("db".to_owned(), true)))
        };
        let (before, _) = cache.stamp_with(7, fetch).await.unwrap();
        cache.put(
            before.clone(),
            "/api/meta".into(),
            Bytes::from_static(b"old"),
        );
        assert_eq!(
            cache.get(&before, "/api/meta").as_deref(),
            Some(&b"old"[..])
        );

        // 名册整体过期 ⇒ 代数 +1。库没变，取戳也仍然走自缓存（calls 不增）。
        let (after, _) = cache.stamp_with(8, fetch).await.unwrap();
        assert_ne!(before, after, "代数没进戳，名册刷新后页面会一直回旧响应");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "代数不该让库那一截重查");
        assert_eq!(cache.get(&after, "/api/meta"), None, "旧响应必须整体作废");
    }

    #[test]
    fn stamp_change_drops_everything_and_bytes_are_capped() {
        let cache = Cache::new(10);
        cache.put("s1".into(), "/a".into(), Bytes::from_static(b"1234"));
        assert_eq!(cache.get("s1", "/a").as_deref(), Some(&b"1234"[..]));
        // 别的戳看不到，且不会误伤已有内容
        assert_eq!(cache.get("s2", "/a"), None);
        assert_eq!(cache.get("s1", "/a").as_deref(), Some(&b"1234"[..]));
        // 同键覆盖只算一份
        cache.put("s1".into(), "/a".into(), Bytes::from_static(b"12345"));
        cache.put("s1".into(), "/b".into(), Bytes::from_static(b"12345"));
        assert_eq!(cache.inner.lock().unwrap().bytes, 10);
        // 装不下：整个清空后再装
        cache.put("s1".into(), "/c".into(), Bytes::from_static(b"1"));
        assert_eq!(cache.get("s1", "/a"), None);
        assert_eq!(cache.get("s1", "/c").as_deref(), Some(&b"1"[..]));
        // 单条超过上限：不存，也不清别人
        cache.put("s1".into(), "/d".into(), Bytes::from_static(b"12345678901"));
        assert_eq!(cache.get("s1", "/d"), None);
        assert_eq!(cache.get("s1", "/c").as_deref(), Some(&b"1"[..]));
        // 条目数上限：字节没超也会清 —— 键本身不计字节，光靠 `max_bytes` 封不住
        // 「很多个很短的响应」那条路（无关 query 参数就能分叉出新键）。
        let many = Cache::new(usize::MAX);
        for i in 0..MAX_ENTRIES {
            many.put("s".into(), format!("/k{i}"), Bytes::from_static(b"1"));
        }
        assert_eq!(many.inner.lock().unwrap().entries.len(), MAX_ENTRIES);
        many.put("s".into(), "/overflow".into(), Bytes::from_static(b"1"));
        assert_eq!(many.inner.lock().unwrap().entries.len(), 1);
        assert_eq!(many.get("s", "/k0"), None);
        // 戳变了：旧的全部作废
        cache.put("s2".into(), "/x".into(), Bytes::from_static(b"9"));
        assert_eq!(cache.get("s1", "/c"), None);
        assert_eq!(cache.get("s2", "/x").as_deref(), Some(&b"9"[..]));
        assert_eq!(cache.inner.lock().unwrap().bytes, 1);
    }
}
