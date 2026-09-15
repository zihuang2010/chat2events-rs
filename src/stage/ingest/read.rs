//! 读 —— **DuckDB 连接 · SQL · 上游字段语义三样只住这个文件**。
//! 上游 camelCase 字段名只允许出现在下面那条 `SELECT ... AS ...` 里。
//!
//! 端口的读函数在这里（[`read_room`] / [`read_synced_room`]），
//! `list_rooms` 只遍历目录、不碰 DuckDB，所以住在 `layout.rs`。
//!
//! ⚠️ **`read_by_ids` 已删。** 它唯一的读取点是 webUI 下钻，而下钻改读
//! `b_merchant_group_event.source_messages` 之后，那条路径整个不存在了 ——
//! 连带 `scan` 的 `ids` 参数、`IngestError::Missing` 和那个 410。

use super::{
    layout::{MONTH_FMT, files, synced_files},
    types::{Conversation, IngestError, Message, Result, Role},
};
use crate::window::Window;
use chrono::{NaiveDate, NaiveDateTime};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

const SCHEMA_VERSION: i64 = 1;
const PARSER_VERSION: i64 = 1;

/// 归属日按业务本地时区算，不按 UTC。跨零点的归属会差一天。
///
/// ⚠️ **写成定值偏移，不写 `AT TIME ZONE 'Asia/Shanghai'`** —— 后者是 ICU 扩展提供的，
/// 而 ICU 没法像 json 那样静态链进来（`duckdb` 的 `icu` feature 会把整个构建切成
/// `bundled-cmake`）。于是它只能运行时联网下载：CI 在公网上跑、平台串是真的，
/// 下得到，全绿；跑批机在内网、平台串又是 `DUCKDB_CUSTOM_PLATFORM` 那个假的，
/// 404，**每个群都在 `read_room()` 第一句就失败**。实测踩过一次。
///
/// 换成偏移是无损的：Asia/Shanghai 自 1991 年起无夏令时，恒 UTC+8。
/// 「+8」这条换算另一处在 `testutil::upstream_ms`（测试侧的反向换算）。
const TZ_OFFSET_MICROS: i64 = 8 * 3600 * 1_000_000;

/// 样本里出现过的类型。**不做过滤** —— 「什么算业务事件」是 ③ 的活，
/// 这里只负责把没见过的类型吼一声，好让真实数据自己告诉我们还有什么。
const KNOWN_TYPES: [&str; 4] = ["TEXT", "IMAGE", "GIF", "VIDEO"];

/// 只为日志去重，不参与任何逻辑。进程级，跑完即退出。
static SEEN_UNKNOWN: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// 全进程**一个** DuckDB 实例，每次查询从它 `try_clone` 一条连接出来。
///
/// 不只是省开连接那点时间：`Connection::open_in_memory()` 每次都新建一个完整的数据库
/// 实例 —— 实测 **24.5ms/次**，且每个实例自带 `threads = 核数` 个工作线程、声明
/// `memory_limit = 80% RAM`。`room_concurrency = 10` 时那是 **10 × 12 个工作线程压在 12 个
/// 核上、10 份各 12.7 GiB 的内存预算声明**。共享之后线程池和内存上限都只剩一份，
/// `try_clone` 实测 **0.03ms**（10 个真实群的 `read_room` 共 957ms，其中 245ms 是纯开连接）。
///
/// 锁只圈住 `try_clone` 这一下 —— 查询在各自的连接上跑，不进临界区。要锁是因为
/// `duckdb::Connection` 是 `Send` 不是 `Sync`。
///
/// 建实例失败 / 锁毒化都不是「某个群的事」：进程内内存数据库都起不来，整轮本来就该死，
/// 按硬规则用 `expect`。
///
/// 建好就**关掉扩展自动下载**：跑批机在内网，联网取扩展是死路，而 DuckDB 的默认
/// 行为是静默去 extensions.duckdb.org 取。关掉之后「SQL 用了个需要扩展的函数」
/// 在**干净机器**上当场报错 —— CI 的 runner 就是干净的，于是这类事故在 CI 就现形，
/// 不用等部署（ICU 就是这么漏过去的，见 [`TZ_OFFSET_MICROS`]）。
///
/// ⚠️ 实测：`autoload_known_extensions = false` **拦不住已经装在 `~/.duckdb/extensions`
/// 里的那份**（本机开发机上 icu 照样 loaded=true）。真正兑现这条的是
/// `autoinstall`。所以本地全绿不等于目标机能跑，判据以 CI 为准。
/// json 是 `STATICALLY_LINKED` 的内建扩展，不走这条路。
static DB: LazyLock<Mutex<duckdb::Connection>> = LazyLock::new(|| {
    let con = duckdb::Connection::open_in_memory().expect("建 DuckDB 实例");
    con.execute_batch(
        // 两个独立进程各自限制工作集，另为 Rust 会话、事件、JSON 留内存；这不是 RSS 硬上限。
        //
        // ⚠️ **`memory_limit` 和 `threads` 是一组的，改一个必须重算另一个，还要连着
        // `ingest.room_concurrency` 一起算。** JSON 扫描器每个工作线程固定申请
        // **32 MiB** 读缓冲，**与文件大小无关**（148 KB 的月文件照样要 32 MiB）——
        // DuckDB 1.5 里是硬编码的，`maximum_object_size` 参数改不动它，
        // `duckdb_settings()` 里也没有任何旋钮。而 `room_concurrency` 个群同时在扫，
        // 每个查询各握自己那份，峰值 ≈ (room_concurrency + threads) × 32 MiB。
        //
        // 实测（duckdb CLI 1.5.5，937 KB 的 ndjson）单次扫描的内存下限：
        // threads=1 要 40 MB、threads=2 要 80 MB、threads=4 读两个月文件要 160 MB。
        // 原先 `512MB`（DuckDB 按 10⁶ 算，实为 488 MiB）配 threads=4、room_concurrency=10，
        // 峰值 (10+4)×32 = 448 MiB 贴着上限 —— 实跑在 362.5 MiB 上就有群 OOM 失败。
        // 现在 (10+2)×32 = 384 MiB，对 1GB（= 953 MiB）留 2.5 倍余量。
        //
        // **扫描内部并行不值得买**：单群读取是毫秒级（138 个群并发 10 跑完 0.4s），
        // 而它读完要等的模型调用是分钟级。threads 只是内存乘数，不是产能旋钮。
        "SET autoinstall_known_extensions = false; SET autoload_known_extensions = false; \
         SET memory_limit = '1GB'; SET threads = 2; SET max_temp_directory_size = '1GB';",
    )
    .expect("关闭扩展自动加载");
    Mutex::new(con)
});

/// 列名即领域名。上游字段名只允许出现在这里。
///
/// `schemaVersion` / `parserVersion` 显式 `CAST ... AS BIGINT`：read_json_auto
/// 推出来的整数宽度跟着样本走，取值端要一个定死的物理类型。
///
/// **为什么是 `macro_rules!` 而不是 `const`**：这样下面那个 `format!` 能在**编译期**
/// 校验剩下那几个运行期占位符（`{tz}` / `{files}` / 窗口两端）。换成
/// `const` + `.replace("{since}", …)` 的话，占位符打错一个字母会原样带进 SQL、到
/// DuckDB 才报解析错；而且 `.replace` 有先后顺序 —— 先插进去的内容会被后面几次
/// replace 再扫一遍。
///
/// ⚠️ **领域列名写字面量，不再各起一个 `COL_*` 常量。** 那 12 个常量的理由是
/// 「打错一个字母是编译错误，而不是运行期 `InvalidColumnName`」，可 `ingest/tests.rs`
/// 本来就在真文件上执行这条 SQL —— 打错一个字母 `cargo test` 两秒内就红。
/// 12 个常量 + 12 个具名参数买到的只是把「测试第 2 秒失败」提前成「编译失败」，
/// 代价是这条 SELECT 读起来不再像 SQL。
macro_rules! select_sql {
    () => {
        r#"
WITH src AS (
    SELECT
        sourceMessageId                                  AS msg_id,
        officialRoomId                                   AS room,
        corpId                                           AS corp,
        -- messageTime 是毫秒，×1000 转微秒、加上本地时区偏移，直接得本地 TIMESTAMP。
        -- make_timestamp 是 core 函数，不碰 ICU（见 TZ_OFFSET_MICROS）
        make_timestamp(messageTime * 1000 + {tz_offset})     AS "at",
        sender.easyUserId                                AS sender_id,
        sender.identityType                              AS sender_role,
        NULLIF(json_extract_string(to_json(sender), '$.officialUserId'), '') AS official_user_id,
        COALESCE(NULLIF(analysisText, ''), content)      AS text,
        semanticPayload.replyTo.sourceMessageId          AS reply_to,
        CAST(schemaVersion AS BIGINT)                    AS schema_version,
        CAST(parserVersion AS BIGINT)                    AS parser_version,
        standardType                                     AS upstream_type,
        filename                                         AS src_file
    FROM read_json_auto([{files}], format='newline_delimited', filename=true)
)
SELECT msg_id, room, corp, "at", sender_id, sender_role, official_user_id, text, reply_to,
       schema_version, parser_version, upstream_type, src_file
FROM src
-- 缺时间的行无法判断是否属于窗口，必须交给必填守卫，不能静默过滤。
WHERE "at" IS NULL OR CAST("at" AS DATE) BETWEEN DATE '{since}' AND DATE '{until}'
ORDER BY "at", msg_id
"#
    };
}

/// 一次查询，边取行边变成领域对象，路上守五件事。文件列表为空直接返回空。
///
/// 连接从共享实例 `try_clone` 出来、用完即弃（见 [`DB`]）—— 换来的是「从任意线程
/// 调用都安全」，又不必每个群新建一个带 12 条工作线程的数据库实例。
///
/// 过滤、投影、排序全下推给 DuckDB；这里只做「行 → 领域对象」和守卫，
/// **不把整表拉进内存再筛**。
///
/// ⚠️ **「不筛」是真的，「流式」不是。** `Statement::query` 走的是
/// `duckdb_execute_prepared`（**物化**入口，不是 `execute_streaming`），所以
/// `rows.next()` 吐第一行之前，过滤+排序后的**整个结果集**已经在 DuckDB 内存里了 ——
/// 本函数内的峰值实为两份（DuckDB 那份 ＋ 正在长的 `out`），都在返回时释放。
/// 换流式 API 也救不了：`ORDER BY "at", msg_id` 本身就是阻塞算子，排完才有第一行。
/// 谓词确实下推了（窗口外的行根本不进结果集），那才是硬规则要的东西。
fn scan(files: &[(String, PathBuf)], w: &Window, corp: &str, room: &str) -> Result<Vec<Message>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    // 路径原样进 SQL，DuckDB 的 `filename` 列原样还回来 —— 于是「这一行来自哪个月」
    // 查一次表就有了，不用把路径拆开。每群最多两个月，这张表就两项。
    let month_of: BTreeMap<String, &str> = files
        .iter()
        .map(|(m, p)| (p.display().to_string(), m.as_str()))
        .collect();
    let quoted = month_of
        .keys()
        .map(|p| format!("'{}'", p.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    // 窗口的两端是我们自己格式化出来的日期，直接进 SQL —— 这条 SQL 里
    // **没有任何来自外部的值**（`read_by_ids` 删掉后，msg_id 那条绑定也没了）。
    let sql = format!(
        select_sql!(),
        tz_offset = TZ_OFFSET_MICROS,
        files = quoted,
        since = w.since(),
        until = w.until(),
    );

    // 共享实例上开一条连接（见 [`DB`]）。锁只圈这一下，查询不在临界区里。
    let con = DB.lock().expect("锁内只有 try_clone").try_clone()?;
    let mut stmt = con.prepare(&sql)?;
    let mut rows = stmt.query([])?;

    let mut out: Vec<Message> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut dupes = 0usize;
    let mut undated: Option<(usize, String)> = None;
    let mut bytes = 0usize;

    while let Some(r) = rows.next()? {
        let Some(m) = message_from_row(r, &month_of, corp, room)? else {
            // 守卫②a：缺 `messageTime` 的行跳过，每群汇总一行 warn（见那条守卫）。
            let slot = undated.get_or_insert_with(|| (0, r.get(12).unwrap_or_default()));
            slot.0 += 1;
            continue;
        };

        // ⑤ 去重 —— raw 只增不删、上游可能重复投递，同一 msg_id 出现两次是真实会
        //    发生的。行已按 at 升序，所以首次出现的就是时间最早的那条。
        if !seen.insert(m.msg_id.clone()) {
            dupes += 1;
            continue;
        }
        bytes += std::mem::size_of::<Message>()
            + m.msg_id.len() * 2
            + m.room.len()
            + m.corp.len()
            + m.sender_id.len()
            + m.text.len()
            + m.reply_to.as_ref().map_or(0, String::len)
            + m.official_user_id.as_ref().map_or(0, String::len);
        if bytes > 32 * 1024 * 1024 {
            return Err(IngestError::Room(
                "单群会话超过 32 MiB 读取预算，停止整群处理而非截断消息".into(),
            ));
        }
        out.push(m);
    }

    if dupes > 0 {
        // 静默去重 = 不知道自己在丢东西。重复是上游噪声不是数据损坏，不必整群失败。
        tracing::warn!(room, dupes, "去掉重复 msg_id");
    }
    if let Some((undated, src_file)) = undated {
        // **每群汇总一行，不逐条打。** 上游真出问题时坏行是成批的，逐条打会把日志
        // 淹掉，而淹掉的日志等于没有（同 `download.rs` 那条教训）。带首条的文件名，
        // 够定位到是哪个月文件。**不带正文** —— 这条路上 `redact::body` 没跑过。
        tracing::warn!(room, undated, src_file, "跳过缺 messageTime 的消息");
    }
    Ok(out)
}

/// 一行 → 领域 [`Message`]，路上守四件事（①版本 / ②必填＋②b角色 / ③归属 / ④月份）。
///
/// 从 [`scan`] 拆出来，是为了让「SQL 构造 · DuckDB 交互 · 守卫」不再叠在一个 175 行的
/// 函数里 —— 守卫这一层的错误分类（`Upstream` 整轮死 / `Room` 该群跳过）是承重的。
/// 守卫⑤（去重）是跨行状态，留在 `scan` 的循环里。
///
/// `Ok(None)` = **这一行该跳过**。只有守卫②a（缺 `messageTime`）会产出它。
fn message_from_row(
    r: &duckdb::Row<'_>,
    month_of: &BTreeMap<String, &str>,
    corp: &str,
    room: &str,
) -> Result<Option<Message>> {
    // 按**列名**取，不按下标 —— 列名即领域名，那个名字必须在读取点也成立。
    // 下标错位在这里是静默的（12 列有 8 列都是字符串，互换类型兼容、编译通过、
    // 守卫也放行）；列名打错则是运行期 `InvalidColumnName`，而 `tests.rs` 在真文件上
    // 跑这条 SQL，两秒内就红。
    let msg_id: String = r.get::<_, Option<String>>("msg_id")?.unwrap_or_default();
    let r_room: String = r.get::<_, Option<String>>("room")?.unwrap_or_default();
    let r_corp: String = r.get::<_, Option<String>>("corp")?.unwrap_or_default();
    let at: Option<NaiveDateTime> = r.get("at")?;
    let sender_id: String = r.get::<_, Option<String>>("sender_id")?.unwrap_or_default();
    let raw_role: Option<String> = r.get("sender_role")?;
    let text: String = r.get::<_, Option<String>>("text")?.unwrap_or_default();
    let reply_to: Option<String> = r.get("reply_to")?;
    let schema_v: Option<i64> = r.get("schema_version")?;
    let parser_v: Option<i64> = r.get("parser_version")?;
    let upstream_type: Option<String> = r.get("upstream_type")?;
    let src_file: String = r.get::<_, Option<String>>("src_file")?.unwrap_or_default();

    // ① 上游版本 —— 不匹配是**整轮**的事（上游解析器变了），不是某个群的事
    if schema_v != Some(SCHEMA_VERSION) || parser_v != Some(PARSER_VERSION) {
        return Err(IngestError::Upstream(format!(
            "{src_file}: schemaVersion/parserVersion = {schema_v:?}/{parser_v:?}，\
             期望 {SCHEMA_VERSION}/{PARSER_VERSION}。不做兼容层，直接停。"
        )));
    }

    // ②a 缺时间 —— **跳过这一行，不是该群失败。**
    //
    // ⚠️ 这一支是从守卫②里**拆出来的，判据是影响面不是严重程度**：读取 SQL 的
    // `WHERE "at" IS NULL OR CAST("at" AS DATE) BETWEEN …` 只放行这一类穿过窗口过滤，
    // 而 `read_json_auto` 扫的是**整个月文件**。合起来的后果是：月内**任何一天**的
    // 一条缺时间的行，都会让这个群**整月每天**失败 —— 含窗口早就滑过去、已经冻结的天。
    // 冻结区不会再被重抽，那些天的 unknown 是**永久的**。
    //
    // 而一条缺 `messageTime` 的消息本来就进不了任何 `Conversation` 窗口（窗口按时间切），
    // 让它掀翻一个群的整月数据，是护栏打在了错误的地方。
    //
    // ⚠️ **下面那三个必填字段不拆**：它们的 `at` 有值，已经被 SQL 的 `BETWEEN` 夹在
    // 窗口内 —— 能走到那里的必然在窗口内，显式失败是对的。别把这两支合回去。
    let Some(at) = at else {
        return Ok(None);
    };

    // ② 必填字段 —— 缺了就是该群失败：承重不变量 6（溯源）在这个群上已经站不住。
    //
    // `text` 也在这一组里：契约头一条就是「`text` **恒非空**」，而它是 COALESCE
    // 兜完底的结果 —— 到这儿还是空，说明上游连占位符都没给，是上游形状变了。
    // NULL 和空串都落到这里：`content` 本身是空串时 COALESCE 返回的就是空串
    // 而不是 NULL，只判 NULL 漏得掉。
    if msg_id.is_empty() || sender_id.is_empty() || text.is_empty() {
        // ⚠️ **正文只报有无，不报内容。** 这条错误的去处是
        //    `daily::tally` 的 `tracing::error!` → `run.log`，而**这条路径上
        //    `redact::body` 根本没跑过** —— 照抄 `text` 等于把未脱敏的客户消息
        //    （手机号 / 门牌号 / 姓名）写进日志。而它的触发条件恰好是「上游字段
        //    形状变了」，最可能真发生的那一种。
        //    诊断要的是**缺了哪个字段**，那几个都是标识符，不是正文。
        return Err(IngestError::Room(format!(
            "{src_file}: 有消息缺必填字段 \
             (msg_id={msg_id:?} sender_id={sender_id:?} at={at} text={})",
            if text.is_empty() { "<空>" } else { "<非空>" }
        )));
    }

    // ②b 角色 —— 认不出的 identityType 是该群失败，**不兜底成任意一边**
    //     （理由见 [`Role::parse`]）。上游加一个新的身份类型时，这里会当场喊，
    //     而不是让它默默按某一边参与指标计算。
    let Some(sender_role) = raw_role.as_deref().and_then(Role::parse) else {
        return Err(IngestError::Room(format!(
            "{src_file}: 认不出的 identityType {raw_role:?}，只接受 INTERNAL / EXTERNAL"
        )));
    };

    // ③ 文件放对了没有 —— 内容里的标识必须和路径一致
    if r_corp != corp || r_room != room {
        return Err(IngestError::Room(format!(
            "{src_file}: 内容是 {r_corp}/{r_room}，路径却是 {corp}/{room}"
        )));
    }

    // ④ 月份守卫 —— 索引表说 file_month 是「消息月份」。若上游其实按接收时间
    //    分月，8/31 深夜的消息会落进 9 月文件，而我们读 {202608} 就会**静默漏掉**。
    //    这里只能证伪（读到的文件里出现了别的月份），证伪即显式失败。
    //    月份由 `files()` 一路带进来，不从路径反解 —— 布局只被「拼」一次。
    // ponytail: 只查读到的行；跨月窗口本来就读两个月文件，实际敞口≈0。
    //           真要根治得让 mirror 认识 messageTime，那会破坏「上游字段名只在这里」。
    let Some(file_month) = month_of.get(src_file.as_str()) else {
        return Err(IngestError::Room(format!(
            "{src_file}: 不在本次要读的文件列表里 —— \
             DuckDB 回的 filename 和传进去的路径对不上了"
        )));
    };
    if at.format(MONTH_FMT).to_string() != *file_month {
        return Err(IngestError::Room(format!(
            "{src_file}: 消息时间 {at} 不在文件所属月份 {file_month} 内 —— \
             上游 file_month 不是按消息月份分的，读取窗口的月份集合不再可靠"
        )));
    }

    // 毒化只可能来自别的线程持锁时 panic —— 纯日志装饰品，那种情况下少打
    // 一条日志无所谓，不值得让它反过来掀翻这个群（is_ok_and 而非 unwrap）。
    if let Some(t) = &upstream_type
        && !KNOWN_TYPES.contains(&t.as_str())
        && SEEN_UNKNOWN.lock().is_ok_and(|mut s| s.insert(t.clone()))
    {
        tracing::info!(
            room,
            r#type = t,
            "遇到样本外的消息类型，原样通过（正文走 content 兜底）"
        );
    }

    Ok(Some(Message {
        msg_id,
        room: r_room,
        corp: r_corp,
        at,
        sender_id,
        sender_role,
        official_user_id: if sender_role == Role::Internal {
            r.get("official_user_id")?
        } else {
            None
        },
        // 原样带走，不在这里判「算不算文本」—— 那条判定住在 `Message::placeholder`。
        msg_type: upstream_type,
        text,
        reply_to,
    }))
}

fn counts(msgs: &[Message]) -> BTreeMap<NaiveDate, (usize, usize)> {
    let mut senders: BTreeMap<NaiveDate, HashSet<&str>> = BTreeMap::new();
    let mut n: BTreeMap<NaiveDate, usize> = BTreeMap::new();
    for m in msgs {
        let d = m.at.date();
        *n.entry(d).or_default() += 1;
        senders.entry(d).or_default().insert(&m.sender_id);
    }
    n.into_iter()
        .map(|(d, c)| (d, (c, senders[&d].len())))
        .collect()
}

/// 每天每个平台客服发了多少条。**只数 `Role::Internal`** —— 商家侧那半边已经
/// 计进 [`counts`] 的群级消息数，这一份的用途是「对冲只看处理量」，商家进来就废了。
///
/// 和 [`counts`] 分成两个函数而不是一趟出两个 map：`msgs` 早在内存里，多走一遍
/// 的代价是零，而合起来的返回类型是个双 map 元组，两个调用点都要解包。
fn agent_counts(msgs: &[Message]) -> BTreeMap<(NaiveDate, String), usize> {
    let mut n: BTreeMap<(NaiveDate, String), usize> = BTreeMap::new();
    for m in msgs.iter().filter(|m| m.sender_role == Role::Internal) {
        *n.entry((m.at.date(), m.sender_id.clone())).or_default() += 1;
    }
    n
}

/// 一个群在窗口内的完整会话，按 `at` 升序。一次只在内存里持有一个群。
///
/// ⚠️ **窗口过滤按消息真实时间 `at` 做，不只靠路径。** 一个月文件里装着整月，
/// 路径只用来收窄 I/O，正确性靠 SQL 里那个 `BETWEEN`。
pub fn read_room(raw_root: &Path, corp: &str, room: &str, w: &Window) -> Result<Conversation> {
    let msgs = scan(&files(raw_root, corp, room, w), w, corp, room)?;
    Ok(Conversation {
        msg_counts: counts(&msgs),
        agent_msg_counts: agent_counts(&msgs),
        msgs,
    })
}

/// 跑批读取本轮索引确认的月份；其他历史文件仅供本地检查和原文下钻。
pub(crate) fn read_synced_room(
    raw_root: &Path,
    corp: &str,
    room: &str,
    w: &Window,
    months: &[String],
) -> Result<Conversation> {
    let msgs = scan(&synced_files(raw_root, corp, room, months), w, corp, room)?;
    Ok(Conversation {
        msg_counts: counts(&msgs),
        agent_msg_counts: agent_counts(&msgs),
        msgs,
    })
}
