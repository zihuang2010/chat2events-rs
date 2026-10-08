use super::*;
use crate::testutil::{self, scripted};
use serde_json::{Value, json};

const MERCHANTS: &str = "/rpc/merchantGroupConfigSummary/getMap";
const MANAGERS: &str = "/v1/rpc/account/operatorIdNameMap";

fn nacos() -> NacosConfig {
    NacosConfig {
        nacos: "http://unused.invalid".into(),
        namespace: "public".into(),
        group_name: "DEFAULT_GROUP".into(),
        merchant_service: "merchant-app".into(),
        employee_service: "account-app".into(),
        timeout_secs: 3,
    }
}

/// 刷新进程只要数据库、日志、Nacos 三节，跑批的模型 / OSS / 并发一概不要；
/// Nacos 的键与账号密码缺一个都是启动即崩，不是悄悄走一个谁都没写过的值。
#[test]
fn merchant_sync_startup_needs_only_database_log_and_nacos() {
    let mut value: toml::Value = toml::from_str(include_str!("../../../config.toml")).unwrap();
    let table = value.as_table_mut().unwrap();
    for key in ["daily", "extract", "classify", "llm", "ingest", "web"] {
        table.remove(key);
    }
    let dir = crate::testutil::fresh_root("config", "merchant-sync-only");
    std::fs::create_dir_all(&dir).unwrap();
    let write = |config: &toml::Value, secrets: &str| {
        std::fs::write(dir.join("config.toml"), toml::to_string(config).unwrap()).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(&path, secrets).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    };
    let secrets = "[mysql]\nurl = 'mysql://localhost/writer'\n\
                   [roster]\nusername = 'nacos'\npassword = 'nacos'\n";
    write(&value, secrets);
    let (cfg, secrets_ok) = load_from_dir(&dir);
    assert_eq!(cfg.roster.employee_service, "account-app");
    assert_eq!(cfg.roster.merchant_service, "merchant-app");
    assert_eq!(secrets_ok.mysql.url, "mysql://localhost/writer");
    assert_eq!(secrets_ok.roster.username, "nacos");

    let mut holed = value.clone();
    holed["roster"].as_table_mut().unwrap().remove("group_name");
    write(&holed, secrets);
    assert!(std::panic::catch_unwind(|| load_from_dir(&dir)).is_err());

    write(&value, "[mysql]\nurl = 'mysql://localhost/writer'\n");
    assert!(std::panic::catch_unwind(|| load_from_dir(&dir)).is_err());
}

/// 商家域把 `managerId` 下发成字符串（线上实测，超过 2^53）—— 数字、字符串、null、缺键都得接住，
/// 不是数字也不是十进制字符串的照样报错。
#[test]
fn manager_id_accepts_number_or_string() {
    let parse = |manager: Value| {
        serde_json::from_value::<HashMap<u64, MerchantSummary>>(
            json!({ "1": { "managerId": manager } }),
        )
        .map(|m| m[&1].manager_id)
    };
    assert_eq!(
        parse(json!("1638016126178816000")).unwrap(),
        Some(1638016126178816000)
    );
    assert_eq!(parse(json!(u64::MAX.to_string())).unwrap(), Some(u64::MAX));
    assert_eq!(parse(json!(7)).unwrap(), Some(7));
    assert_eq!(parse(Value::Null).unwrap(), None);
    let missing: HashMap<u64, MerchantSummary> =
        serde_json::from_value(json!({ "1": { "merchantName": "店一" } })).unwrap();
    assert_eq!(missing[&1].manager_id, None);
    assert!(parse(json!("abc")).is_err());
    assert!(parse(json!(1.5)).is_err());
}

/// 建库并造一张模拟的群配置表。
///
/// `merchant_id` 照上游 DDL 是 `BIGINT UNSIGNED`（2026-10-07 核实）。
/// 也故意带 `group_status` / `is_deleted`：刷新不按它们过滤，得有列才能证明。
async fn fixture(name: &str) -> MySqlPool {
    let pool = testutil::mysql_pool(name).await;
    sqlx::raw_sql(
        "CREATE TABLE b_wecom_merchant_group (\
             corp_id VARCHAR(64) NOT NULL, official_room_id VARCHAR(128) NOT NULL, \
             merchant_id BIGINT UNSIGNED NULL, group_status TINYINT NOT NULL DEFAULT 0, \
             is_deleted TINYINT NOT NULL DEFAULT 0, \
             UNIQUE KEY uk_corp_room (corp_id, official_room_id))",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// 每个商家编号一个群；`None` = 群没关联商家。
async fn seed_rooms(pool: &MySqlPool, merchants: &[Option<u64>]) {
    let rows: Vec<String> = merchants
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let m = m.map_or("NULL".to_owned(), |m| m.to_string());
            format!("('corp', 'room-{i}', {m})")
        })
        .collect();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO b_wecom_merchant_group (corp_id, official_room_id, merchant_id) VALUES {}",
        rows.join(", ")
    )))
    .execute(pool)
    .await
    .unwrap();
}

/// 商家域 `getMap` 里一个商家的摘要。带 `merchantStatus`：它在契约里存在，但我们不用。
/// `managerId` 照线上实测下发成**字符串**（数字形状由 `manager_id_accepts_number_or_string` 钉住）。
fn summary(name: &str, group: &str, manager: Option<u64>) -> Value {
    json!({"merchantName": name, "merchantGroupConfigName": group,
           "managerId": manager.map(|m| m.to_string()), "merchantStatus": 1})
}

fn ok(data: Value) -> Value {
    json!({"code": 1, "data": data})
}

/// `scripted` 的脚本：`(路径, 状态码, 应答)`，按到达顺序消费。
type Script = Vec<(&'static str, u16, Value)>;

/// 跑一次刷新：先起假上游（5 秒死线从这里开始计，所以库要先建好），再调编排函数。
/// 返回 `(结果, 假上游收到的 (请求行, 正文))`。
///
/// ⚠️ `server.join().unwrap()` 同时是一道断言：脚本里的每一条都必须被消费，
/// 否则假上游等不到请求会 panic —— 「失败后不许再调后面的接口」靠它钉住，
/// 而不是只看函数返回了 `Err`（多发一个未脚本化的请求也会得到 `Err`）。
async fn sync(pool: &MySqlPool, script: Script) -> (crate::Result<()>, Vec<(String, String)>) {
    let (base, server) = scripted(script);
    let discovery = Discovery::fixed(&[("merchant-app", &base), ("account-app", &base)]);
    let result = run(pool, &discovery, &nacos()).await;
    (result, server.join().unwrap())
}

type Row = (
    u64,
    u64,
    Option<String>,
    Option<String>,
    Option<u64>,
    Option<String>,
    String,
    String,
);

/// 整张表连 `id` 和两个时间戳一起读出来：「一个字节不变」要连这些一起比。
async fn table(pool: &MySqlPool) -> Vec<Row> {
    sqlx::query_as(
        "SELECT id, merchant_id, merchant_name, merchant_group_config_name, \
         business_manager_id, business_manager_name, \
         CAST(gmt_created_time AS CHAR), CAST(gmt_modified_time AS CHAR) \
         FROM b_merchant_group_merchant_summary ORDER BY merchant_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// 业务列：商家编号 · 商家名称 · 分组 · 经理编号 · 经理姓名。
type Content<'a> = (
    u64,
    Option<&'a str>,
    Option<&'a str>,
    Option<u64>,
    Option<&'a str>,
);

fn content(rows: &[Row]) -> Vec<Content<'_>> {
    rows.iter()
        .map(|r| (r.1, r.2.as_deref(), r.3.as_deref(), r.4, r.5.as_deref()))
        .collect()
}

fn body(request: &(String, String)) -> Value {
    serde_json::from_str(&request.1).unwrap()
}

/// 1001 个商家 → `getMap` 分 1000 + 1 两批串行，经理编号去重后只问账号域一次，
/// 全部成功才写表。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_sync_fills_the_table_from_two_batches_and_one_manager_lookup() {
    let pool = fixture("merchant_sync_fill").await;
    // 1..=1000 ＋ BIGINT UNSIGNED 的上界（u64::MAX，远超 2^53）：不丢精度；再混入同一商家的第二个群、
    // 一个没关联商家的群。
    let mut merchants: Vec<Option<u64>> = (1..=1000).map(Some).collect();
    merchants.push(Some(u64::MAX));
    merchants.push(Some(1));
    merchants.push(None);
    seed_rooms(&pool, &merchants).await;
    // 已解散、已删除的群，它的商家照常刷新。
    sqlx::query(
        "UPDATE b_wecom_merchant_group SET group_status = 1, is_deleted = 1 WHERE merchant_id = 7",
    )
    .execute(&pool)
    .await
    .unwrap();

    let max = u64::MAX;
    let (result, requests) = sync(
        &pool,
        vec![
            (
                MERCHANTS,
                200,
                ok(json!({
                    "1": summary("店一", "华东组", Some(100)),
                    "2": summary("店二", "未分组", Some(100)),
                    "3": summary("店三", "华东组", Some(u64::MAX)),
                    "4": summary("店四", "华北组", None),
                    "5": summary("店五", "华北组", Some(0)),
                    "7": summary("店七", "华北组", Some(200)),
                })),
            ),
            (
                MERCHANTS,
                200,
                ok(json!({ max.to_string(): summary("大号店", "未分组", Some(200)) })),
            ),
            (MANAGERS, 200, ok(json!({"100": "张三", "200": "李四"}))),
        ],
    )
    .await;
    result.unwrap();

    assert_eq!(requests.len(), 3);
    assert!(requests[0].0.starts_with(&format!("POST {MERCHANTS} ")));
    assert_eq!(
        body(&requests[0]),
        json!({"merchantIdList": (1..=1000).collect::<Vec<u64>>()})
    );
    assert!(requests[1].0.starts_with(&format!("POST {MERCHANTS} ")));
    assert_eq!(body(&requests[1]), json!({"merchantIdList": [max]}));
    // 去重（100 出现两次）、去 null 与 0，请求体是裸数组。
    assert!(requests[2].0.starts_with(&format!("POST {MANAGERS} ")));
    assert_eq!(body(&requests[2]), json!([100, 200, u64::MAX]));

    assert_eq!(
        content(&table(&pool).await),
        [
            (1, Some("店一"), Some("华东组"), Some(100), Some("张三")),
            // 「未分组」是上游的字面值，原样存下。
            (2, Some("店二"), Some("未分组"), Some(100), Some("张三")),
            // 有编号但账号域查不到：只有姓名是 NULL。
            (3, Some("店三"), Some("华东组"), Some(u64::MAX), None),
            // 商家没配经理：编号和姓名都是 NULL。
            (4, Some("店四"), Some("华北组"), None, None),
            // 经理编号 0 也是没配。
            (5, Some("店五"), Some("华北组"), None, None),
            (7, Some("店七"), Some("华北组"), Some(200), Some("李四")),
            (max, Some("大号店"), Some("未分组"), Some(200), Some("李四")),
        ]
    );
    testutil::drop_mysql_database(pool).await;
}

/// 摘要表里预置一行旧值，并把 `gmt_modified_time` 显式钉在 2000 年：
/// 之后只要这一行被碰过（值变了），时间就一定会离开 2000 年。
async fn seed_summary(pool: &MySqlPool, row: (u64, &str, &str, Option<u64>, Option<&str>)) {
    sqlx::query(
        "INSERT INTO b_merchant_group_merchant_summary \
         (merchant_id, merchant_name, merchant_group_config_name, business_manager_id, \
          business_manager_name, gmt_modified_time) \
         VALUES (?, ?, ?, ?, ?, '2000-01-01 00:00:00')",
    )
    .bind(row.0)
    .bind(row.1)
    .bind(row.2)
    .bind(row.3)
    .bind(row.4)
    .execute(pool)
    .await
    .unwrap();
}

/// 上游没返回的商家，原来那一行不碰；返回了的才更新，更新不换 `id`、不动 `gmt_created_time`。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_sync_never_touches_or_deletes_merchants_upstream_did_not_return() {
    let pool = fixture("merchant_sync_keep").await;
    seed_rooms(&pool, &[Some(1), Some(2), Some(3)]).await;
    seed_summary(&pool, (1, "旧店一", "旧组", Some(9), Some("旧经理"))).await;
    seed_summary(&pool, (2, "下线店", "华东组", Some(9), Some("旧经理"))).await;
    // 商家 99 连群配置表里都没有了，也不删。
    seed_summary(&pool, (99, "孤儿店", "未分组", None, None)).await;
    let before = table(&pool).await;

    let (result, _) = sync(
        &pool,
        vec![
            (
                MERCHANTS,
                200,
                ok(json!({
                    "1": summary("新店一", "华东组", Some(100)),
                    "3": summary("店三", "未分组", None),
                })),
            ),
            (MANAGERS, 200, ok(json!({"100": "张三"}))),
        ],
    )
    .await;
    result.unwrap();

    let after = table(&pool).await;
    assert_eq!(after.len(), 4);
    assert_eq!(after[1], before[1], "上游没返回的商家 2：一个字节不变");
    assert_eq!(after[3], before[2], "商家 99：不在群配置表里，也不删");
    assert_eq!(
        content(&after),
        [
            (1, Some("新店一"), Some("华东组"), Some(100), Some("张三")),
            (2, Some("下线店"), Some("华东组"), Some(9), Some("旧经理")),
            (3, Some("店三"), Some("未分组"), None, None),
            (99, Some("孤儿店"), Some("未分组"), None, None),
        ]
    );
    // 更新：同一行（id 与创建时间不变），修改时间离开了 2000 年。
    assert_eq!((after[0].0, &after[0].6), (before[0].0, &before[0].6));
    assert_ne!(after[0].7, before[0].7);
    testutil::drop_mysql_database(pool).await;
}

/// 没东西可问就不调：商家域 `merchantIdList` 带 `@NotEmpty`，空列表会被拒；
/// 账号域同理。空的那一侧上游一次都不该收到请求。
///
/// 群配置表里一个商家编号都没有则是**失败**：生产上这张表不会是空的，空了说明连错了库
/// 或上游表被清空，得让 systemd 标成 failed，而不是以 0 退出悄悄过去。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_sync_does_not_call_a_domain_it_has_nothing_to_ask() {
    let pool = fixture("merchant_sync_empty").await;
    // 服务发现里没有任何实例：调了上游也是 Err，所以靠错误文案区分是哪一步失败的。
    seed_rooms(&pool, &[None]).await;
    let none = Discovery::fixed(&[]);
    let error = run(&pool, &none, &nacos()).await.unwrap_err().to_string();
    assert!(error.contains("群配置表里没有任何商家编号"), "{error}");
    assert!(table(&pool).await.is_empty());

    // 商家都没配经理：只问商家域，不问账号域。
    sqlx::query("DELETE FROM b_wecom_merchant_group")
        .execute(&pool)
        .await
        .unwrap();
    seed_rooms(&pool, &[Some(1), Some(2)]).await;
    let (result, requests) = sync(
        &pool,
        vec![(
            MERCHANTS,
            200,
            ok(json!({"1": summary("店一", "未分组", None), "2": summary("店二", "未分组", Some(0))})),
        )],
    )
    .await;
    result.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(table(&pool).await.len(), 2);
    testutil::drop_mysql_database(pool).await;
}

/// 商家名称、经理姓名只有空白（trim 后为空）存 NULL —— 空白名等于没有名字，工作台读侧不再兜底，
/// 这里是唯一一处。分组名不动：原样存上游的值。
///
/// 同时钉住写入顺序：upsert 按商家编号升序，两个并发事务加锁顺序一致才不会死锁。
/// 空表上 `id` 是自增的，所以「编号小的 `id` 也小」就是行按升序写的证据；
/// 商家域返回的是 `HashMap`，不排序的话 8 个商家恰好有序的概率只有 1/40320。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_sync_stores_blank_names_as_null_and_writes_in_merchant_id_order() {
    let pool = fixture("merchant_sync_blank").await;
    let merchants: Vec<Option<u64>> = (1..=8).map(Some).collect();
    seed_rooms(&pool, &merchants).await;
    let (result, _) = sync(
        &pool,
        vec![
            (
                MERCHANTS,
                200,
                ok(json!({
                    "1": summary("  ", "华东组", Some(100)),
                    "2": summary("", "华东组", Some(200)),
                    // 非空白的名字原样存，不 trim。
                    "3": summary(" 店三 ", "  ", Some(300)),
                    "4": summary("店四", "华东组", None),
                    "5": summary("店五", "华东组", None),
                    "6": summary("店六", "华东组", None),
                    "7": summary("店七", "华东组", None),
                    "8": summary("店八", "华东组", None),
                })),
            ),
            (
                MANAGERS,
                200,
                ok(json!({"100": "   ", "200": "", "300": "王五"})),
            ),
        ],
    )
    .await;
    result.unwrap();

    let rows = table(&pool).await;
    assert_eq!(
        content(&rows)[..3],
        [
            (1, None, Some("华东组"), Some(100), None),
            (2, None, Some("华东组"), Some(200), None),
            // 分组 "  " 一字不动；经理姓名非空白原样存。
            (3, Some(" 店三 "), Some("  "), Some(300), Some("王五")),
        ]
    );
    let ids: Vec<u64> = rows.iter().map(|r| r.0).collect();
    assert!(ids.is_sorted(), "按商家编号升序写入，id 随之递增：{ids:?}");
    testutil::drop_mysql_database(pool).await;
}

async fn pin_stamps(pool: &MySqlPool) {
    sqlx::query(
        "UPDATE b_merchant_group_merchant_summary SET gmt_modified_time = '2000-01-01 00:00:00'",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// 任何一步失败：表一个字节不变（含 `id` 与两个时间戳）、返回错误、错误里说清是哪个域，
/// 而且失败之后**不再调后面的接口**（脚本里没有的请求，`sync` 里的 `join` 会让测试挂掉）。
///
/// 1001 个商家，所以有第二批；前面的批次已经成功过，照样不许落一行。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_sync_writes_nothing_when_any_upstream_step_fails() {
    let pool = fixture("merchant_sync_fail").await;
    let merchants: Vec<Option<u64>> = (1..=1001).map(Some).collect();
    seed_rooms(&pool, &merchants).await;
    seed_summary(&pool, (1, "旧店一", "旧组", Some(9), Some("旧经理"))).await;
    seed_summary(&pool, (42, "旧店四十二", "未分组", None, None)).await;
    let before = table(&pool).await;

    let first = || ok(json!({"1": summary("新店一", "华东组", Some(100))}));
    let second = || ok(json!({"1001": summary("店一〇〇一", "华东组", Some(200))}));
    let boom = json!({"code": 500, "message": "boom"});
    let with = |data: Value| {
        let mut body = boom.clone();
        body["data"] = data;
        body
    };
    let cases: Vec<(&str, Script, usize, &str)> = vec![
        (
            "账号域 HTTP 500",
            vec![
                (MERCHANTS, 200, first()),
                (MERCHANTS, 200, second()),
                (MANAGERS, 500, json!({})),
            ],
            3,
            "账号域",
        ),
        (
            "账号域 code != 1、data 为 null",
            vec![
                (MERCHANTS, 200, first()),
                (MERCHANTS, 200, second()),
                (MANAGERS, 200, with(Value::Null)),
            ],
            3,
            "账号域",
        ),
        (
            // 上游失败时 data 是 null 还是 {} 没有权威答案，所以 code != 1 带着 {} 也得判失败：
            // 只看 data 会把它当成「全部查无」写下去，把所有经理姓名洗成 NULL。
            "账号域 code != 1、data 为 {}",
            vec![
                (MERCHANTS, 200, first()),
                (MERCHANTS, 200, second()),
                (MANAGERS, 200, with(json!({}))),
            ],
            3,
            "账号域",
        ),
        (
            // 请求了非空的经理编号，账号域却答 code == 1 加空对象：不是「全部查无此人」，
            // 而是上游异常。当成成功写下去，所有经理姓名都会被洗成 NULL。
            // （部分查到、只缺某几个编号仍是成功 —— 见上面的 fill 用例里查不到姓名的 u64::MAX。）
            "账号域 code == 1 但 data 为 {}（请求了非空编号列表）",
            vec![
                (MERCHANTS, 200, first()),
                (MERCHANTS, 200, second()),
                (MANAGERS, 200, ok(json!({}))),
            ],
            3,
            "账号域",
        ),
        (
            "账号域 code == 1 但 data 为 null",
            vec![
                (MERCHANTS, 200, first()),
                (MERCHANTS, 200, second()),
                (MANAGERS, 200, json!({"code": 1, "data": null})),
            ],
            3,
            "账号域",
        ),
        (
            "商家域第二批 HTTP 500",
            vec![(MERCHANTS, 200, first()), (MERCHANTS, 500, json!({}))],
            2,
            "商家域",
        ),
        (
            "商家域第一批 code != 1",
            vec![(MERCHANTS, 200, with(json!({})))],
            1,
            "商家域",
        ),
    ];
    for (name, script, requests, domain) in cases {
        let (result, seen) = sync(&pool, script).await;
        let error = result.expect_err(name).to_string();
        assert!(error.contains(domain), "{name}：{error}");
        assert_eq!(seen.len(), requests, "{name}");
        assert_eq!(table(&pool).await, before, "{name}：表必须一个字节不变");
    }
    testutil::drop_mysql_database(pool).await;
}

/// 值没变的行 `gmt_modified_time` 不动 —— 工作台的缓存数据戳读这一列，
/// 每次刷新都推进它，白天的响应缓存就每 12 小时无谓作废两次。
///
/// 不 sleep：`DATETIME` 是秒精度。每轮之间把时间显式钉回 2000 年（显式赋值胜过
/// `ON UPDATE`），之后哪一行离开了 2000 年，哪一行就是被这一轮改过的。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_sync_advances_the_stamp_only_for_rows_whose_values_changed() {
    let pool = fixture("merchant_sync_stamp").await;
    seed_rooms(&pool, &[Some(1), Some(2), Some(3)]).await;
    let upstream = |manager_name: &str| {
        vec![
            (
                MERCHANTS,
                200,
                ok(json!({
                    "1": summary("店一", "华东组", Some(100)),
                    "2": summary("店二", "华东组", Some(200)),
                    // 商家 3 没配经理：NULL → NULL 也算没变。
                    "3": summary("店三", "未分组", None),
                })),
            ),
            (
                MANAGERS,
                200,
                ok(json!({"100": manager_name, "200": "李四"})),
            ),
        ]
    };
    sync(&pool, upstream("张三")).await.0.unwrap();
    let first = table(&pool).await;
    assert_eq!(first.len(), 3);
    pin_stamps(&pool).await;

    // 同样的上游应答再来一轮：一行都不动。
    sync(&pool, upstream("张三")).await.0.unwrap();
    let again = table(&pool).await;
    assert!(
        again.iter().all(|r| r.7 == "2000-01-01 00:00:00"),
        "{again:?}"
    );
    assert_eq!(content(&again), content(&first));

    // 经理 100 改了名：只有商家 1 那一行被推进。
    sync(&pool, upstream("张三丰")).await.0.unwrap();
    let renamed = table(&pool).await;
    assert_ne!(renamed[0].7, "2000-01-01 00:00:00");
    assert_eq!(renamed[0].5.as_deref(), Some("张三丰"));
    assert_eq!(renamed[1].7, "2000-01-01 00:00:00");
    assert_eq!(renamed[2].7, "2000-01-01 00:00:00");
    assert_eq!(
        renamed.iter().map(|r| r.0).collect::<Vec<_>>(),
        first.iter().map(|r| r.0).collect::<Vec<_>>(),
        "id 稳定 —— 这张表是 upsert 不是 REPLACE"
    );
    testutil::drop_mysql_database(pool).await;
}
