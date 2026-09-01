-- Chat2Events 表结构。**人工执行一次**：
--
--     mysql -h "$MYSQL_HOST" -P "$MYSQL_PORT" -u "$MYSQL_USER" -p "$MYSQL_DATABASE" < schema.sql
--
-- 不用 CREATE TABLE IF NOT EXISTS —— 它会掩盖「表结构变了但没迁移」。
-- 不引 ORM、不引 migration 框架：跑批进程只读写数据，不碰 DDL。
--
-- 遵循公司《数据库规范》，本仓库适用条款与**四条已取下的例外**见
-- docs/database-conventions.md。要求 MySQL 8.0+。
--
-- **InnoDB 是承重的**（承重不变量 2）：一个群一次运行的 N 个分片必须在同一个事务里。
-- occurred_on = date(first_msg_time)，而「首条来源消息是哪条」由模型判断 —— 同一个
-- event 会在分片之间移动。分两个事务提交，中间失败就会造成它一个分片都不在，
-- 或者两个分片都在。
--
-- **原始消息不入库**：./data/raw/ 已经是不可变的唯一事实来源，
-- 溯源靠 source_msg_ids 回 raw 区查（DuckDB 一条 SQL）。
-- **embedding 也不入库**：存内容寻址缓存文件，不引向量库。
--
-- ⚠️ **不加 is_deleted**（规范例外 A）：本仓库只有物理删重写和覆盖写，没有软删场景。
--    一个恒为 0 的 is_deleted 会让 BI 以为存在「被软删的历史行」。要历史看 ./data/raw/。

-- ─────────────────────────────────────────────────────────────────────────────
-- b_merchant_group_event —— 按 (corpid, roomid, occurred_on) 分片删重写
--
-- 事实列（冻结区 occurred_on < T-N 不可写）：除 event_type / taxonomy_version 外的全部。
-- 标注列（任何时候可写，但只有词表升版这一个原因）：event_type / taxonomy_version。
-- summary 归**事实列** —— 它由抽取那一次的模型决定，而冻结区本来就不再跑抽取。
-- 这保证冻结区的 sha256(summary) 缓存永远命中。
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE b_merchant_group_event (
    id                     BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT '主键ID',
    corpid                 VARCHAR(32)     NOT NULL COMMENT '企业ID。样本恒18位但只有1个取值，不足以定长化（规范例外B）',
    roomid                 VARCHAR(64)     NOT NULL COMMENT '群ID = officialRoomId = 文件名',
    source_msg_ids         JSON            NOT NULL COMMENT '来源消息sourceMessageId数组。溯源：非空且每个ID必须真实存在于该次抽取的消息里。用JSON列不拆关系表、不建索引——反查是低频人工操作',
    first_msg_time         DATETIME        NOT NULL COMMENT '首条来源消息时间',
    last_msg_time          DATETIME        NOT NULL COMMENT '末条来源消息时间',
    first_agent_reply_time DATETIME        NULL     COMMENT '首条INTERNAL来源消息时间。首响锚点，NULL=未回复。首响时效=first_agent_reply_time-first_msg_time',
    occurred_on            DATE            NOT NULL COMMENT '归属日 = date(first_msg_time)。报表归属日 + 幂等分片键',
    asker                  CHAR(16)        NOT NULL COMMENT '提问方easyUserId。16位定长，内外统一形态；officialUserId形态混杂（手机号/字母账号）换号即腰斩，不可用作主键',
    asker_role             ENUM('EXTERNAL','INTERNAL') NOT NULL COMMENT '发起方角色=首条来源消息发送方的identityType。EXTERNAL=商家发起，INTERNAL=平台发起（工单推送类，first_agent_reply_time恒等于first_msg_time、首响0秒，算首响指标前要先滤掉）',
    agents                 JSON            NOT NULL COMMENT '涉及的全部INTERNAL成员easyUserId数组，全存。归属口径换了不用重跑LLM',
    first_responder        CHAR(16)        NULL     COMMENT 'first_agent_reply_time那条消息的发送方easyUserId',
    summary                VARCHAR(200)    NOT NULL COMMENT '事件摘要。契约：中文一句话≤100字，不含ID/脱敏占位符（落库前由抽取校验器拦截）',
    event_type             VARCHAR(64)     NOT NULL COMMENT '事件类型。为空时用显式的__untyped__不用NULL；v0+__untyped__=还没有词表（系统状态），vN+__untyped__=归不上去（数据信号）',
    taxonomy_version       VARCHAR(16)     NOT NULL COMMENT '打标所用的词表版本',
    gmt_created_time       DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    KEY idx_shard (corpid, roomid, occurred_on) COMMENT '分片删重写必需',
    KEY idx_day (occurred_on) COMMENT 'BI直连：按时间段捞事件明细'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '抽取出的结构化业务事件';

-- ─────────────────────────────────────────────────────────────────────────────
-- b_merchant_group_metric_daily —— 群 × 日，REPLACE 覆盖写（靠 uk_group_daily 触发冲突）
--
-- 混着两类指标：消息级不依赖抽取（失败的群照样有），事件级依赖。
--
-- ⚠️ **首响口径只算商家发起的事件**（asker_role='EXTERNAL'）。平台发起的工单推送
--    first_agent_reply_time 恒等于 first_msg_time —— 首响 0 秒、且永远算「已回复」，
--    混进来会同时拉低分位数和未回复率。所以分母是 merchant_event_count 不是 event_count，
--    单独存一列：不存的话 BI 只能拿 event_count 当分母，那是个静默偏低的比率。
-- **Ok([]) 与 Failed 绝不混淆**（承重不变量 4）：
--     Ok([])  这天确实没有业务事件，正常  -> extraction_status='ok'     事件级 = 0
--     Failed  没算出来                    -> extraction_status='failed' 事件级 = NULL
-- 绝不用 0 表示「没算出来」。
--
-- ⚠️ REPLACE = DELETE + INSERT，所以这张表上的 id 每重算一次就换一个新值、
--    gmt_created_time 也重置 —— **id 不是稳定行标识**，语义键是 uk_group_daily。
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE b_merchant_group_metric_daily (
    id                  BIGINT UNSIGNED     NOT NULL AUTO_INCREMENT COMMENT '主键ID。REPLACE写入，每次覆盖都会换新值，不是稳定行标识',
    corpid              VARCHAR(32)         NOT NULL COMMENT '企业ID',
    roomid              VARCHAR(64)         NOT NULL COMMENT '群ID',
    dt                  DATE                NOT NULL COMMENT '统计日',
    msg_count           INT UNSIGNED        NOT NULL COMMENT '当日消息条数。消息级指标，不依赖抽取，失败的群照样有',
    sender_count        INT UNSIGNED        NOT NULL COMMENT '当日发言人数。消息级指标',
    event_count         INT UNSIGNED        NULL     COMMENT '当日事件数（含平台发起）。事件级指标，抽取失败时为NULL不是0',
    merchant_event_count INT UNSIGNED       NULL     COMMENT '当日asker_role=EXTERNAL的事件数。**unreplied_count与两个首响分位数的分母就是它，不是event_count**——用event_count当分母会得到一个偏低但看起来正常的未回复率',
    unreplied_count     INT UNSIGNED        NULL     COMMENT '当日未回复的商家发起事件数（first_agent_reply_time IS NULL）。分母是merchant_event_count。抽取失败时为NULL',
    first_reply_p50_sec INT UNSIGNED        NULL     COMMENT '首响时效P50（秒），只统计商家发起的事件。用分位数不用均值：一条几小时才回的会把均值整个带偏',
    first_reply_p90_sec INT UNSIGNED        NULL     COMMENT '首响时效P90（秒），只统计商家发起的事件',
    extraction_status   ENUM('ok','failed') NOT NULL COMMENT '抽取状态。ok=算出来了（可能是0个事件），failed=没算出来（事件级列全为NULL）',
    gmt_created_time    DATETIME            NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time   DATETIME            NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    UNIQUE KEY uk_group_daily (corpid, roomid, dt) COMMENT '语义键：REPLACE覆盖写靠它触发冲突',
    KEY idx_day (dt) COMMENT 'BI直连：跨群看某一天/某时间段'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '群维度日指标';

-- ─────────────────────────────────────────────────────────────────────────────
-- b_merchant_group_agent_metric_daily —— 语义键六列（uk_agent_daily）
--
--   event_type        进键：否则一行只能存总量，存不了「每类各多少个」
--   taxonomy_version  进键：词表会升版重打标，不记版本这张表就是一堆无法解释的数字
--   room              进键：**键必须嵌套在「群 × 日」的失败隔离粒度里**。
--                     没有它，小明在 A 群和 B 群都干了活、B 群抽取失败被跳过时，
--                     他当天那一行会被只含 A 群的数字覆盖 —— 残缺覆盖完整。
--                     跨群总量查询时 SUM。
--
-- **不单独存总量行** —— 总量 = 求和。存两处会打架。
-- ⚠️ 失败的群在这张表上是**整行缺失**（不是 0，也不需要状态列 —— 它已经嵌套进
--    失败粒度了）。**判断某客服当天的数字完不完整 → join b_merchant_group_metric_daily 看
--    当天有没有 failed 行。直接 SUM 而不检查这一点，会得到一个偏小但看起来正常的数字。**
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE b_merchant_group_agent_metric_daily (
    id                BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT '主键ID',
    corpid            VARCHAR(32)     NOT NULL COMMENT '企业ID',
    room              VARCHAR(64)     NOT NULL COMMENT '群ID。进键是为了让键嵌套进「群×日」的失败隔离粒度',
    agent             CHAR(16)        NOT NULL COMMENT '平台客服（INTERNAL）easyUserId',
    dt                DATE            NOT NULL COMMENT '统计日',
    event_type        VARCHAR(64)     NOT NULL COMMENT '事件类型，空用__untyped__',
    taxonomy_version  VARCHAR(16)     NOT NULL COMMENT '词表版本',
    event_count       INT UNSIGNED    NOT NULL COMMENT '该客服该日该类型的事件数',
    gmt_created_time  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    UNIQUE KEY uk_agent_daily (corpid, room, agent, dt, event_type, taxonomy_version) COMMENT '语义键六列',
    KEY idx_agent (agent, dt) COMMENT 'BI直连：某客服某时间段（语义键前缀是corpid，按人查走不到）',
    KEY idx_day (dt) COMMENT 'BI直连：跨群跨人看某时间段'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '客服维度日指标';

-- ─────────────────────────────────────────────────────────────────────────────
-- b_merchant_group_taxonomy —— 版本化的类型词表。人工执行插入，不做管理界面。
--
-- 要同时容纳两种类：LLM 归纳产出的类**没有 centroid**，人工加的类同理。
-- 所以 description 必填 —— 让 classify 不依赖向量也能工作；有 centroid 就多一条路径。
-- **人工加类只能通过升版**：不做「给现有版本热加一个类」，那会让同一个版本号
-- 在不同时间对应两套词表，taxonomy_version 就失去意义。
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE b_merchant_group_taxonomy (
    id                BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT '主键ID',
    version           VARCHAR(16)     NOT NULL COMMENT '词表版本。人工加类只能通过升版，不做热加',
    type_id           VARCHAR(64)     NOT NULL COMMENT '类型ID',
    name              VARCHAR(128)    NOT NULL COMMENT '类型名，人工审阅后确定',
    description       TEXT            NOT NULL COMMENT '类型描述。必填——LLM归纳和人工加的类都没有centroid，靠它让classify不依赖向量也能工作',
    centroid          JSON            NULL     COMMENT '类中心向量，可空。有就多一条分类路径',
    gmt_created_time  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    UNIQUE KEY uk_taxonomy (version, type_id) COMMENT '语义键'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '版本化类型词表';

-- ─────────────────────────────────────────────────────────────────────────────
-- b_merchant_group_run_failure —— 追加。粒度 = 群 × 本次运行。
-- 不记窗口范围：窗口已经是抽取模块的内部细节，不上浮到接口上。
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE b_merchant_group_run_failure (
    id                BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT '主键ID',
    run_date          DATE            NOT NULL COMMENT '跑批日',
    corpid            VARCHAR(32)     NOT NULL COMMENT '企业ID',
    roomid            VARCHAR(64)     NOT NULL COMMENT '群ID',
    reason            TEXT            NOT NULL COMMENT '失败原因',
    gmt_created_time  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    KEY idx_run (run_date) COMMENT '按跑批日查失败群'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '跑批失败记录（群×本次运行）';
