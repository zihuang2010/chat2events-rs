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
-- 事实列（冻结区 occurred_on < T-(N+1) 不可写）：除 event_type / taxonomy_version 外的全部。
-- 标注列：新事实保存后独立补齐；冻结区已有标签只因词表升版重打。
-- 已有库升级步骤见 docs/deploy.md 的「独立打标流水线升级」。
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
    summary                VARCHAR(200)    NOT NULL COMMENT '事件摘要。契约：中文一句话≤100字，不含ID/脱敏占位符。ID 由抽取校验器硬拦（不通过即整批失败），脱敏占位符就地抹除；≤100 字是软的，超了只重问一次就放行，列宽 200 才是硬闸',
    last_msg_role          ENUM('EXTERNAL','INTERNAL') NULL COMMENT '末条来源消息发送方的identityType——asker_role的镜像，同源同形，只是取末条而非首条。EXTERNAL=商家说完没人接（把人晾着），INTERNAL=客服收的尾。确定性计算不经模型，与first_agent_reply_time同一条路。NULL=加这一列之前抽取的历史行，不是某一边（承重不变量4的形状），冻结区不可回填。不进idx_overview：今天只在事件明细逐行显示，没有聚合点',
    followup_wait_max_sec  INT UNSIGNED    NULL     COMMENT '后续轮次最长等待秒数（工作时段口径）。首响之后每次EXTERNAL→INTERNAL间隔取最大，扣除08:30-21:00之外的时间；周末与节假日不扣（没有工作日历）。末尾没人接的那一段不计——那是last_msg_role的活。⚠️与首响时效同一个工作时段口径（曾经只有本列这么算）。但两者可改性不同：首响由两个时间列查询期现算、口径随时可改，而后续等待查询期拿不到中间轮次，只能写入时算，口径就此定死。0=确实没有后续轮次（算出来的事实），NULL=没算过（加这一列之前的历史行，冻结区不可回填）——承重不变量4，两者绝不混。算指标前先滤掉asker_role=INTERNAL，与首响同一条WHERE。不进idx_overview：今天只在事件明细逐行显示',
    event_type             VARCHAR(64)     NULL COMMENT '事件类型；NULL=尚未完成打标，__untyped__=模型归不上去或v0未建词表。一个事件一个类——曾经还有一列event_types存标签全集（副类只供下钻、不进任何指标），2026-09-14连同整套多标签机制移除',
    taxonomy_version       VARCHAR(16)     NULL COMMENT '打标所用词表版本；NULL=尚未完成打标',
    source_messages        MEDIUMTEXT      NULL COMMENT '来源消息渲染快照。JSON数组，与source_msg_ids等长同序，元素就是webUI下钻接口的返回体（msg_id/at/sender_id/sender_role/text）。展示列——既非事实列也非标注列，不参与任何指标、不回读进Event、不参与抽取与打标。非TEXT消息的text已在抽取时换成占位符（[图片]等），媒体URL一律不存（带签名会过期）。NULL=升级前的历史行，不是空数组（承重不变量4的形状）。不用JSON类型：MySQL的JSON是带键偏移索引的二进制格式，比等价文本更大，而这一列只整块读写、从不JSON_EXTRACT',
    gmt_created_time       DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    KEY idx_shard (corpid, roomid, occurred_on) COMMENT '分片删重写必需。**也是「点进某个群看历史」的最优索引**：群号已知时等值+范围，比idx_overview更贴',
    KEY idx_day (occurred_on) COMMENT 'BI直连：按时间段捞事件明细',
    KEY idx_modified (gmt_modified_time) COMMENT '只读工作台缓存的数据戳：MAX(gmt_modified_time) 走索引尾读，一次一行。没有它每个请求都全表扫一遍',
    KEY idx_overview (corpid, occurred_on, roomid, asker_role, event_type, first_msg_time, first_agent_reply_time, last_msg_time) COMMENT '只读工作台全局概览的覆盖索引。前两列corpid等值+occurred_on范围负责定位（等值列必须在范围列之前）；后六列只为「别回表」——聚合要用的全在这儿，47万行的窗口聚合不必回表47万次。首响秒差由这两个时间列在索引内算出并过滤（ICP），所以不需要生成列。⚠️agents是JSON进不了索引，按客服筛选那条查询会回表，是已知且有意的例外。⚠️它是idx_corp_day(corpid,occurred_on)的超集，那条已删'
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
    first_reply_p50_sec INT UNSIGNED        NULL     COMMENT '首响时效P50（**工作时段秒数**，只算08:30-21:00之内，周末节假日不扣）。只统计商家发起的事件。用分位数不用均值：一条几小时才回的会把均值整个带偏。⚠️2026-09改的口径，此前是墙钟差——历史行是否已迁移见docs/deploy.md「首响口径改为工作时段」',
    first_reply_p90_sec INT UNSIGNED        NULL     COMMENT '首响时效P90（**工作时段秒数**），只统计商家发起的事件。口径与迁移同P50',
    extraction_status   ENUM('ok','failed') NOT NULL COMMENT '抽取状态。ok=算出来了（可能是0个事件），failed=没算出来（事件级列全为NULL）',
    classification_status ENUM('pending','ok','failed') NOT NULL DEFAULT 'ok' COMMENT '打标状态；pending=已保存事实，ok=标签和分类指标齐全，failed=打标未完成',
    agent_accounts      JSON               NULL COMMENT '本轮群窗口的平台客服账号映射，easyUserId到officialUserId；供独立打标后计算客服指标',
    fact_completed_time DATETIME(6)        NULL COMMENT '事实成功保存时间；仅成功抽取写入，打标不修改；NULL表示缺少可信完成凭据',
    gmt_created_time    DATETIME            NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time   DATETIME            NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    UNIQUE KEY uk_group_daily (corpid, roomid, dt) COMMENT '语义键：REPLACE覆盖写靠它触发冲突',
    KEY idx_day (dt) COMMENT 'BI直连：跨群看某一天/某时间段',
    KEY idx_corp_day (corpid, dt) COMMENT '只读工作台按企业与日期读取群日',
    KEY idx_modified (gmt_modified_time) COMMENT '只读工作台缓存的数据戳，同事件表那条'
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
    official_user_id   VARCHAR(64)         NULL COMMENT '平台客服账号，对应sender.officialUserId；取本次群处理窗口最新非空值，缺失为NULL，不参与语义键',
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
-- b_merchant_group_agent_msg_daily —— 语义键四列（uk_agent_msg_daily）
--
-- 客服自己发了多少条消息。**用来对冲「只看处理量」** —— 上面那张表只有事件数，
-- 而 b_merchant_group_metric_daily.msg_count 是群级、不分人。
--
-- ⚠️ **为什么不是 b_merchant_group_agent_metric_daily 上的一列**，三条理由：
--   1. 那张表的语义键含 event_type，而消息数不随类型变 —— 挂上去同一个数字要在
--      N 个类型行里各存一遍，BI 直连 SUM 会按类型数放大，且看起来完全正常。
--   2. 那张表按 first_responder 归属。发了 200 条却一次首响都没抢到的客服在那张表上
--      **一行都没有** —— 而「对冲只看处理量」要看的正是这种人。
--   3. 生命周期对不上：那张表由打标阶段整段删重写、抽取失败时整行缺失、词表升版
--      再重写一遍；而消息数**既不依赖抽取也不依赖词表**，抽取失败的群照样算得出来。
--
-- 所以这张表由**事实阶段**（store::write_room 的同一个事务）写，
-- 抽取失败照写、打标和重打标一个字节都不碰，也没有 taxonomy_version。
--
-- 走 REPLACE 不做删重写：raw 只增不删，同一个「群 × 日」的客服集合只会变大不会变小，
-- 不存在需要清掉的陈旧行。与 b_merchant_group_metric_daily 同一个写法。
-- ⚠️ 同理，REPLACE = DELETE + INSERT，这张表上的 id 每重算一次就换一个新值。
-- ⚠️ 这里**没有 official_user_id** —— 账号映射已经在 b_merchant_group_metric_daily
--    的 agent_accounts 里，存第二份会打架。
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE b_merchant_group_agent_msg_daily (
    id                BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT '主键ID',
    corpid            VARCHAR(32)     NOT NULL COMMENT '企业ID',
    room              VARCHAR(64)     NOT NULL COMMENT '群ID。与客服日指标表同粒度，跨群总量查询时SUM',
    agent             CHAR(16)        NOT NULL COMMENT '平台客服（INTERNAL）easyUserId',
    dt                DATE            NOT NULL COMMENT '统计日',
    msg_count         INT UNSIGNED    NOT NULL COMMENT '该客服该日在该群发的消息条数。只数INTERNAL，不依赖抽取与词表',
    gmt_created_time  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP COMMENT '创建时间',
    gmt_modified_time DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT '更新时间',
    PRIMARY KEY (id),
    UNIQUE KEY uk_agent_msg_daily (corpid, room, agent, dt) COMMENT '语义键四列：REPLACE覆盖写靠它触发冲突',
    KEY idx_agent (agent, dt) COMMENT 'BI直连：某客服某时间段（语义键前缀是corpid，按人查走不到）',
    KEY idx_day (dt) COMMENT 'BI直连：跨群跨人看某时间段'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '客服维度日消息量';

-- ─────────────────────────────────────────────────────────────────────────────
-- b_merchant_group_taxonomy —— 版本化的类型词表。人工执行插入，不做管理界面。
--
-- description 必填 —— classify 就靠它认类，**不依赖任何向量**。
-- **人工加类只能通过升版**：不做「给现有版本热加一个类」，那会让同一个版本号
-- 在不同时间对应两套词表，taxonomy_version 就失去意义。
--
-- ⚠️ 曾经有一列 `centroid JSON NULL`（类中心向量，「有就多一条分类路径」）。
-- 2026-09-03 删掉：机器归纳舍弃之后，**没有任何路径能产出它**，也没有任何代码
-- 读写它 —— 它只会是一列永远为 NULL 的承诺。真要走向量路径时再加回来，
-- 那时它的产出方和读取方会一起进来。已建过表的库：
--   ALTER TABLE b_merchant_group_taxonomy DROP COLUMN centroid;
--
-- 词表是**两级**的，但只有二级进 event_type：parent_name 是一级分类名，
-- type_id / name 是二级（叶子）。一级不单独建行、不做自引用 —— 它是叶子的一个属性列。
-- 这样 event_type 存的仍是叶子，uk_agent_daily 六列语义键和全部指标一个字不动；
-- 报表要一级维度就 JOIN 这张表按 (version, type_id) 取 parent_name 再 GROUP BY。
-- 不给 parent_name 建索引：词表一共几十行，全表扫比维护索引便宜。
--
-- 已建过表的库补这一列（parent_name 是后加的）：
--   ALTER TABLE b_merchant_group_taxonomy
--     ADD COLUMN parent_name VARCHAR(64) NOT NULL AFTER type_id;
-- 补完必须逐行填上真实的一级分类名 —— 空串过不了 Draft::check，且它会逐字进分类 prompt。
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE b_merchant_group_taxonomy (
    id                BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT '主键ID',
    version           VARCHAR(16)     NOT NULL COMMENT '词表版本。人工加类只能通过升版，不做热加',
    type_id           VARCHAR(64)     NOT NULL COMMENT '类型ID（二级/叶子）。event_type 存的就是它',
    parent_name       VARCHAR(64)     NOT NULL COMMENT '一级分类名。只用于分组——不进event_type、不进任何语义键；报表要一级维度靠JOIN本表取它',
    name              VARCHAR(128)    NOT NULL COMMENT '二级类型名，人工审阅后确定',
    description       TEXT            NOT NULL COMMENT '类型描述。必填——classify靠它认类，逐字进分类prompt',
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
    stage             ENUM('extract','classify') NOT NULL DEFAULT 'extract' COMMENT '失败阶段；打标失败不使已保存事实失效',
    window_since      DATE            NULL     COMMENT '本次失败覆盖的数据窗口起点。⚠️不是run_date——那是跑批日，T+2下与数据日差两天，且lookback_days可配、backfill窗口任意，反推不可靠。只读工作台判事实新鲜度时用它把影响面夹在这个区间内：没有它，今天一次失败会把这个群全部历史（含冻结区里早已成功的天）标成unknown，而冻结区不会再被重抽，不可逆。NULL=加这两列之前的历史行，保守当作影响全历史（承重不变量5：历史未知不能推断成已知）',
    window_until      DATE            NULL     COMMENT '本次失败覆盖的数据窗口终点；与window_since同时写入或同时为NULL',
    gmt_created_time  DATETIME(6)     NOT NULL DEFAULT CURRENT_TIMESTAMP(6) COMMENT '创建时间；与事实完成凭据同精度，避免同秒失败被遗漏',
    gmt_modified_time DATETIME(6)     NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6) COMMENT '更新时间',
    PRIMARY KEY (id),
    KEY idx_run (run_date) COMMENT '按跑批日查失败群',
    KEY idx_room_stage_time (corpid, roomid, stage, gmt_created_time) COMMENT '按群与阶段定位最后失败时间，避免每次聚合全部历史'
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_general_ci COMMENT = '跑批失败记录（群×本次运行）';
