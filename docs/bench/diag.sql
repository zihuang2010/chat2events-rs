-- 只读诊断：工作台概览页重算慢（2026-10-15 09:15 / 10:01 日志）。docs/deploy.md「实测基线」生产那一节的出处。
-- ⚠️ 第 4 段四条语句是那天慢日志里的**原文快照**，代码里的 SQL 改了它不会跟着变 ——
--    要量当前版本，从 webui 日志的 `sqlx::query: slow statement` 里重新取。
-- 窗口改下面的 @since/@until：默认是生产 2026-10-07..13 的 7 天。本机种子（docs/bench/seed.sql）
--    同样以 10-13 收尾，量满 31 天把 @since 改成 2026-09-13。
-- 全部是 SELECT / EXPLAIN ANALYZE，不写任何表。EXPLAIN ANALYZE 会真的执行一遍（只读），每条约 1~3 秒。
-- 挑没人用工作台的时候跑；整份连跑两遍，两份输出都贴回来（第一遍偏冷缓冲池，第二遍偏热，用来区分 IO 和 CPU）。
--
--   mysql --raw -h ... -u <只读账号> -p <库名> < docs/bench/diag.sql > diag-1.out 2>&1
--   mysql --raw -h ... -u <只读账号> -p <库名> < docs/bench/diag.sql > diag-2.out 2>&1
--
-- --raw 让 EXPLAIN ANALYZE 的树形输出保留换行。MySQL < 8.0.18 没有 EXPLAIN ANALYZE，
-- 那一段会报语法错误 —— 把 ANALYZE 换成 FORMAT=TREE 再跑一遍即可（只给计划不给耗时）。

SET @corp  := (SELECT corpid FROM b_merchant_group_metric_daily LIMIT 1);
SET @since := DATE '2026-10-07';
SET @until := DATE '2026-10-13';
SET @sla   := 1800;

SELECT '== 0. 环境' AS section;
SELECT VERSION() AS version, @@innodb_buffer_pool_size DIV 1048576 AS buffer_pool_mb,
       DATABASE() AS db, @corp AS corp, NOW() AS ran_at;

SELECT '== 1. 规模' AS section;
SELECT TABLE_NAME, TABLE_ROWS, DATA_LENGTH DIV 1048576 AS data_mb,
       INDEX_LENGTH DIV 1048576 AS index_mb, AVG_ROW_LENGTH
FROM information_schema.TABLES
WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME LIKE 'b\_merchant\_group\_%';

-- 窗口内事件数，以及展开 agents 之后的行数（客服那几条中间结果的大小）。这一条本身要回表读整个窗口。
SELECT COUNT(*) AS window_events, COUNT(DISTINCT roomid) AS rooms,
       SUM(JSON_LENGTH(agents)) AS agent_rows
FROM b_merchant_group_event
WHERE corpid = @corp AND occurred_on BETWEEN @since AND @until;

SELECT COUNT(*) AS window_group_days, COUNT(DISTINCT roomid) AS rooms,
       SUM(extraction_status = 'ok' AND fact_completed_time IS NOT NULL) AS ok_with_credential
FROM b_merchant_group_metric_daily
WHERE corpid = @corp AND dt BETWEEN @since AND @until;

SELECT stage, COUNT(*) AS n, SUM(window_since IS NULL) AS legacy_rows
FROM b_merchant_group_run_failure WHERE corpid = @corp GROUP BY stage;

SELECT '== 2. 生产库索引是否与 schema.sql 一致' AS section;
SELECT TABLE_NAME, INDEX_NAME, GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX) AS cols
FROM information_schema.STATISTICS
WHERE TABLE_SCHEMA = DATABASE()
  AND TABLE_NAME IN ('b_merchant_group_event', 'b_merchant_group_metric_daily', 'b_merchant_group_run_failure')
GROUP BY TABLE_NAME, INDEX_NAME;

SELECT '== 3. 商家摘要表最近按小时被改了几行（merchant_sync 白天会不会让缓存整体作废）' AS section;
SELECT DATE_FORMAT(gmt_modified_time, '%Y-%m-%d %H:00') AS hour, COUNT(*) AS n
FROM b_merchant_group_merchant_summary GROUP BY hour ORDER BY hour DESC LIMIT 20;

SELECT '== 4.1 筛选器客服名单（日志 1.54s）' AS section;
EXPLAIN ANALYZE
SELECT DISTINCT a.agent FROM b_merchant_group_event e
JOIN JSON_TABLE(e.agents, '$[*]' COLUMNS(agent VARCHAR(16) PATH '$')) a
WHERE e.corpid = @corp AND e.occurred_on BETWEEN @since AND @until ORDER BY a.agent LIMIT 20001;

SELECT '== 4.2 按群聚合（日志 1.22s）' AS section;
EXPLAIN ANALYZE
WITH ev AS (SELECT e.roomid, e.asker_role, e.first_agent_reply_time, e.occurred_on, GREATEST(0, (TO_DAYS(e.first_agent_reply_time) - TO_DAYS(e.first_msg_time)) * 45000 + LEAST(GREATEST(TIME_TO_SEC(e.first_agent_reply_time), 30600), 75600) - LEAST(GREATEST(TIME_TO_SEC(e.first_msg_time), 30600), 75600)) AS secs FROM b_merchant_group_event e FORCE INDEX (idx_overview) JOIN (SELECT g.roomid, g.dt FROM b_merchant_group_metric_daily g LEFT JOIN b_merchant_group_run_failure f ON f.corpid = g.corpid AND f.roomid = g.roomid AND f.stage = 'extract' AND (f.window_since IS NULL OR g.dt BETWEEN f.window_since AND f.window_until) WHERE g.corpid = @corp AND g.dt BETWEEN @since AND @until AND g.extraction_status = 'ok' AND g.fact_completed_time IS NOT NULL GROUP BY g.roomid, g.dt, g.fact_completed_time HAVING MAX(f.gmt_created_time) IS NULL OR MAX(f.gmt_created_time) < g.fact_completed_time) d ON d.roomid = e.roomid AND d.dt = e.occurred_on WHERE e.corpid = @corp AND e.occurred_on BETWEEN @since AND @until) SELECT c.roomid, c.events, c.merchant, c.unreplied, c.overdue, c.backlog, p.p50, p.p90 FROM (SELECT roomid, COUNT(*) AS events, CAST(SUM(asker_role = 'EXTERNAL') AS SIGNED) AS merchant, CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL) AS SIGNED) AS unreplied, CAST(SUM(asker_role = 'EXTERNAL' AND (first_agent_reply_time IS NULL OR secs > @sla)) AS SIGNED) AS overdue, CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL AND occurred_on < @until) AS SIGNED) AS backlog FROM ev GROUP BY roomid) c LEFT JOIN (SELECT roomid, MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.5) + 1) THEN secs END) AS p50, MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.9) + 1) THEN secs END) AS p90 FROM (SELECT roomid, secs, ROW_NUMBER() OVER (PARTITION BY roomid ORDER BY secs) AS rn, COUNT(*) OVER (PARTITION BY roomid) AS n FROM ev WHERE asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL) w GROUP BY roomid) p ON p.roomid = c.roomid ORDER BY c.roomid;

SELECT '== 4.3 按客服聚合（日志 2.02s / 2.14s）' AS section;
EXPLAIN ANALYZE
WITH inv AS (SELECT /*+ JOIN_ORDER(d, e, a) */ a.agent, e.id, e.roomid, e.occurred_on, e.asker_role, e.event_type, e.first_agent_reply_time, (e.first_responder = a.agent COLLATE utf8mb4_general_ci) AS owned, GREATEST(0, (TO_DAYS(e.first_agent_reply_time) - TO_DAYS(e.first_msg_time)) * 45000 + LEAST(GREATEST(TIME_TO_SEC(e.first_agent_reply_time), 30600), 75600) - LEAST(GREATEST(TIME_TO_SEC(e.first_msg_time), 30600), 75600)) AS secs FROM b_merchant_group_event e FORCE INDEX (idx_overview) JOIN (SELECT g.roomid, g.dt FROM b_merchant_group_metric_daily g LEFT JOIN b_merchant_group_run_failure f ON f.corpid = g.corpid AND f.roomid = g.roomid AND f.stage = 'extract' AND (f.window_since IS NULL OR g.dt BETWEEN f.window_since AND f.window_until) WHERE g.corpid = @corp AND g.dt BETWEEN @since AND @until AND g.extraction_status = 'ok' AND g.fact_completed_time IS NOT NULL GROUP BY g.roomid, g.dt, g.fact_completed_time HAVING MAX(f.gmt_created_time) IS NULL OR MAX(f.gmt_created_time) < g.fact_completed_time) d ON d.roomid = e.roomid AND d.dt = e.occurred_on JOIN JSON_TABLE(e.agents, '$[*]' COLUMNS(agent VARCHAR(16) PATH '$')) a WHERE e.corpid = @corp AND e.occurred_on BETWEEN @since AND @until) SELECT /*+ SET_VAR(optimizer_switch='derived_merge=off') */ c.agent, c.involved, c.rooms, c.owned, c.merchant_owned, c.reply_samples, c.overdue, p.p50, p.p90 FROM (SELECT agent, COUNT(DISTINCT id) AS involved, COUNT(DISTINCT roomid) AS rooms, CAST(SUM(owned) AS SIGNED) AS owned, CAST(SUM(owned AND asker_role = 'EXTERNAL') AS SIGNED) AS merchant_owned, CAST(SUM(owned AND asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL) AS SIGNED) AS reply_samples, CAST(SUM(owned AND asker_role = 'EXTERNAL' AND (first_agent_reply_time IS NULL OR secs > @sla)) AS SIGNED) AS overdue FROM inv GROUP BY agent) c LEFT JOIN (SELECT agent, MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.5) + 1) THEN secs END) AS p50, MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.9) + 1) THEN secs END) AS p90 FROM (SELECT agent, secs, ROW_NUMBER() OVER (PARTITION BY agent ORDER BY secs) AS rn, COUNT(*) OVER (PARTITION BY agent) AS n FROM inv WHERE owned AND asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL) w GROUP BY agent) p ON p.agent = c.agent ORDER BY c.agent;

SELECT '== 4.4 按二级分类聚合（日志 1.07s）' AS section;
EXPLAIN ANALYZE
WITH ev AS (SELECT e.event_type AS k, e.occurred_on, e.asker_role, e.first_agent_reply_time, GREATEST(0, (TO_DAYS(e.first_agent_reply_time) - TO_DAYS(e.first_msg_time)) * 45000 + LEAST(GREATEST(TIME_TO_SEC(e.first_agent_reply_time), 30600), 75600) - LEAST(GREATEST(TIME_TO_SEC(e.first_msg_time), 30600), 75600)) AS secs FROM b_merchant_group_event e FORCE INDEX (idx_overview) JOIN (SELECT g.roomid, g.dt FROM b_merchant_group_metric_daily g LEFT JOIN b_merchant_group_run_failure f ON f.corpid = g.corpid AND f.roomid = g.roomid AND f.stage = 'extract' AND (f.window_since IS NULL OR g.dt BETWEEN f.window_since AND f.window_until) WHERE g.corpid = @corp AND g.dt BETWEEN @since AND @until AND g.extraction_status = 'ok' AND g.fact_completed_time IS NOT NULL GROUP BY g.roomid, g.dt, g.fact_completed_time HAVING MAX(f.gmt_created_time) IS NULL OR MAX(f.gmt_created_time) < g.fact_completed_time) d ON d.roomid = e.roomid AND d.dt = e.occurred_on WHERE e.corpid = @corp AND e.occurred_on BETWEEN @since AND @until AND e.event_type IS NOT NULL) SELECT c.k, c.count, c.merchant, c.unreplied, p.p50, p.p90 FROM (SELECT k, COUNT(*) AS count, CAST(SUM(asker_role = 'EXTERNAL') AS SIGNED) AS merchant, CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL) AS SIGNED) AS unreplied FROM ev WHERE k IS NOT NULL GROUP BY k) c LEFT JOIN (SELECT k, MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.5) + 1) THEN secs END) AS p50, MIN(CASE WHEN rn = LEAST(n, FLOOR(n * 0.9) + 1) THEN secs END) AS p90 FROM (SELECT k, secs, ROW_NUMBER() OVER (PARTITION BY k ORDER BY secs) AS rn, COUNT(*) OVER (PARTITION BY k) AS n FROM ev WHERE asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL) w GROUP BY k) p ON p.k = c.k ORDER BY c.k;
