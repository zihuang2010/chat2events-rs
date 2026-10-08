//! 商家摘要刷新 —— 把商家名称 / 商家分组 / 业务经理落进 `b_merchant_group_merchant_summary`，
//! 给 BI 直连和只读工作台用。由 systemd timer 定时拉起（`src/bin/merchant_sync.rs`），
//! **跑完即退出**。它**不是七阶段的一环**：不调用任何阶段的处理逻辑，只借 `stage::store` 的表名常量；
//! **跑批不读这张表、不检查它在不在**（所以它不进 `store::check_schema`）—— 这边刷新失败永远影响不到跑批。
//!
//! # 链路
//!
//! 群配置表（`b_wecom_merchant_group`，上游的表，本项目只读）取全部商家编号 →
//! 商家域 `getMap` 分批问名称 / 分组 / 经理编号 → 经理编号去重后问账号域**一次**拿姓名 →
//! **一个事务** upsert。上游的表知识（表名、字段名）整体住在本文件。
//!
//! # 取舍（结论都在这里，`docs/adr/` 已删）
//!
//! - **只存当前归属，只 upsert、永不删行。** 上游没返回的商家（已删除 / 查不到）原行不碰，
//!   保留最后一次已知的值 —— 历史群不该因为商家下线就变成一串编号。上游不提供历史，
//!   换了经理的商家历史上属于谁补不回来，这一点是接受过的。
//! - **输入不按 `group_status` / `is_deleted` 过滤。** 群解散只是 `group_status` 改了，行与
//!   `merchant_id` 都在；已解散群的历史数据照样会出现在报表里，它的商家也得有名字。
//! - **失败一行不写、非零退出、进程内不重试。** Nacos / 商家域 / 账号域 / 数据库任何一步失败，
//!   整轮作废；下一轮定时运行就是重试（12 小时一次，页面上的值最多旧半天）。
//!   ⚠️ 尤其**不能拿上游失败当「全部查无」写下去**：账号域挂了而继续写，就是把所有经理姓名
//!   洗成 NULL。成功判据沿用名册的规则 —— `code == 1` 且 `data` 不是 null；在此之上
//!   **账号域多一条刷新进程自己的规则**：请求了非空经理编号却拿回 `{}`，也算失败
//!   （名册对 `{}` 仍是合法成功，不受影响）。部分查到（`data` 非空、缺某几个编号）仍是成功，
//!   缺的姓名存 NULL。
//! - **商家域一批最多 1000、串行发。** 服务端上限就是 1000，超了整批报错；串行是不给上游压力
//!   （一轮请求数 = 商家数 ÷ 1000 向上取整 ＋ 1）。**空集合不调**：商家域 `@NotEmpty`，空列表
//!   会被拒。
//! - **商家名称、经理姓名只有空白存 NULL，且按商家编号升序写。** 空白名等于没有名字，写入侧
//!   清洗一次，工作台读侧不再兜底（只留这一处）；分组名不动，原样存上游的值。升序是为了两个
//!   并发事务加锁顺序一致，不会死锁（`BTreeMap` 自带）。
//! - **经理编号 0 当作没配经理。** 与 null 同待遇：不问账号域、编号和姓名都存 NULL。
//! - **编号全程 `u64`。** 商家编号和经理编号都是上游的 `Long`，不经 `f64`。
//! - **「值不变不推进 `gmt_modified_time`」靠 `ON UPDATE CURRENT_TIMESTAMP` 的同值不更新语义**，
//!   所以 upsert 里**不写**这一列、也不能用 `REPLACE`（它是删了重插，每次都变，还换 `id`）。
//!   工作台的缓存数据戳读这一列：每次刷新都推进它，白天的响应缓存就每 12 小时无谓作废两次。
//!
//! # 配置
//!
//! 只要 `[mysql]` / `[log]` / Nacos 三节，**不要** `Boot::load` 那整套（模型、OSS、跑批并发）。
//! Nacos 那一节在文件里叫 `[roster]`（历史上只有工作台名册用它），键归 [`NacosConfig`]，
//! 名册自己的 `ttl_secs` 在这里是多余键、被 serde 忽略。账号密码与数据库同在 `secrets.toml`。

use crate::config::{LogConfig, MysqlConfig, MysqlSecrets, load, require_owner_only};
use crate::nacos::{Discovery, NacosConfig, NacosSecrets};
// 只借表名常量（让数据戳与筛选子查询读到同一个名字），不调用任何阶段的处理逻辑。
use crate::stage::store::T_MERCHANT_SUMMARY;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use sqlx::MySqlPool;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    time::Duration,
};

/// 商家域（`merchant-app`）的批量商家摘要。
const MERCHANT_PATH: &str = "/rpc/merchantGroupConfigSummary/getMap";

/// 账号域（`account-app`）的「人员主键 → 姓名」。请求体是**裸 JSON 数组**，服务端没有条数上限。
const MANAGER_PATH: &str = "/v1/rpc/account/operatorIdNameMap";

/// `getMap` 一批最多几个商家 —— 服务端上限，超了整批报错，所以分批是调用方的责任。
const MERCHANT_BATCH: usize = 1000;

/// 一条 upsert 语句最多几行（5 列 × 1000 = 5000 个占位符，远低于 MySQL 的 65535）。
const UPSERT_BATCH: usize = 1000;

/// 统一包装 `Result` 的成功码。**是 1，不是 0 也不是 200**，与 `web::roster` 同一份契约。
const RESULT_OK: i64 = 1;

#[derive(Deserialize)]
pub struct SyncConfig {
    pub mysql: MysqlConfig,
    pub log: LogConfig,
    pub roster: NacosConfig,
}

#[derive(Deserialize)]
pub struct SyncSecrets {
    pub mysql: MysqlSecrets,
    pub roster: NacosSecrets,
}

/// 加载配置，缺键直接 panic（错误要在进程起来第一秒暴露）。
/// 文件读取与 0600 权限检查跟跑批共用 `config` 的那一份。
pub fn load_from_dir(dir: &Path) -> (SyncConfig, SyncSecrets) {
    let config: SyncConfig = load(&dir.join("config.toml"), false);
    assert!(
        config.mysql.max_connections > 0
            && config.mysql.acquire_timeout_secs > 0
            && config.roster.timeout_secs > 0,
        "MySQL 连接数、等待时间与 HTTP 超时必须大于零"
    );
    let secrets = dir.join("secrets.toml");
    #[cfg(unix)]
    require_owner_only(&secrets);
    (config, load(&secrets, true))
}

/// 外层包装 `Result<T>`。`code` / `message` 只在失败时有意义。
#[derive(Deserialize)]
struct Envelope<T> {
    code: Option<i64>,
    message: Option<String>,
    data: Option<T>,
}

/// 商家域给的摘要。只取三个字段；`merchantStatus` 在契约里有，但不进表（表里只存当前值，
/// 以后要再加也不会丢什么），serde 默认忽略多余键。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MerchantSummary {
    merchant_name: Option<String>,
    /// 没配分组或分组已删时，上游自己补字面量「未分组」；原样存，不翻译成 NULL。
    merchant_group_config_name: Option<String>,
    manager_id: Option<u64>,
}

/// 要写进表的一行。
struct Row {
    merchant_id: u64,
    name: Option<String>,
    group: Option<String>,
    manager_id: Option<u64>,
    manager_name: Option<String>,
}

struct Upstream<'a> {
    http: reqwest::Client,
    discovery: &'a Discovery,
}

impl Upstream<'_> {
    /// POST 一次 JSON，返回 `data`。`what` 是「哪个域、哪一步、几个编号」，原样进错误文案 ——
    /// 这就是失败日志（进程非零退出，错误由入口打到 stderr）。
    ///
    /// 不用 `.json()` / `Response::json()`：reqwest 的 `json` 特性是别的依赖顺手开的，
    /// 别新依赖这个隐式开启（同 `web::roster`）。
    async fn post<T: DeserializeOwned>(
        &self,
        service: &str,
        path: &str,
        body: String,
        what: &str,
    ) -> crate::Result<T> {
        let instance = self
            .discovery
            .pick(service)
            .ok_or_else(|| format!("{what}：Nacos 尚无服务 `{service}` 的健康实例"))?;
        let response = self
            .http
            .post(format!("{instance}{path}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("{what}：调用失败：{e}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("{what}：返回 HTTP {status}（{instance}）").into());
        }
        let answer: Envelope<T> =
            serde_json::from_str(&text).map_err(|e| format!("{what}：应答无法解析：{e}"))?;
        // `code != 1` 或 `data` 缺席都是失败，绝不当成「全部查无」。
        Ok(answer
            .data
            .filter(|_| answer.code == Some(RESULT_OK))
            .ok_or_else(|| {
                format!(
                    "{what}：应答异常：code={:?}（成功是 {RESULT_OK}） message={:?}",
                    answer.code, answer.message
                )
            })?)
    }
}

/// 空白（trim 后为空）等于没有名字，存 NULL。名称与姓名都过这一道，工作台读侧不再兜底；
/// 分组名不过 —— 原样存上游的值。
fn non_blank(name: Option<String>) -> Option<String> {
    name.filter(|n| !n.trim().is_empty())
}

/// 刷新一轮。任何一步失败都返回 `Err`、表一个字节不动。
pub async fn run(
    pool: &MySqlPool,
    discovery: &Discovery,
    nacos: &NacosConfig,
) -> crate::Result<()> {
    // `CAST ... AS UNSIGNED`：上游的 `merchant_id` 是有符号 BIGINT（Java `Long`），
    // sqlx 解码 `u64` 要求列带 UNSIGNED 标志，直接读会在生产报类型不匹配。
    let ids: Vec<u64> = sqlx::query_scalar(
        "SELECT DISTINCT CAST(merchant_id AS UNSIGNED) AS id FROM b_wecom_merchant_group \
         WHERE merchant_id IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("读群配置表的商家编号失败：{e}"))?;
    if ids.is_empty() {
        tracing::info!("群配置表里没有商家编号，无需刷新");
        return Ok(());
    }

    let upstream = Upstream {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(nacos.timeout_secs))
            .build()?,
        discovery,
    };
    let batches = ids.len().div_ceil(MERCHANT_BATCH);
    // `BTreeMap`：之后按商家编号升序 upsert，两个并发事务加锁顺序一致才不会死锁。
    let mut found: BTreeMap<u64, MerchantSummary> = BTreeMap::new();
    for (i, batch) in ids.chunks(MERCHANT_BATCH).enumerate() {
        let what = format!(
            "商家域 `{}` 第 {}/{batches} 批（{} 个编号）",
            nacos.merchant_service,
            i + 1,
            batch.len()
        );
        let body = serde_json::to_string(&json!({ "merchantIdList": batch }))?;
        found.extend(
            upstream
                .post::<HashMap<u64, MerchantSummary>>(
                    &nacos.merchant_service,
                    MERCHANT_PATH,
                    body,
                    &what,
                )
                .await?,
        );
    }

    let managers: BTreeSet<u64> = found
        .values()
        .filter_map(|s| s.manager_id)
        .filter(|id| *id != 0)
        .collect();
    let mut requests = batches;
    let names = if managers.is_empty() {
        HashMap::new()
    } else {
        requests += 1;
        let what = format!(
            "账号域 `{}` 查经理姓名（{} 个编号）",
            nacos.employee_service,
            managers.len()
        );
        let names = upstream
            .post::<HashMap<u64, Option<String>>>(
                &nacos.employee_service,
                MANAGER_PATH,
                serde_json::to_string(&managers)?,
                &what,
            )
            .await?;
        // 上游对「查不到的编号」是不放进 map，所以 `{}` 本该是「一个都没查到」；但 `data` 在失败时
        // 到底是 null 还是 `{}` 没有权威答案，拿它当成功写下去就会把全部经理姓名洗成 NULL。
        // 宁可整轮失败、下一轮重试。
        if names.is_empty() {
            return Err(format!(
                "{what}：应答 code={RESULT_OK} 但 data 为空对象，请求了编号却一个都没查到，按上游异常处理"
            )
            .into());
        }
        names
    };

    let rows: Vec<Row> = found
        .into_iter()
        .map(|(merchant_id, s)| {
            let manager_id = s.manager_id.filter(|id| *id != 0);
            Row {
                merchant_id,
                name: non_blank(s.merchant_name),
                group: s.merchant_group_config_name,
                manager_id,
                manager_name: non_blank(
                    manager_id.and_then(|id| names.get(&id).cloned().flatten()),
                ),
            }
        })
        .collect();
    let (inserted, updated) = upsert(pool, &rows)
        .await
        .map_err(|e| format!("写商家摘要表失败（{} 个商家）：{e}", rows.len()))?;
    tracing::info!(
        merchants = ids.len(),
        requests,
        rows = rows.len(),
        inserted,
        updated,
        "商家摘要刷新完成"
    );
    Ok(())
}

/// 一个事务 upsert，返回 `(新增行数, 更新行数)`。`tx` 在任何一步 `?` 早退时被 drop，sqlx 自动回滚。
///
/// ⚠️ 不能用 `rows_affected` 直接当「变化行数」：sqlx 建连接时开了 `CLIENT_FOUND_ROWS`，
/// `ON DUPLICATE KEY UPDATE` 下新增 = 1、**值没变 = 1**、真更新 = 2，所以更新数 =
/// `rows_affected - 行数`；新增数用事务前后的 `COUNT(*)` 之差。
async fn upsert(pool: &MySqlPool, rows: &[Row]) -> Result<(u64, u64), sqlx::Error> {
    let count = format!("SELECT COUNT(*) FROM {T_MERCHANT_SUMMARY}");
    let mut tx = pool.begin().await?;
    let before: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(count.clone()))
        .fetch_one(&mut *tx)
        .await?;
    let mut affected = 0;
    for chunk in rows.chunks(UPSERT_BATCH) {
        let sql = format!(
            "INSERT INTO {T_MERCHANT_SUMMARY} (merchant_id, merchant_name, merchant_group_config_name, \
             business_manager_id, business_manager_name) VALUES {} \
             ON DUPLICATE KEY UPDATE merchant_name = VALUES(merchant_name), \
             merchant_group_config_name = VALUES(merchant_group_config_name), \
             business_manager_id = VALUES(business_manager_id), \
             business_manager_name = VALUES(business_manager_name)",
            vec!["(?, ?, ?, ?, ?)"; chunk.len()].join(", ")
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = q
                .bind(r.merchant_id)
                .bind(&r.name)
                .bind(&r.group)
                .bind(r.manager_id)
                .bind(&r.manager_name);
        }
        affected += q.execute(&mut *tx).await?.rows_affected();
    }
    let after: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(count))
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(((after - before) as u64, affected - rows.len() as u64))
}

#[cfg(test)]
mod tests;
