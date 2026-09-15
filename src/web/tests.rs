use super::config::WebLimits;
use super::{budget::*, query::*, serve::*, state::*};
use crate::testutil;
use axum::{Router, http::StatusCode, middleware, routing::get};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

/// GET 一个 JSON 接口。**先看状态码再解析。**
///
/// 直接 `serde_json::from_str(&res.text())` 的话，后端 500 出来的空体只会报
/// 「expected value at line 1 column 1」—— 看不出是哪个接口，更看不出后端说了什么。
/// 出错时把 URL、状态码和响应体一起打出来，一次就能定位。
async fn get_json(http: &reqwest::Client, url: String) -> serde_json::Value {
    let res = http.get(&url).send().await.unwrap();
    let status = res.status();
    let body = res.text().await.unwrap();
    assert!(status.is_success(), "{url} 返回 {status}：{body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{url} 返回的不是 JSON（{e}）：{body}"))
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_http_dataset_and_evidence_obey_the_read_contract() {
    use serde_json::{Value, json};
    let _log = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::ERROR)
            .finish(),
    );
    let pool = testutil::mysql_pool("web").await;
    sqlx::raw_sql(
        "CREATE TABLE b_wecom_merchant_group (\
         corp_id VARCHAR(64) NOT NULL, official_room_id VARCHAR(128) NOT NULL, \
         group_name VARCHAR(255) NOT NULL DEFAULT '', merchant_id BIGINT UNSIGNED NULL, \
         is_deleted TINYINT UNSIGNED NOT NULL DEFAULT 0, UNIQUE KEY uk_corp_room (corp_id, official_room_id)\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci; \
         INSERT INTO b_wecom_merchant_group (corp_id,official_room_id,group_name,merchant_id,is_deleted) VALUES \
         ('C','R','商家服务群',18446744073709551615,1), \
         ('other','R','其他企业群',2,0), ('C','empty-name','',NULL,0), \
         ('C','unused-room','未产生记录的群',3,0); \
         INSERT INTO b_merchant_group_event \
         (corpid,roomid,source_msg_ids,first_msg_time,last_msg_time,first_agent_reply_time,occurred_on,asker,asker_role,agents,first_responder,summary,last_msg_role,event_type,taxonomy_version,source_messages) \
         VALUES ('C','R',JSON_ARRAY('m1','m2'),'2026-08-25 23:55:00','2026-08-26 00:05:00','2026-08-26 00:05:00','2026-08-25', \
         'merchant00000001','EXTERNAL',JSON_ARRAY('agent00000000001'),'agent00000000001','商家要求改期，平台已受理','INTERNAL','reschedule','v1', \
         CAST(JSON_ARRAY(\
           JSON_OBJECT('msg_id','m1','at','2026-08-25 23:55:00','sender_id','merchant00000001','sender_role','EXTERNAL','text','请改期，原文保留'),\
           JSON_OBJECT('msg_id','m2','at','2026-08-26 00:05:00','sender_id','agent00000000001','sender_role','INTERNAL','text','稍等，已受理')\
         ) AS CHAR)), \
         ('C','R',JSON_ARRAY('m3','m4'),'2026-08-26 09:00:00','2026-08-26 09:10:00','2026-08-26 09:10:00','2026-08-26', \
         'merchant00000001','EXTERNAL',JSON_ARRAY('agent00000000001'),'agent00000000001','商家问上门时间，平台已答复','INTERNAL','reschedule','v1', \
         CAST(JSON_ARRAY(\
           JSON_OBJECT('msg_id','m3','at','2026-08-26 09:00:00','sender_id','merchant00000001','sender_role','EXTERNAL','text','几点上门'),\
           JSON_OBJECT('msg_id','m4','at','2026-08-26 09:10:00','sender_id','agent00000000001','sender_role','INTERNAL','text','十点前')\
         ) AS CHAR)); \
         INSERT INTO b_merchant_group_metric_daily \
         (corpid,roomid,dt,msg_count,sender_count,event_count,merchant_event_count,unreplied_count,first_reply_p50_sec,first_reply_p90_sec,extraction_status,agent_accounts,gmt_modified_time,fact_completed_time) \
         VALUES ('C','R','2026-08-25',1,1,1,1,0,600,600,'ok',JSON_OBJECT('agent00000000001','stale.account'),'2026-08-30 10:00:00','2026-08-30 10:00:00'), \
         ('C','R','2026-08-26',1,1,1,1,0,600,600,'ok',JSON_OBJECT('agent00000000001','zhang.san','agent00000000009','never.in.any.event'),'2026-08-30 10:00:00','2026-08-30 10:00:00'); \
         INSERT INTO b_merchant_group_taxonomy (version,type_id,parent_name,name,description) \
         VALUES ('v1','reschedule','订单变更','改期','修改上门服务日期');"
    ).execute(&pool).await.unwrap();
    let at = |d, h, m| {
        chrono::NaiveDate::from_ymd_opt(2026, 8, d)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    };
    // ⚠️ **没有 raw 夹具了。** 只读工作台不再读文件 —— 原文来自
    // `b_merchant_group_event.source_messages`，上面那条 INSERT 就是全部输入。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let app = router(WebState {
        pool: pool.clone(),
        corp: "C".into(),
        limits: toml::from_str::<super::config::WebConfig>(include_str!("../../config.toml"))
            .unwrap()
            .web,
        requests: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
        cache: std::sync::Arc::new(super::cache::Cache::new(1 << 20)),
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = signal.await;
            })
            .await
            .unwrap();
    });
    let http = reqwest::Client::new();
    let response = http
        .get(format!("{base}/api/dataset?from=2026-08-25&to=2026-08-26"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let data: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(data["meta"]["days"], json!(["2026-08-25", "2026-08-26"]));
    // 客服别名取 `agent_accounts` 里的 officialUserId：**窗口内最新的那一天赢**
    // （08-26 覆盖 08-25 的 stale.account），且只给真出现在事件里的人 ——
    // `agent00000000009` 只在账号映射里、没进过任何 `event.agents`，不该冒出来。
    assert_eq!(
        data["meta"]["agents"],
        json!([{"agent": "agent00000000001", "alias": "zhang.san"}])
    );
    // ⚠️ 它是账号不是姓名，所以这一位仍然是 false。
    assert_eq!(data["meta"]["alias_is_authoritative"], false);
    assert_eq!(
        data["meta"]["rooms"],
        json!([{
            "roomid": "R", "alias": "商家服务群", "merchant_id": "18446744073709551615",
            "alias_is_authoritative": true
        }])
    );
    assert_eq!(data["meta"]["alias_is_authoritative"], false);
    let meta: Value = http
        .get(format!("{base}/api/meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(meta, data["meta"]);
    // ⚠️ **`/api/dataset` 不再带事件明细** —— 它现在只回 meta ＋ 群日记录。
    // 明细一律走 `/api/events` 翻页，指标一律走聚合接口。
    assert!(data.get("events").is_none(), "dataset 不该再带事件明细");
    assert_eq!(data["groupDaily"][0]["freshness"], "known");
    let page: Value = serde_json::from_str(
        &http
            .get(format!(
                "{base}/api/events?from=2026-08-25&to=2026-08-26&page=1&page_size=10"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    // ⚠️ **翻页契约整个在响应体里**：行 ＋ 总数 ＋ 页数 ＋ 是否被护栏夹过。
    // 前端不再借 `/api/summary` 的事件总数推算分页 —— 那是另一个集合
    // （只算已知成功群日），窗口里一有失败的群日两个数就不等。
    assert_eq!(page["rows"].as_array().unwrap().len(), 2);
    assert_eq!(page["total"], 2);
    assert_eq!(page["pages"], 1);
    assert_eq!(page["truncated"], false);
    assert_eq!(page["rows"][0]["source_msg_ids"], json!(["m1", "m2"]));
    assert_eq!(page["rows"][0]["first_msg_time"], "2026-08-25 23:55:00");
    let id = page["rows"][0]["id"].as_u64().unwrap();
    let detail: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/event/{id}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(detail, page["rows"][0]);
    let response = http
        .get(format!("{base}/api/event/{id}/messages"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let messages: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(messages[0]["text"], "请改期，原文保留");
    assert_eq!(messages[1]["at"], "2026-08-26 00:05:00");

    // ── 聚合接口：每个数字都必须和前端 `aggregate()` 的口径一致 ──────────────
    // 夹具是两个商家发起、都已回复的事件：
    //   e1 08-25 23:55 → 08-26 00:05 —— **整段落在工作时段外**，工作秒数 0（跨零点）；
    //   e2 08-26 09:00 → 09:10 —— 时段内，工作秒数 600。
    //
    // ⚠️ **首响一律是工作时段口径**（`worktime::sql_between`，`[08:30, 21:00)`），
    // 不是墙钟差。e1 那 10 分钟墙钟落在 23:55–00:05，一秒都不计入 —— 拿它当
    // 600 秒的话，这组断言就完全测不出口径漂移了（曾经就是这么写的）。
    let sum: Value = serde_json::from_str(
        &http
            .get(format!(
                "{base}/api/summary?from=2026-08-25&to=2026-08-26&sla_sec=300&buckets=60,300,900"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(sum["events"], 2);
    assert_eq!(sum["rooms"], 1);
    assert_eq!(sum["agents"], 1);
    // 来源消息按 (企业, 群, msg_id) 去重：两个事件各 2 条，无重叠 → 4。
    assert_eq!(sum["sourceMessages"], 4);
    assert_eq!(sum["merchant"], 2, "asker_role=EXTERNAL");
    assert_eq!(sum["replied"], 2);
    assert_eq!(sum["unreplied"], 0);
    assert_eq!(sum["push"], 0);
    // secs 升序 [0, 600]，n=2。前端 quantile：i = min(n-1, floor(n*p))
    //   p50 → min(1, 1) = 1 → 600；p90 → min(1, floor(1.8)=1) = 1 → 600。
    // SQL 那份是 1-based 的 LEAST(n, FLOOR(n*p)+1) = 2 → 同一个值。**两边必须相等。**
    assert_eq!(sum["p50"], 600);
    assert_eq!(sum["p90"], 600);
    // e2 的 600 > 300 算超时，e1 的 0 不算。**未回复的也算超时**，这里没有未回复的。
    assert_eq!(sum["overdue"], 1);
    assert_eq!(sum["overdueRate"], 0.5);
    assert_eq!(sum["unrepliedRate"], 0.0);
    // 事件跨了零点：occurred_on=08-25 而 last_msg_time 落在 08-26。只有 e1 是。
    assert_eq!(sum["crossDay"], 1);
    assert_eq!(sum["backlog"], 0, "已回复的不算积压");
    // 按天：只出确实有事件的天，缺的天由前端补零。
    assert_eq!(
        sum["byDay"],
        json!([
            {"day": "2026-08-25", "events": 1, "merchant": 1, "unreplied": 0,
             "overdue": 0, "overdueRate": 0.0, "p50": 0, "p90": 0},
            {"day": "2026-08-26", "events": 1, "merchant": 1, "unreplied": 0,
             "overdue": 1, "overdueRate": 1.0, "p50": 600, "p90": 600},
        ])
    );
    // 按小时：按**首条消息**归入。e2 在 09 点且超时，e1 在 23 点不超时。
    assert_eq!(
        sum["byHour"],
        json!([
            {"hour": 9, "events": 1, "overdue": 1},
            {"hour": 23, "events": 1, "overdue": 0},
        ])
    );
    // 直方图：3 个边界 → 4 个桶，第一个是闭区间 [0,60]，其余左开右闭。
    // e1 的 0 落进桶 0，e2 的 600 落进 (300,900] 也就是桶 2。
    assert_eq!(sum["replyBuckets"], json!([1, 0, 1, 0]));
    // 不给 buckets 就不出直方图 —— 后端不自带一份默认分桶。
    let no_buckets: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/summary?from=2026-08-25&to=2026-08-26"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(no_buckets["replyBuckets"], json!([]));
    // 桶边界非法一律 400，不静默跳过 —— 少一根柱子的图没有任何东西会提示它错了。
    for path in [
        "/api/summary?buckets=60,abc",
        "/api/summary?buckets=300,60",
        "/api/summary?buckets=60,60",
    ] {
        assert_eq!(
            http.get(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            400,
            "{path}"
        );
    }
    // 比率的分母为零时给 null 不给 0 —— 空窗口不能读成「零超时率」。
    let empty: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/summary?from=2026-08-27&to=2026-08-27"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(empty["events"], 0);
    assert!(empty["overdueRate"].is_null() && empty["p50"].is_null());

    // ── 按群 / 按客服的聚合行：与 `aggregate()` 同口径 ────────────────────
    let rooms: Value = get_json(
        &http,
        format!("{base}/api/rooms?from=2026-08-25&to=2026-08-26&sla_sec=300"),
    )
    .await;
    assert_eq!(rooms.as_array().unwrap().len(), 1, "只有 R 这一个群有事件");
    assert_eq!(rooms[0]["roomid"], "R");
    assert_eq!(rooms[0]["events"], 2);
    assert_eq!(rooms[0]["merchant"], 2);
    assert_eq!(rooms[0]["p50"], 600);
    assert_eq!(
        rooms[0]["overdue"], 1,
        "e2 的 600 秒 > sla 300，e1 的 0 秒不算"
    );
    // 群 × 日事件数 —— 热力图要它。**没有事件的格子不出行**：那个格子是「真的 0」
    // 还是「抽取失败」只有群日表答得了，在这里补零会把失败伪装成 0。
    assert_eq!(
        rooms[0]["series"],
        json!([
            {"day": "2026-08-25", "events": 1},
            {"day": "2026-08-26", "events": 1},
        ])
    );
    // 主要事件类型：**在数据库里截前四**，行数恒为「群数 × 4」而不是实际组合数。
    assert_eq!(
        rooms[0]["topGroups"],
        json!([{"key": "reschedule", "count": 2}])
    );

    let agents: Value = serde_json::from_str(
        &http
            .get(format!(
                "{base}/api/agents?from=2026-08-25&to=2026-08-26&sla_sec=300"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(agents.as_array().unwrap().len(), 1);
    assert_eq!(agents[0]["agent"], "agent00000000001");
    // 参与 = agents 数组里有他；归属 = first_responder 是他。**两个口径不能混。**
    assert_eq!(agents[0]["involved"], 2);
    assert_eq!(agents[0]["owned"], 2);
    assert_eq!(agents[0]["merchantOwned"], 2);
    assert_eq!(agents[0]["replySamples"], 2);
    assert_eq!(agents[0]["p50"], 600);
    // ⚠️ **这一路没有 `unreplied`，是删掉的不是漏掉的。** 它此前在「参与过」那一侧
    // 算未回复的商家事件，而抽取保证「无平台回复 ⟹ `agents` 为空」—— 未回复的事件
    // 一个参与者都没有，那一列**结构上恒为 0**，看起来却像「这个人没有欠回复的」。
    // 团队口径的无响应数在 `/api/summary` 的 `unreplied`，那条是对的。
    assert!(agents[0].get("unreplied").is_none());
    assert_eq!(
        agents[0]["series"],
        json!([
            {"day": "2026-08-25", "involved": 1, "owned": 1},
            {"day": "2026-08-26", "involved": 1, "owned": 1},
        ])
    );
    // 参与过的群 —— 抽屉的分群明细与「数据完整性」那一列要它。
    // 走独立查询而不是 `GROUP_CONCAT`：后者有 1KB 的**静默截断**。
    assert_eq!(agents[0]["roomIds"], json!(["R"]));

    // ── 分类汇总：后端不认识词表，只按调用方给的分组键数数 ──────────────────
    let by_type: Value = serde_json::from_str(
        &http
            .get(format!(
                "{base}/api/categories?from=2026-08-25&to=2026-08-26&sla_sec=300"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        by_type,
        json!([{"key": "reschedule", "count": 2, "merchant": 2, "unreplied": 0,
                "unrepliedRate": 0.0, "p50": 600, "p90": 600,
                "series": [{"day": "2026-08-25", "events": 1},
                           {"day": "2026-08-26", "events": 1}]}]),
        "不给 groups 就按 event_type 分组，key 即 type_id"
    );
    // 一级：前端把词表的父类分组传上来，`key` 是**组下标**，后端不 join 词表。
    // 分位数必须在这里按父类现算 —— 分位数不可加，前端合不出来。
    let by_parent: Value = serde_json::from_str(
        &http
            .get(format!(
                "{base}/api/categories?from=2026-08-25&to=2026-08-26&groups=reschedule"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(by_parent[0]["key"], "0");
    assert_eq!(by_parent[0]["count"], 2);
    assert_eq!(by_parent[0]["p50"], 600);
    // 空组会让下标与前端的父类名错位，直接 400。
    assert_eq!(
        http.get(format!("{base}/api/categories?groups=reschedule,"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );

    // ── 明细翻页：延迟关联 + 页码护栏 ──────────────────────────────────────
    // 翻过头返回空页，不报错 —— 那是「这一页确实没有」，不是参数错。
    let beyond: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/events?page=9&page_size=10"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    // 空页仍然带着正确的总数与页数 —— 前端据此把人送回有数据的那一页。
    assert!(beyond["rows"].as_array().unwrap().is_empty());
    assert_eq!(beyond["total"], 2);
    assert_eq!(beyond["pages"], 1);
    // ── 排序：只开放 `idx_overview` 里有的列，NULL 一律排最后 ─────────────
    // e1 未超时（工作秒数 0），e2 600 秒。按首响耗时降序 e2 在前。
    let sorted: Value = serde_json::from_str(
        &http
            .get(format!(
                "{base}/api/events?from=2026-08-25&to=2026-08-26&sort=wait&dir=desc"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(sorted["rows"][0]["summary"], "商家问上门时间，平台已答复");
    assert_eq!(sorted["rows"][1]["summary"], "商家要求改期，平台已受理");
    // 不在 `idx_overview` 里的列不给排 —— 放行的话内层分页会退化成整窗口回表，
    // 而页面上只是多了个能点的表头。显式 400，不静默退回默认排序。
    for path in [
        "/api/events?sort=summary",
        "/api/events?sort=followup_wait_max_sec",
        "/api/events?sort=time&dir=sideways",
    ] {
        assert_eq!(
            http.get(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            400,
            "{path}"
        );
    }
    // 越界的页码 / 页大小是 400，**不夹到边界上** —— 夹住的话用户会以为看到了全部。
    for path in [
        "/api/events?page=0",
        "/api/events?page=201",
        "/api/events?page_size=0",
        "/api/events?page_size=101",
    ] {
        assert_eq!(
            http.get(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            400,
            "{path}"
        );
    }

    // 可选导出实际 HTTP 响应，让前端 zod 对拍同一份跨语言契约。
    if let Ok(dir) = std::env::var("CHAT2EVENTS_WEB_FIXTURE_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join("dataset.json"),
            data.to_string(),
        )
        .unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join("events.json"),
            page.to_string(),
        )
        .unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join("messages.json"),
            messages.to_string(),
        )
        .unwrap();
    }
    for (path, status) in [
        ("/api/dataset?from=2026-08-27&to=2026-08-25", 400),
        ("/api/dataset?from=broken", 400),
        ("/api/event/999999/messages", 404),
        ("/api/event/999999", 404),
    ] {
        assert_eq!(
            http.get(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            status
        );
    }
    assert_eq!(
        http.post(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    let narrow: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset?from=2026-08-26&to=2026-08-26"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(narrow["groupDaily"].as_array().unwrap().len(), 1);

    sqlx::query("INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,event_count,merchant_event_count,unreplied_count,extraction_status) VALUES ('C','R','2026-08-01',0,0,0,0,0,'ok')").execute(&pool).await.unwrap();
    let recent: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        recent["meta"]["days"][0], "2026-08-01",
        "可用历史范围保持完整"
    );
    assert_eq!(
        recent["groupDaily"].as_array().unwrap().len(),
        2,
        "默认只加载最近七天的记录"
    );
    let history: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset?from=2026-08-01&to=2026-08-26"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        history["groupDaily"].as_array().unwrap().len(),
        3,
        "用户仍可显式读取全部历史"
    );

    // ⚠️ **筛选器名单按 `window_since/window_until` 收敛，不看 `run_date`。**
    // `run_date` 是**跑批日**、窗口是**数据日**，T+2 之下两者差两天 —— 拿跑批日去套
    // 数据窗口是两根不同的时间轴，那一支永远匹配不到，而它要捞的正是下面这几个
    // 「只有失败、没有群日」的群（整群被跳过）。所以这里的 `run_date` 全部**故意**
    // 落在任何被查询的窗口之外：它要是还被用着，这条测试就红。
    //
    // `gmt_created_time` 仍然留在 08-30，新鲜度看的是它和 `fact_completed_time` 的
    // 先后，跟这两列都无关。
    sqlx::query(
        "INSERT INTO b_merchant_group_run_failure \
         (run_date,corpid,roomid,reason,window_since,window_until,gmt_created_time) VALUES \
         ('2026-08-28','C','R','合成同步失败','2026-08-25','2026-08-26','2026-08-30 11:00:00'), \
         ('2026-08-28','C','missing-room','合成读取失败','2026-08-25','2026-08-26','2026-08-30 11:00:00'), \
         ('2026-08-28','C','empty-name','合成读取失败','2026-08-25','2026-08-26','2026-08-30 11:00:00'), \
         ('2026-08-04','C','frozen-room','冻结区的老失败','2026-08-01','2026-08-02','2026-08-04 11:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let uncertain: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(uncertain["groupDaily"][0]["freshness"], "unknown");
    // 标签状态更新不能替一次失败的抽取证明事实新鲜，必须穿过真实写入与读取链路。
    let window = crate::window::Window::span(at(25, 0, 0).date(), at(26, 0, 0).date());
    let shard = crate::stage::store::Shard::new("C", "R", &window);
    crate::stage::store::fail_classification(&pool, at(30, 0, 0).date(), shard, "测试分类失败")
        .await
        .unwrap();
    crate::stage::store::finish_classification(&pool, shard, &[])
        .await
        .unwrap();
    let after_retag: Value = http
        .get(format!("{base}/api/dataset"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_retag["groupDaily"][0]["freshness"], "unknown",
        "标签更新不能推进事实完成凭据"
    );
    assert_eq!(
        uncertain["groupDaily"][0]["event_count"], 1,
        "保留旧值，但必须标记未知"
    );
    let room_ids = |v: &Value| -> Vec<String> {
        v["meta"]["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["roomid"].as_str().unwrap().to_owned())
            .collect()
    };
    for roomid in ["missing-room", "empty-name"] {
        let room = uncertain["meta"]["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["roomid"] == roomid)
            .unwrap();
        assert_eq!(
            room,
            &json!({"roomid": roomid, "alias": null,
            "merchant_id": null, "alias_is_authoritative": false})
        );
    }
    // **失败只进它自己那个数据窗口的名单。** 冻结区那次失败（08-01~08-02）不该出现在
    // 默认窗口（最近七天）的筛选器里；显式查到 08-01 才出现。少了这一对断言，
    // 拿 `run_date` 去夹窗口那种写法照样能绿。
    assert!(
        !room_ids(&uncertain).contains(&"frozen-room".to_owned()),
        "窗口外的失败不该进筛选器名单"
    );
    let wide: Value = serde_json::from_str(
        &http
            .get(format!("{base}/api/dataset?from=2026-08-01&to=2026-08-26"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        room_ids(&wide).contains(&"frozen-room".to_owned()),
        "窗口覆盖到那次失败时，整群被跳过的群必须出现在筛选器名单里"
    );
    // 410 现在只有一种含义：**这一行在加 source_messages 之前就抽取过了**，
    // 原文确实永久取不到。此前它还兼任「raw 镜像没同步 / raw_root 配错」，
    // 于是一个可修的运维故障长得跟正常的数据过期一模一样。
    sqlx::query("UPDATE b_merchant_group_event SET source_messages = NULL WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        http.get(format!("{base}/api/event/{id}/messages"))
            .send()
            .await
            .unwrap()
            .status(),
        410
    );
    for status in ["pending", "failed", "ok"] {
        sqlx::query(
            "UPDATE b_merchant_group_metric_daily SET classification_status=? WHERE roomid='R'",
        )
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
        let response = http
            .get(format!("{base}/api/dataset?from=2026-08-25&to=2026-08-26"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "打标未完成也能读取事实");
        let data: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(data["groupDaily"][0]["classification_status"], status);
        let page: Value = serde_json::from_str(
            &http
                .get(format!("{base}/api/events?from=2026-08-25&to=2026-08-26"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(page["rows"].as_array().unwrap().len(), 2);
        assert_eq!(page["rows"][0]["event_type"].is_null(), status != "ok");
        let response = http
            .get(format!("{base}/api/event/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let detail: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(detail["taxonomy_version"].is_null(), status != "ok");
    }
    sqlx::query("UPDATE b_merchant_group_event SET taxonomy_version='v0'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        http.get(format!("{base}/api/dataset"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let (_, facts) = crate::stage::store::read_events(&pool, shard)
        .await
        .unwrap();
    let counts =
        std::collections::BTreeMap::from([(window.since(), (1, 1)), (window.until(), (1, 1))]);
    let group = crate::stage::metrics::group_rows(
        "C",
        "R",
        &window,
        &counts,
        Some(&facts),
        crate::stage::metrics::Status::Ok,
    );
    crate::stage::store::write_room(
        &pool,
        at(30, 0, 0).date(),
        shard,
        Some(&facts),
        None,
        &group,
        &[],
        &std::collections::BTreeMap::new(),
    )
    .await
    .unwrap();
    let refreshed: Value = http
        .get(format!("{base}/api/dataset"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        refreshed["groupDaily"][0]["freshness"], "known",
        "只有真实保存事实才能恢复新鲜度"
    );
    assert!(
        refreshed["events"][0]["event_type"].is_null(),
        "事实新鲜不意味着分类已完成"
    );
    // 响应缓存：上面每一次读都紧跟着一次写（戳太新），所以都是 BYPASS；
    // 把最后一次写推到 60 秒之前，第一次 MISS 装入、第二次 HIT 且字节逐位相同；
    // 再写一笔（模拟跑批），戳一变整个缓存作废、回到 BYPASS。
    //
    // ⚠️ **每次写之后要等过数据戳的自缓存**（`cache::STAMP_TTL` = 1 秒）：戳在那一秒里
    // 是复用的，刚写进去的东西看不见。那正是那个自缓存**有意**的影响面
    // （「跑批结束后的第一秒最坏多命中一次旧缓存」），不是这里要测的东西。
    let settle =
        || tokio::time::sleep(super::cache::STAMP_TTL + std::time::Duration::from_millis(100));
    let header = |r: &reqwest::Response| r.headers()["x-cache"].to_str().unwrap().to_owned();
    let url = format!("{base}/api/summary?from=2026-08-25&to=2026-08-26");
    assert_eq!(header(&http.get(&url).send().await.unwrap()), "BYPASS");
    sqlx::raw_sql(
        "UPDATE b_merchant_group_event SET gmt_modified_time = NOW() - INTERVAL 1 HOUR; \
         UPDATE b_merchant_group_metric_daily SET gmt_modified_time = NOW() - INTERVAL 1 HOUR; \
         UPDATE b_merchant_group_taxonomy SET gmt_modified_time = NOW() - INTERVAL 1 HOUR;",
    )
    .execute(&pool)
    .await
    .unwrap();
    settle().await;
    let miss = http.get(&url).send().await.unwrap();
    assert_eq!(header(&miss), "MISS");
    let miss = miss.bytes().await.unwrap();
    let hit = http.get(&url).send().await.unwrap();
    assert_eq!(header(&hit), "HIT");
    assert_eq!(hit.headers()["content-type"], "application/json");
    assert_eq!(hit.bytes().await.unwrap(), miss);
    // 键是完整 URI：另一组筛选不会串
    assert_eq!(
        header(&http.get(format!("{url}&sla_sec=60")).send().await.unwrap()),
        "MISS"
    );
    sqlx::query("INSERT INTO b_merchant_group_run_failure (run_date, corpid, roomid, reason) VALUES ('2026-08-31','C','R','夜里跑批')")
        .execute(&pool).await.unwrap();
    settle().await;
    // 失败表只增不改，戳里是 MAX(id) —— 它也让缓存作废；距最后一次 gmt 写仍超过 60 秒，所以是 MISS 不是 BYPASS
    assert_eq!(header(&http.get(&url).send().await.unwrap()), "MISS");
    shutdown.send(()).unwrap();
    server.await.unwrap();
    testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_read_snapshot_is_read_only_and_stable_across_tables() {
    let pool = testutil::mysql_pool("snapshot").await;
    let mut connection = pool.acquire().await.unwrap();
    let mut tx = snapshot(&mut connection).await.unwrap();
    let (before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_run_failure")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO b_merchant_group_run_failure (run_date, corpid, roomid, reason) VALUES ('2026-08-30','C','R','test')").execute(&pool).await.unwrap();
    let (after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM b_merchant_group_run_failure")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(before, after, "同一快照不应读到后续提交");
    assert!(
        sqlx::query("DELETE FROM b_merchant_group_run_failure")
            .execute(&mut *tx)
            .await
            .is_err(),
        "只读事务必须由数据库阻止写入"
    );
    tx.commit().await.unwrap();
    drop(connection);
    testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_filtered_reads_use_date_and_failure_indexes() {
    use sqlx::Row;
    let pool = testutil::mysql_pool("read_indexes").await;
    sqlx::raw_sql("INSERT INTO b_merchant_group_event (corpid,roomid,source_msg_ids,first_msg_time,last_msg_time,occurred_on,asker,asker_role,agents,summary) \
        WITH RECURSIVE seq(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM seq WHERE n<999) \
        SELECT CONCAT('C',MOD(n,10)),CONCAT('R',MOD(n,100)),JSON_ARRAY(CONCAT('m',n)),'2026-01-01','2026-01-01', \
        DATE_ADD('2026-01-01',INTERVAL (n DIV 10) DAY),'merchant00000001','EXTERNAL',JSON_ARRAY('agent00000000001'),'合成事件' FROM seq; \
        INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,event_count,extraction_status,fact_completed_time) \
        SELECT corpid,roomid,occurred_on,1,1,1,'ok','2026-08-30' FROM b_merchant_group_event; \
        INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason,stage) SELECT '2026-08-31',corpid,roomid,'合成失败','extract' FROM b_merchant_group_event; \
        ANALYZE TABLE b_merchant_group_event, b_merchant_group_metric_daily, b_merchant_group_run_failure;")
        .execute(&pool).await.unwrap();
    for (sql, expected) in [
        (
            // idx_overview 取代了 idx_corp_day（它是前缀）。`summary` 不在索引里，
            // 所以这条仍要回表 —— 钉的是「定位走索引、不扫全表」，不是「不回表」。
            "SELECT summary FROM b_merchant_group_event WHERE corpid='C1' AND occurred_on='2026-01-05'",
            "idx_overview",
        ),
        (
            "SELECT msg_count FROM b_merchant_group_metric_daily WHERE corpid='C1' AND dt='2026-01-05'",
            "idx_corp_day",
        ),
        (
            "SELECT gmt_created_time FROM b_merchant_group_run_failure WHERE corpid='C1' AND roomid='R1' AND stage='extract' ORDER BY gmt_created_time DESC LIMIT 1",
            "idx_room_stage_time",
        ),
        // 缓存的数据戳：MAX 走索引尾读，一次一行 —— 没有 idx_modified 它就是每个请求全表扫一遍
        (
            "SELECT gmt_modified_time FROM b_merchant_group_event ORDER BY gmt_modified_time DESC LIMIT 1",
            "idx_modified",
        ),
        (
            "SELECT gmt_modified_time FROM b_merchant_group_metric_daily ORDER BY gmt_modified_time DESC LIMIT 1",
            "idx_modified",
        ),
    ] {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
            .fetch_one(&pool)
            .await
            .unwrap();
        let key: Option<String> = row.try_get("key").unwrap();
        assert_eq!(key.as_deref(), Some(expected));
        let rows: u64 = row.try_get("rows").unwrap();
        assert!(rows <= 10, "索引不应扫描整份1000行历史：{rows}");
        eprintln!("{expected}: estimated_rows={rows}");
    }
    for (name, query) in [
        (
            "old_meta_dates",
            "SELECT MIN(dt), MAX(dt) FROM (SELECT dt FROM b_merchant_group_metric_daily WHERE corpid='C1' UNION ALL SELECT occurred_on FROM b_merchant_group_event WHERE corpid='C1') dates",
        ),
        (
            "new_meta_dates",
            "SELECT MIN(since), MAX(until) FROM (SELECT MIN(dt) since,MAX(dt) until FROM b_merchant_group_metric_daily WHERE corpid='C1' UNION ALL SELECT MIN(occurred_on),MAX(occurred_on) FROM b_merchant_group_event WHERE corpid='C1') dates",
        ),
    ] {
        let plan: String =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN ANALYZE {query}")))
                .fetch_one(&pool)
                .await
                .unwrap();
        eprintln!("{name}: {plan}");
    }
    testutil::drop_mysql_database(pool).await;
}

#[tokio::test]
async fn admission_rejects_excess_work_and_releases_timed_out_slots() {
    let state = WebState {
        pool: sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://localhost/test")
            .unwrap(),
        corp: "C".into(),
        limits: WebLimits {
            concurrency: 1,
            max_response_bytes: 1000,
            max_rows: 10,
            query_timeout_secs: 1,
            cache_bytes: 0,
        },
        requests: Arc::new(Semaphore::new(1)),
        cache: Arc::new(super::cache::Cache::new(0)),
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let ready = entered.clone();
    let app = Router::new()
        .route(
            "/slow",
            get(move || {
                let ready = ready.clone();
                async move {
                    ready.notify_one();
                    std::future::pending::<&'static str>().await
                }
            }),
        )
        .layer(middleware::from_fn_with_state(state.clone(), admit));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/slow", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let http = reqwest::Client::new();
    let first = tokio::spawn(http.get(&url).send());
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert_eq!(
        http.get(&url).send().await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        first.await.unwrap().unwrap().status(),
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(state.requests.available_permits(), 1);
    // 下钻改成一次主键查找之后**没有第二道扫描名额了** —— `scan_concurrency`
    // 和 `WebState.scans` 一起删掉，原来那条 503 断言随之消失。
    server.abort();
    let _ = server.await;
}

/// 一次失败**只毒化它自己那个窗口**，不毒化这个群的全部历史。
///
/// ⚠️ 这条钉的是一个曾经的永久性数据损坏：`OK_DAYS` 的子查询原来不带日期条件，
/// 于是今天一次「拉取失败 / 整轮预算用完没轮到」会把这个群**所有**群日标成
/// `unknown` —— 含冻结区里早已成功的天。而 `OK_DAYS` 是四个聚合接口的**分母边界**，
/// 那些天的事件会被整个排除出统计；冻结区又不会再被重抽，所以那个 unknown 回不来。
/// 失败形状正是承重不变量 5 点名的那种：数字偏小，但看起来完全正常。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_a_failure_only_clouds_its_own_window() {
    let pool = testutil::mysql_pool("failure_window").await;
    sqlx::raw_sql(
        "INSERT INTO b_merchant_group_metric_daily \
         (corpid,roomid,dt,msg_count,sender_count,event_count,merchant_event_count,unreplied_count,extraction_status,fact_completed_time) VALUES \
         ('C','R','2026-08-01',1,1,1,1,0,'ok','2026-08-04 10:00:00'), \
         ('C','R','2026-08-20',1,1,1,1,0,'ok','2026-08-23 10:00:00'); \
         INSERT INTO b_merchant_group_event \
         (corpid,roomid,source_msg_ids,first_msg_time,last_msg_time,occurred_on,asker,asker_role,agents,summary) VALUES \
         ('C','R',JSON_ARRAY('m1'),'2026-08-01 09:00:00','2026-08-01 09:00:00','2026-08-01','merchant00000001','EXTERNAL',JSON_ARRAY(),'冻结区的老事件'), \
         ('C','R',JSON_ARRAY('m2'),'2026-08-20 09:00:00','2026-08-20 09:00:00','2026-08-20','merchant00000001','EXTERNAL',JSON_ARRAY(),'较近的事件');"
    ).execute(&pool).await.unwrap();

    // 今天（09-09 这一轮）在 08-20 这个窗口上失败了。冻结区的 08-01 与它无关。
    crate::stage::store::record_failure(
        &pool,
        chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap(),
        crate::stage::store::Shard::new(
            "C",
            "R",
            &crate::window::Window::span(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
            ),
        ),
        "extract",
        "合成失败",
    )
    .await
    .unwrap();

    let ok_days: Vec<(String, chrono::NaiveDate)> = sqlx::query_as(sqlx::AssertSqlSafe(
        super::query::ok_days_sql_for_test().to_owned(),
    ))
    .bind("C")
    .bind(chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap())
    .bind(chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        ok_days,
        vec![(
            "R".to_owned(),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()
        )],
        "只有 08-20 该被这次失败挡住；08-01 在冻结区、与本次窗口无关，必须仍然可用"
    );

    // 加这两列之前的历史失败行（窗口为 NULL）仍按「影响全历史」保守处理。
    sqlx::query("INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason,stage) VALUES ('2026-09-09','C','R','升级前的历史失败','extract')")
        .execute(&pool).await.unwrap();
    let ok_days: Vec<(String, chrono::NaiveDate)> = sqlx::query_as(sqlx::AssertSqlSafe(
        super::query::ok_days_sql_for_test().to_owned(),
    ))
    .bind("C")
    .bind(chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap())
    .bind(chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        ok_days.is_empty(),
        "窗口未知的历史失败行不能被当成「只影响某几天」"
    );

    testutil::drop_mysql_database(pool).await;
}

/// **已知成功群日的改写等价于上一版** —— 这条测试是那次改写唯一可运行的证据。
///
/// 上一版把同一条相关子查询逐字写了两遍（一次判空、一次比较），MySQL 不保证对相关
/// 子查询做公共子表达式消除，于是每个候选群日行都要做两次失败记录的索引查找。
/// 新版改成「先左连接、再按群日取最大失败时间」——**不是机械替换**：一个群日可能
/// 匹配多条窗口不同的失败记录，连接出来的每一行各比各的，只要有一条失败早于凭据
/// 就会放行（R6 与 R5 就是这一对反例）。所以必须先 `MAX` 再比。
///
/// 六种群日各压一类：从没失败过 · 失败早于凭据 · 失败晚于凭据 · 失败窗口为空的历史行 ·
/// 一个群日匹配两条失败（最晚的那条晚于凭据）· 一个群日匹配两条失败（晚的那条不覆盖它）。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_known_ok_days_rewrite_matches_the_previous_definition() {
    let pool = testutil::mysql_pool("known_ok_days").await;
    sqlx::raw_sql(
        "INSERT INTO b_merchant_group_metric_daily \
         (corpid,roomid,dt,msg_count,sender_count,event_count,merchant_event_count,unreplied_count,extraction_status,fact_completed_time) VALUES \
         ('C','R1','2026-08-01',1,1,1,1,0,'ok','2026-08-04 10:00:00'), \
         ('C','R2','2026-08-05',1,1,1,1,0,'ok','2026-08-08 10:00:00'), \
         ('C','R3','2026-08-10',1,1,1,1,0,'ok','2026-08-12 10:00:00'), \
         ('C','R4','2026-08-20',1,1,1,1,0,'ok','2026-08-22 10:00:00'), \
         ('C','R5','2026-08-25',1,1,1,1,0,'ok','2026-08-27 10:00:00'), \
         ('C','R6','2026-08-28',1,1,1,1,0,'ok','2026-08-30 10:00:00'), \
         ('C','R7','2026-08-02',1,1,1,1,0,'failed',NULL), \
         ('C','R8','2026-08-03',1,1,1,1,0,'ok',NULL); \
         INSERT INTO b_merchant_group_run_failure \
         (run_date,corpid,roomid,reason,stage,window_since,window_until,gmt_created_time) VALUES \
         ('2026-08-06','C','R2','早于凭据','extract','2026-08-05','2026-08-05','2026-08-06 10:00:00'), \
         ('2026-08-15','C','R3','晚于凭据','extract','2026-08-10','2026-08-10','2026-08-15 10:00:00'), \
         ('2026-08-26','C','R5','早于凭据','extract','2026-08-25','2026-08-25','2026-08-26 10:00:00'), \
         ('2026-08-28','C','R5','晚于凭据，同样覆盖这一天','extract','2026-08-25','2026-08-25','2026-08-28 10:00:00'), \
         ('2026-08-29','C','R6','早于凭据','extract','2026-08-28','2026-08-28','2026-08-29 10:00:00'), \
         ('2026-09-01','C','R6','晚于凭据，但不覆盖这一天','extract','2026-08-01','2026-08-02','2026-09-01 10:00:00'); \
         INSERT INTO b_merchant_group_run_failure (run_date,corpid,roomid,reason,stage) VALUES \
         ('2026-09-09','C','R4','升级前的历史失败，窗口未知','extract');"
    ).execute(&pool).await.unwrap();

    let (since, until) = (
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
    );
    let run = async |sql: &'static str| -> Vec<(String, chrono::NaiveDate)> {
        let mut rows: Vec<(String, chrono::NaiveDate)> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql.to_owned()))
                .bind("C")
                .bind(since)
                .bind(until)
                .fetch_all(&pool)
                .await
                .unwrap();
        rows.sort();
        rows
    };
    let fresh = run(super::query::ok_days_sql_for_test()).await;
    let legacy = run(super::query::LEGACY_KNOWN_OK_DAYS).await;
    assert_eq!(fresh, legacy, "改写必须与上一版选出完全相同的群日");
    // 也钉住集合本身 —— 两版一起错的话「相等」证明不了什么。
    assert_eq!(
        fresh,
        vec![
            (
                "R1".to_owned(),
                chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()
            ),
            (
                "R2".to_owned(),
                chrono::NaiveDate::from_ymd_opt(2026, 8, 5).unwrap()
            ),
            (
                "R6".to_owned(),
                chrono::NaiveDate::from_ymd_opt(2026, 8, 28).unwrap()
            ),
        ],
        "R3/R5 失败晚于凭据 · R4 失败窗口未知 · R7 抽取失败 · R8 无事实凭据，都该掉出分母"
    );

    testutil::drop_mysql_database(pool).await;
}

/// 口径对拍 —— **同一批输入，SQL 那份和 TypeScript 那份必须算出同一组数字。**
///
/// ⚠️ `query.rs` 里写着「落地时必须有一条测试拿同一批数据对拍两边」，这就是那条。
/// 两边共用 `webui/src/domain/parity-vectors.json`：这里跑真 SQL，
/// `webui/src/api/mock/parity.test.ts` 跑 `mockSummary`，各自断言等于同一组 `expected`。
///
/// 口径分家是**静默**的 —— 页面照样显示一个看起来合理的数字，没有人会发现。
/// 所以这条测试钉的不是「结果对不对」，是「两个实现有没有开始分家」。
#[tokio::test]
#[ignore = "需要隔离 MySQL，显式设置 CHAT2EVENTS_TEST_DATABASE_URL"]
async fn mysql_summary_matches_the_frontend_definitions() {
    use serde_json::Value;
    let vectors: Value =
        serde_json::from_str(include_str!("../../webui/src/domain/parity-vectors.json")).unwrap();
    let pool = testutil::mysql_pool("parity").await;

    for cell in vectors["groupDaily"].as_array().unwrap() {
        sqlx::query(
            "INSERT INTO b_merchant_group_metric_daily (corpid,roomid,dt,msg_count,sender_count,\
             event_count,merchant_event_count,unreplied_count,first_reply_p50_sec,first_reply_p90_sec,\
             extraction_status,classification_status,fact_completed_time) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,'2026-09-01 10:00:00')",
        )
        .bind(cell["corpid"].as_str())
        .bind(cell["roomid"].as_str())
        .bind(cell["dt"].as_str())
        .bind(cell["msg_count"].as_i64())
        .bind(cell["sender_count"].as_i64())
        .bind(cell["event_count"].as_i64())
        .bind(cell["merchant_event_count"].as_i64())
        .bind(cell["unreplied_count"].as_i64())
        .bind(cell["first_reply_p50_sec"].as_i64())
        .bind(cell["first_reply_p90_sec"].as_i64())
        .bind(cell["extraction_status"].as_str())
        .bind(cell["classification_status"].as_str())
        .execute(&pool)
        .await
        .unwrap();
    }
    for e in vectors["events"].as_array().unwrap() {
        sqlx::query(
            "INSERT INTO b_merchant_group_event (corpid,roomid,source_msg_ids,first_msg_time,\
             last_msg_time,first_agent_reply_time,occurred_on,asker,asker_role,agents,first_responder,\
             summary,last_msg_role,followup_wait_max_sec,event_type,taxonomy_version) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(e["corpid"].as_str())
        .bind(e["roomid"].as_str())
        .bind(e["source_msg_ids"].to_string())
        .bind(e["first_msg_time"].as_str())
        .bind(e["last_msg_time"].as_str())
        .bind(e["first_agent_reply_time"].as_str())
        .bind(e["occurred_on"].as_str())
        .bind(e["asker"].as_str())
        .bind(e["asker_role"].as_str())
        .bind(e["agents"].to_string())
        .bind(e["first_responder"].as_str())
        .bind(e["summary"].as_str())
        .bind(e["last_msg_role"].as_str())
        .bind(e["followup_wait_max_sec"].as_i64())
        .bind(e["event_type"].as_str())
        .bind(e["taxonomy_version"].as_str())
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut connection = pool.acquire().await.unwrap();
    let buckets: Vec<u32> = vectors["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_u64().unwrap() as u32)
        .collect();
    let actual = super::query::read_summary(
        &mut connection,
        vectors["corpid"].as_str().unwrap(),
        vectors["window"]["from"].as_str().unwrap().parse().unwrap(),
        vectors["window"]["to"].as_str().unwrap().parse().unwrap(),
        vectors["slaSec"].as_u64().unwrap() as u32,
        &Default::default(),
        &buckets,
    )
    .await
    .unwrap();

    assert_eq!(
        actual, vectors["expected"],
        "SQL 那份和金标不一致 —— 口径改了就要同时改前端那份并说明为什么"
    );
    // ⚠️ **先还连接再删库。** `drop_mysql_database` 会 `pool.close()`，那要等所有
    // 连接归还；`acquire()` 拿出来的这条还在作用域里，于是它等一条永远不还的连接，
    // 整个测试**静默挂死**（MySQL 侧看不到任何活动查询，栈停在 tokio 的 park）。
    drop(connection);
    testutil::drop_mysql_database(pool).await;
}
