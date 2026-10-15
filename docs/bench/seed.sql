-- 只读工作台 SQL 的本机生产规模种子 —— 复现 docs/deploy.md「实测基线」里本机那一节的数字。
-- 形状照 2026-10-15 生产 7 天窗口：单企业 corpid='C' · 1,189 群 · 141 客服 · 约 1.45 万事件/天，
--   occurred_on 2026-08-30..2026-10-13（45 天），事件行宽约 2.7 KB。随机量全由 MD5(键|盐) 派生，可复现。
-- 由一次性的生成脚本产出（脚本没入库）；要改分布就直接改这份 SQL。
-- 用法见 docs/deploy.md「本机生产规模模拟」：先载 schema.sql，再载本文件（约 30 秒）。
-- ⚠️ 下面会 TRUNCATE 五张业务表、DROP 上游群配置表 b_wecom_merchant_group。第一句是护栏：
--    当前库名不以 _test 结尾就报错，mysql 客户端遇错即停（**别加 --force**）。
--    与 testutil::mysql_pool 的 _test 断言同一个规矩。
SELECT IF(DATABASE() LIKE '%\_test', 'ok', (SELECT 1 UNION ALL SELECT 2)) AS guard_database_must_end_with_test;
SET NAMES utf8mb4;
SET SESSION sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION';

TRUNCATE TABLE b_merchant_group_event;
TRUNCATE TABLE b_merchant_group_metric_daily;
TRUNCATE TABLE b_merchant_group_run_failure;
TRUNCATE TABLE b_merchant_group_taxonomy;
TRUNCATE TABLE b_merchant_group_merchant_summary;
DROP TABLE IF EXISTS b_wecom_merchant_group;
DROP TABLE IF EXISTS bench_seq, bench_room, bench_agent, bench_gd;

-- 1. 上游群配置表（schema.sql 里没有，read_filters 左连它；DDL 照 src/web/tests.rs 的夹具）
CREATE TABLE b_wecom_merchant_group (
  corp_id VARCHAR(64) NOT NULL, official_room_id VARCHAR(128) NOT NULL,
  group_name VARCHAR(255) NOT NULL DEFAULT '', merchant_id BIGINT UNSIGNED NULL,
  is_deleted TINYINT UNSIGNED NOT NULL DEFAULT 0, UNIQUE KEY uk_corp_room (corp_id, official_room_id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci;

-- 2. 词表 v1（CURRENT_VERSION = "v1"；内容取自 taxonomy_v1.toml，read_meta 的 check_types 会校验）
INSERT INTO b_merchant_group_taxonomy (version,type_id,parent_name,name,description) VALUES
('v1','urge_accept','履约催促','催接单','订单已挂出但迟迟没有师傅接单，重点是抱怨没人接，不是要求平台指派。'),
('v1','urge_dispatch','履约催促','催派单','要求平台尽快指派师傅、安排师傅。'),
('v1','urge_appointment','履约催促','催预约','已有师傅但迟迟没有约定上门时间，催促尽快定时间。'),
('v1','urge_contact_customer','履约催促','催联系客户','要求师傅打电话或联系客户，落点是联系动作，不是预约时间。'),
('v1','urge_visit','履约催促','催上门','时间已约定或已逾期，催促师傅实际到场施工。'),
('v1','urge_expedite','履约催促','加急履约','笼统要求整单加快，没有指向接单、派单、预约或上门中的具体环节。'),
('v1','tech_reassign','师傅调度','改派师傅','要求将订单从当前师傅换给另一位师傅。换人原因记录为属性。'),
('v1','tech_assign_specific','师傅调度','指定师傅','点名要求由某位具体师傅接单或施工。'),
('v1','tech_coordinate','师傅调度','协调师傅','要求平台居中协调师傅，但既不换人也不指定具体人员。'),
('v1','order_create','订单操作','新建订单','提交新的安装、维修、拆旧等需求，要求平台创建订单、生成工单或进入履约。已有订单的增删改不归此类。'),
('v1','order_add_item','订单操作','加单操作','在已有订单上增加服务数量或项目，订单本身不变更为新单。'),
('v1','order_remove_item','订单操作','减单操作','在已有订单上减少服务数量或项目，订单本身不取消。'),
('v1','order_cancel_service','订单操作','取消服务','取消整单或其中一项服务。'),
('v1','order_resume_service','订单操作','恢复服务','将已取消或挂起的订单重新开启。'),
('v1','order_edit_profile','订单操作','修改订单资料','修改地址、电话、联系人、店铺等资料，服务内容不变。'),
('v1','order_edit_service','订单操作','修改服务内容','将服务类型换成另一种，如安装改维修、普通拆改保护拆；不包括增减数量。'),
('v1','order_edit_schedule','订单操作','修改履约要求','修改已有订单的上门日期、时间段或其他履约约束。'),
('v1','order_edit_media','订单操作','修改订单附件','修改产品图片、补充或替换图片、上传现场附件。'),
('v1','order_hold','订单操作','挂起订单','暂停订单，先不派单也不上门，等待后续通知；订单仍保留。'),
('v1','order_return_intercept','订单售后','拦截退货','申请拦截退回、退货等，不等同于服务费用退款。'),
('v1','query_progress','业务查询','履约进度查询','只询问订单进行到哪一步，不带催促。'),
('v1','query_order_info','业务查询','订单信息查询','询问订单状态、内容或基础信息，不要求平台改动。'),
('v1','query_tech_info','业务查询','师傅信息查询','询问接单师傅、电话或预计到达时间，不要求换人或催促。'),
('v1','query_coverage','业务查询','区域覆盖查询','询问某地址或区域是否在服务范围内。'),
('v1','query_capability','业务查询','服务能力查询','询问某种商品、灯具或场景能否提供服务。'),
('v1','query_product_info','业务查询','商品信息咨询','询问能效、耗电量、规格、配件等商品本身信息，不是服务能力咨询。'),
('v1','fee_price_inquiry','费用结算','价格咨询','询问服务收费标准，尚未产生实际费用。'),
('v1','fee_surcharge','费用结算','附加费用','告知或申请基础费用之外的远程费、空跑费、加急费、超高费等。具体费种作为属性。'),
('v1','fee_liability','费用结算','费用承担','争议重点是谁承担费用，而不是费用金额是否正确。'),
('v1','fee_dispute','费用结算','收费争议','认为收费不合理、重复收费或与报价不一致。'),
('v1','fee_refund','费用结算','费用退款','要求退回已经收取的服务费用。'),
('v1','fee_payment','费用结算','订单支付','执行或催促订单支付，如扫码支付、余额支付、支付链接、加急支付。'),
('v1','fee_recharge','费用结算','账户充值','充值账户余额、对公转账、催财务充值或提交充值凭证。'),
('v1','info_order_not_found','信息异常','订单查不到','系统或师傅端查不到订单。'),
('v1','info_status_mismatch','信息异常','订单状态异常','系统状态与实际履约情况不一致。'),
('v1','info_content_mismatch','信息异常','订单内容不一致','订单中的数量、商品或服务内容与实际不一致。'),
('v1','info_customer_data_error','信息异常','客户资料异常','客户电话、地址等资料错误或无法联系，但尚未明确要求改成什么。'),
('v1','info_duplicate_order','信息异常','重复下单','同一业务产生多个订单，需要识别和处理重复订单。'),
('v1','info_link_error','信息异常','订单关联错误','主单、子单、拆旧单等订单关联关系挂错。'),
('v1','service_unavailable','履约异常','无法安排师傅','下单前或派单阶段明确没有师傅、不覆盖或暂时无法安排；现场客观无法施工仍归 service_blocked。'),
('v1','service_quality_issue','服务售后','服务质量异常','服务已完成但质量不好，如装歪、松动、异响；没有产生额外财产损失。'),
('v1','service_incomplete','服务售后','服务漏项','应做项目未完成就离场。'),
('v1','service_rework','服务售后','返工上门','要求师傅再次上门调整、补做或修复。'),
('v1','service_result_dispute','服务售后','服务结果争议','师傅称已完成、客户称未完成，双方对结果说法不一致。'),
('v1','service_blocked','服务售后','服务无法完成','师傅到现场后因没电、墙体条件、货不对版等客观原因无法施工。'),
('v1','provider_complaint','服务售后','师傅投诉','投诉师傅态度、行为、乱评价、违规操作等，非单纯施工质量问题。'),
('v1','evidence_request','服务售后','凭证核实','要求提供或核实图片、录音、聊天记录、现场证明等证据。'),
('v1','damage_incident','损坏赔付','损坏事件','服务过程造成服务对象之外的财产损失。'),
('v1','damage_liability_check','损坏赔付','责任核实','损失已经发生，正在确认由谁造成。'),
('v1','damage_compensation','损坏赔付','赔付处理','责任明确后商谈或执行赔偿。'),
('v1','order_checkin_verify','履约确认','订单核验','订单签到、核销、验收验证码、确认完工等履约确认动作。具体动作记录为属性。'),
('v1','account_store_operation','账号店铺','店铺账号操作','登录、解冻、权限、绑定店铺、新店审核、店铺归属等平台账户和店铺管理操作。'),
('v1','fulfillment_status_notice','履约信息','履约状态推送','平台主动通知已下单、已安排、已完工、正在协调等正常状态，不是商家查询。'),
('v1','noreq_none','无明确诉求','无明确诉求','只发送单一的订单号、手机号、地址、图片、视频、语音，或发送稍等、已处理、分隔符等没有独立业务动作的消息。若同一事件后续出现明确请求，应按明确请求重新分类。');

-- 3. 辅助表（seed 结束时删除）
CREATE TABLE bench_seq (n INT NOT NULL PRIMARY KEY);
INSERT INTO bench_seq
SELECT a.d + 10*b.d + 100*c.d + 1000*e.d FROM
 (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a,
 (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) b,
 (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) c,
 (SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) e;

-- 客服池：141 个 easyUserId（16 位定长）
CREATE TABLE bench_agent (agent_rank INT NOT NULL PRIMARY KEY, agent CHAR(16) NOT NULL) DEFAULT CHARSET utf8mb4 COLLATE utf8mb4_general_ci;
INSERT INTO bench_agent SELECT n, CONCAT('wo', LEFT(MD5(CONCAT('agent-', n)), 14)) FROM bench_seq WHERE n < 141;

-- 群：roomid 32 位；lam = 群日事件强度（重尾，8.3% 的群几乎没有事件）；dens = 每天有群日行的概率；
-- a0..a3 = 该群的「班组」四个客服槽位（rank 偏小的客服被更多群选中 -> 客服工作量重尾）
CREATE TABLE bench_room (
  room_no INT NOT NULL PRIMARY KEY, roomid VARCHAR(64) NOT NULL, lam DOUBLE NOT NULL, dens DOUBLE NOT NULL,
  a0 CHAR(16) NOT NULL, a1 CHAR(16) NOT NULL, a2 CHAR(16) NOT NULL, a3 CHAR(16) NOT NULL) DEFAULT CHARSET utf8mb4 COLLATE utf8mb4_general_ci;
INSERT INTO bench_room
SELECT r.n, CONCAT('wr', LEFT(MD5(CONCAT('room-', r.n)), 30)),
  IF((CONV(LEFT(MD5(CONCAT(r.n,'|dead')),7),16,10)/268435456) < 0.083, 0.012, 14.0 * (0.2 + 4 * POW((CONV(LEFT(MD5(CONCAT(r.n,'|w')),7),16,10)/268435456), 3))),
  0.6 + 0.4 * (CONV(LEFT(MD5(CONCAT(r.n,'|dens')),7),16,10)/268435456),
  (SELECT agent FROM bench_agent WHERE agent_rank = FLOOR(141 * POW((CONV(LEFT(MD5(CONCAT(r.n,'|s0')),7),16,10)/268435456), 2.2))),
  (SELECT agent FROM bench_agent WHERE agent_rank = FLOOR(141 * POW((CONV(LEFT(MD5(CONCAT(r.n,'|s1')),7),16,10)/268435456), 2.2))),
  (SELECT agent FROM bench_agent WHERE agent_rank = FLOOR(141 * POW((CONV(LEFT(MD5(CONCAT(r.n,'|s2')),7),16,10)/268435456), 2.2))),
  (SELECT agent FROM bench_agent WHERE agent_rank = FLOOR(141 * POW((CONV(LEFT(MD5(CONCAT(r.n,'|s3')),7),16,10)/268435456), 2.2)))
FROM bench_seq r WHERE r.n < 1189;

-- 群日计划：哪些 (群, 日) 有群日行，以及当天抽出几个事件 k（k=0 的群日照样有行）
CREATE TABLE bench_gd (
  dt DATE NOT NULL, room_no INT NOT NULL, roomid VARCHAR(64) NOT NULL, k INT NOT NULL, PRIMARY KEY (dt, room_no)) DEFAULT CHARSET utf8mb4 COLLATE utf8mb4_general_ci;
INSERT INTO bench_gd
SELECT d.dt, r.room_no, r.roomid,
  FLOOR(r.lam * (0.35 + 1.3 * (CONV(LEFT(MD5(CONCAT(CONCAT(r.room_no,'/',d.dt),'|v')),7),16,10)/268435456)) * d.f + (CONV(LEFT(MD5(CONCAT(CONCAT(r.room_no,'/',d.dt),'|w')),7),16,10)/268435456))
FROM bench_room r
JOIN (SELECT DATE('2026-08-30') + INTERVAL n DAY AS dt, 0.9 + 0.2 * (CONV(LEFT(MD5(CONCAT(n,'|f')),7),16,10)/268435456) AS f FROM bench_seq WHERE n < 45) d
WHERE (CONV(LEFT(MD5(CONCAT(CONCAT(r.room_no,'/',d.dt),'|ex')),7),16,10)/268435456) < r.dens;

-- 4. 事件：按天一个 INSERT ... SELECT（id 随 (日, 群) 递增，与生产「按批写入」的聚簇形状一致）
DELIMITER $$
CREATE PROCEDURE bench_seed_events()
BEGIN
  DECLARE d DATE DEFAULT '2026-08-30';
  WHILE d <= '2026-10-13' DO
    INSERT INTO b_merchant_group_event
      (corpid, roomid, source_msg_ids, first_msg_time, last_msg_time, first_agent_reply_time, occurred_on,
       asker, asker_role, agents, first_responder, summary, last_msg_role, followup_wait_max_sec,
       event_type, taxonomy_version, source_messages)
    SELECT /*+ NO_MERGE(b) */ 'C', b.roomid, CONCAT('["m', LEFT(MD5(CONCAT(b.ek,'#1')),24), '","m', LEFT(MD5(CONCAT(b.ek,'#2')),24), '","m', LEFT(MD5(CONCAT(b.ek,'#3')),24), '"', IF(b.u_nid >= 0.33, CONCAT(',"m', LEFT(MD5(CONCAT(b.ek,'#4')),24), '"'), ''), IF(b.u_nid >= 0.66, CONCAT(',"m', LEFT(MD5(CONCAT(b.ek,'#5')),24), '"'), ''), ']'), (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND), GREATEST((TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND) + INTERVAL CAST(CASE WHEN b.u_d1 < 0.86 THEN 120 + FLOOR(2400 * b.u_d2) WHEN b.u_d1 < 0.96 THEN 3600 + FLOOR(18000 * b.u_d2) ELSE 21600 + FLOOR(50400 * b.u_d2) END AS SIGNED) SECOND, COALESCE(IF((b.u_role < 0.135), (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND), IF(b.u_rp < 0.824, (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND) + INTERVAL CAST(LEAST(172800, ROUND(140 / POW(GREATEST(b.u_dl, 0.0005), 0.8))) AS SIGNED) SECOND, NULL)), (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND))), IF((b.u_role < 0.135), (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND), IF(b.u_rp < 0.824, (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND) + INTERVAL CAST(LEAST(172800, ROUND(140 / POW(GREATEST(b.u_dl, 0.0005), 0.8))) AS SIGNED) SECOND, NULL)), b.dt,
      IF((b.u_role < 0.135), ELT(((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1, b.a0, b.a1, b.a2, b.a3), CONCAT('wm', LEFT(MD5(CONCAT(b.room_no,'m',FLOOR(b.u_mer * 6))), 14))), IF((b.u_role < 0.135), 'INTERNAL', 'EXTERNAL'), IF((IF((b.u_role < 0.135), (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND), IF(b.u_rp < 0.824, (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND) + INTERVAL CAST(LEAST(172800, ROUND(140 / POW(GREATEST(b.u_dl, 0.0005), 0.8))) AS SIGNED) SECOND, NULL))) IS NOT NULL, CONCAT('["', ELT(((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1, b.a0, b.a1, b.a2, b.a3), '"', IF(IF(b.u_n < 0.032, 3, IF(b.u_n < 0.185, 2, 1)) >= 2 AND ELT((((((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1 + FLOOR(b.u_x * 3)) % 4)) + 1, b.a0, b.a1, b.a2, b.a3) <> ELT(((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1, b.a0, b.a1, b.a2, b.a3), CONCAT(',"', ELT((((((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1 + FLOOR(b.u_x * 3)) % 4)) + 1, b.a0, b.a1, b.a2, b.a3), '"'), ''), IF(IF(b.u_n < 0.032, 3, IF(b.u_n < 0.185, 2, 1)) >= 3 AND ELT((((((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1 + ((FLOOR(b.u_x * 3)) + 1) % 3) % 4)) + 1, b.a0, b.a1, b.a2, b.a3) <> ELT(((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1, b.a0, b.a1, b.a2, b.a3) AND ELT((((((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1 + ((FLOOR(b.u_x * 3)) + 1) % 3) % 4)) + 1, b.a0, b.a1, b.a2, b.a3) <> ELT((((((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1 + FLOOR(b.u_x * 3)) % 4)) + 1, b.a0, b.a1, b.a2, b.a3), CONCAT(',"', ELT((((((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1 + ((FLOOR(b.u_x * 3)) + 1) % 3) % 4)) + 1, b.a0, b.a1, b.a2, b.a3), '"'), ''), ']'), '[]'), IF((IF((b.u_role < 0.135), (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND), IF(b.u_rp < 0.824, (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND) + INTERVAL CAST(LEAST(172800, ROUND(140 / POW(GREATEST(b.u_dl, 0.0005), 0.8))) AS SIGNED) SECOND, NULL))) IS NOT NULL, ELT(((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1, b.a0, b.a1, b.a2, b.a3), NULL), CONCAT(ELT(1 + FLOOR(b.u_s1 * 8), '商家反馈订单长时间无师傅接单', '商家询问师傅上门的具体时间', '商家要求更换服务师傅', '商家对服务结果提出异议', '商家申请修改订单预约时间', '商家咨询订单费用的构成', '商家反映师傅迟到未联系客户', '商家要求取消当前订单'), ELT(1 + FLOOR(b.u_s2 * 8), '，平台已记录并联系师傅核实', '，平台答复正在协调资源处理', '，客服要求商家提供订单号截图', '，客服告知预计今天内给出结果', '，平台说明需要师傅反馈后确认', '，客服已转交对应负责人跟进', '，平台表示会尽快安排并回复', '，客服核对订单状态后答复商家'), ELT(1 + FLOOR(b.u_s3 * 8), '，商家表示可以接受', '，商家再次催促处理进度', '，后续等待师傅回复确认', '，商家补充了客户联系方式', '，问题暂未得到最终解决', '，商家要求给出明确的时间点', '，客服承诺稍后同步结果', '，商家对处理速度表示不满')), IF((IF((b.u_role < 0.135), (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND), IF(b.u_rp < 0.824, (TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND) + INTERVAL CAST(LEAST(172800, ROUND(140 / POW(GREATEST(b.u_dl, 0.0005), 0.8))) AS SIGNED) SECOND, NULL))) IS NULL, 'EXTERNAL', IF(b.u_lr < 0.6, 'INTERNAL', 'EXTERNAL')), IF(b.u_fw < 0.35, NULL, 60 + FLOOR(3000 * b.u_fw * b.u_fw)),
      IF(b.u_un < 0.015, '__untyped__', ELT(1 + LEAST(53, FLOOR(54 * POW(b.u_type, 2.0))), 'urge_dispatch', 'urge_visit', 'query_progress', 'urge_accept', 'order_edit_schedule', 'tech_reassign', 'service_quality_issue', 'fee_refund', 'urge_appointment', 'order_cancel_service', 'order_create', 'urge_expedite', 'fee_price_inquiry', 'service_incomplete', 'query_order_info', 'order_add_item', 'order_edit_media', 'order_edit_profile', 'order_edit_service', 'order_hold', 'order_remove_item', 'order_resume_service', 'order_return_intercept', 'fee_dispute', 'fee_liability', 'fee_payment', 'fee_recharge', 'fee_surcharge', 'evidence_request', 'provider_complaint', 'service_blocked', 'service_result_dispute', 'service_rework', 'urge_contact_customer', 'order_checkin_verify', 'fulfillment_status_notice', 'service_unavailable', 'tech_assign_specific', 'tech_coordinate', 'damage_compensation', 'damage_incident', 'damage_liability_check', 'noreq_none', 'info_content_mismatch', 'info_customer_data_error', 'info_duplicate_order', 'info_link_error', 'info_order_not_found', 'info_status_mismatch', 'query_capability', 'query_coverage', 'query_product_info', 'query_tech_info', 'account_store_operation')), 'v1', CONCAT('[', REPEAT(CONCAT('{"msg_id":"m', LEFT(MD5(b.ek),24), '","at":"', DATE_FORMAT((TIMESTAMP(b.dt) + INTERVAL CAST(IF(b.u_out < 0.08, FLOOR(86399 * b.u_tm3), 30600 + FLOOR(45000 * (b.u_tm1 + b.u_tm2) / 2)) AS SIGNED) SECOND), '%Y-%m-%d %H:%i:%s'), '","sender_id":"', IF((b.u_role < 0.135), ELT(((CASE WHEN b.u_sl < 0.5 THEN 0 WHEN b.u_sl < 0.75 THEN 1 WHEN b.u_sl < 0.9 THEN 2 ELSE 3 END)) + 1, b.a0, b.a1, b.a2, b.a3), CONCAT('wm', LEFT(MD5(CONCAT(b.room_no,'m',FLOOR(b.u_mer * 6))), 14))), '","sender_role":"EXTERNAL","text":"师傅什么时候能上门？订单一直没人联系我们，麻烦尽快处理一下"},'), 5 + FLOOR(b.u_nmsg * 8)), '{}]')
    FROM (SELECT g.room_no, s.n AS j, g.roomid, g.dt, r.a0, r.a1, r.a2, r.a3, CONCAT(g.room_no,'/',g.dt,'/',s.n) AS ek, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|tm1')),7),16,10)/268435456) AS u_tm1, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|tm2')),7),16,10)/268435456) AS u_tm2, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|tm3')),7),16,10)/268435456) AS u_tm3, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|out')),7),16,10)/268435456) AS u_out, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|role')),7),16,10)/268435456) AS u_role, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|rp')),7),16,10)/268435456) AS u_rp, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|dl')),7),16,10)/268435456) AS u_dl, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|d1')),7),16,10)/268435456) AS u_d1, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|d2')),7),16,10)/268435456) AS u_d2, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|sl')),7),16,10)/268435456) AS u_sl, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|n')),7),16,10)/268435456) AS u_n, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|x')),7),16,10)/268435456) AS u_x, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|type')),7),16,10)/268435456) AS u_type, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|un')),7),16,10)/268435456) AS u_un, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|nid')),7),16,10)/268435456) AS u_nid, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|nmsg')),7),16,10)/268435456) AS u_nmsg, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|s1')),7),16,10)/268435456) AS u_s1, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|s2')),7),16,10)/268435456) AS u_s2, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|s3')),7),16,10)/268435456) AS u_s3, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|lr')),7),16,10)/268435456) AS u_lr, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|fw')),7),16,10)/268435456) AS u_fw, (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt,'/',s.n),'|mer')),7),16,10)/268435456) AS u_mer
          FROM bench_gd g JOIN bench_seq s ON s.n < g.k JOIN bench_room r ON r.room_no = g.room_no
          WHERE g.dt = d) b
    ORDER BY b.room_no, b.j;
    SET d = d + INTERVAL 1 DAY;
  END WHILE;
END$$
DELIMITER ;
CALL bench_seed_events();
DROP PROCEDURE bench_seed_events;

-- 5. 群日：每个计划内的 (群, 日) 一行，全部 extraction_status='ok' 且有 fact_completed_time（T+2 凌晨 3 点）；
--    事件派生列取自已写入的事件，agent_accounts = 当天出现过的客服 easyUserId -> 账号
INSERT INTO b_merchant_group_metric_daily
 (corpid, roomid, dt, msg_count, sender_count, event_count, merchant_event_count, unreplied_count,
  first_reply_p50_sec, first_reply_p90_sec, extraction_status, classification_status, agent_accounts, fact_completed_time)
SELECT 'C', g.roomid, g.dt,
  20 + COALESCE(s.n, 0) * 9 + FLOOR(150 * (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt),'|mc')),7),16,10)/268435456)),
  2 + FLOOR(COALESCE(s.n, 0) / 3) + FLOOR(4 * (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt),'|sc')),7),16,10)/268435456)),
  COALESCE(s.n, 0), COALESCE(s.m, 0), COALESCE(s.u, 0),
  IF(s.rep > 0, 120 + FLOOR(900 * (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt),'|p5')),7),16,10)/268435456)), NULL), IF(s.rep > 0, 600 + FLOOR(6000 * (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt),'|p9')),7),16,10)/268435456)), NULL),
  'ok', 'ok', ac.m,
  TIMESTAMP(g.dt + INTERVAL 2 DAY) + INTERVAL (10800 + FLOOR(1200 * (CONV(LEFT(MD5(CONCAT(CONCAT(g.room_no,'/',g.dt),'|fc')),7),16,10)/268435456))) SECOND
FROM bench_gd g
LEFT JOIN (SELECT roomid, occurred_on, COUNT(*) AS n,
             CAST(SUM(asker_role = 'EXTERNAL') AS SIGNED) AS m,
             CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NULL) AS SIGNED) AS u,
             CAST(SUM(asker_role = 'EXTERNAL' AND first_agent_reply_time IS NOT NULL) AS SIGNED) AS rep
           FROM b_merchant_group_event WHERE corpid = 'C' GROUP BY roomid, occurred_on) s
  ON s.roomid = g.roomid AND s.occurred_on = g.dt
LEFT JOIN (SELECT roomid, occurred_on, JSON_OBJECTAGG(agent, CONCAT('u', LEFT(MD5(agent), 8))) AS m
           FROM (SELECT DISTINCT e.roomid, e.occurred_on, a.agent FROM b_merchant_group_event e
                 JOIN JSON_TABLE(e.agents, '$[*]' COLUMNS(agent VARCHAR(16) PATH '$')) a) t
           GROUP BY roomid, occurred_on) ac
  ON ac.roomid = g.roomid AND ac.occurred_on = g.dt
ORDER BY g.dt, g.room_no;

-- 6. 群配置表 + 商家摘要（每群一个商家，420 个商家；商家分组 12 个；业务经理 35 个）
INSERT INTO b_wecom_merchant_group (corp_id, official_room_id, group_name, merchant_id, is_deleted)
SELECT 'C', roomid, CONCAT('示例商家', room_no % 420, '服务群', room_no DIV 420), 100000 + room_no % 420, 0 FROM bench_room;
INSERT INTO b_merchant_group_merchant_summary
 (merchant_id, merchant_name, merchant_group_config_name, business_manager_id, business_manager_name)
SELECT 100000 + n, CONCAT('示例商家', n), ELT(1 + n % 12, '华东组','华南组','华北组','西南组','西北组','东北组','华中组','直营组','KA组','长尾组','试点组','未分组'),
       2000 + n % 35, CONCAT('经理', n % 35) FROM bench_seq WHERE n < 420;

-- 7. run_failure：窗口内 4 行，全部 stage='classify'、window_since 非空（不毒化抽取分母）
INSERT INTO b_merchant_group_run_failure (run_date, corpid, roomid, reason, stage, window_since, window_until, gmt_created_time)
SELECT f.run_date, 'C', r.roomid, '分类批次失败：模型返回无法解析（基准种子）', 'classify',
       f.run_date - INTERVAL 3 DAY, f.run_date - INTERVAL 2 DAY, TIMESTAMP(f.run_date) + INTERVAL 4 HOUR
FROM (SELECT DATE('2026-10-10') AS run_date, 10 AS room_no UNION ALL SELECT '2026-10-11', 200 UNION ALL
      SELECT '2026-10-12', 400 UNION ALL SELECT '2026-10-13', 800) f
JOIN bench_room r ON r.room_no = f.room_no;

-- 8. 收尾
DROP TABLE bench_seq, bench_room, bench_agent, bench_gd;
ANALYZE TABLE b_merchant_group_event, b_merchant_group_metric_daily, b_merchant_group_run_failure,
              b_merchant_group_taxonomy, b_merchant_group_merchant_summary, b_wecom_merchant_group;

