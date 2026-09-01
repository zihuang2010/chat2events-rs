//! 读 —— **DuckDB 连接 · SQL · 上游字段语义三样只住这个文件**。
//! 上游 camelCase 字段名只允许出现在下面那条 `SELECT ... AS ...` 里。
//!
//! 端口的三个读函数里有两个在这里（[`read_room`] / [`read_by_ids`]），
//! `list_rooms` 只遍历目录、不碰 DuckDB，所以住在 `layout.rs`。

use super::{
    layout::{MONTH_FMT, files},
    types::{Conversation, IngestError, Message, Result, Role},
};
use crate::window::Window;
use chrono::{NaiveDate, NaiveDateTime};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

const SCHEMA_VERSION: i64 = 1;
const PARSER_VERSION: i64 = 1;

/// 归属日按业务本地时区算，不按 UTC。跨零点的归属会差一天。
pub(super) const TZ: &str = "Asia/Shanghai";

/// 样本里出现过的类型。**不做过滤** —— 「什么算业务事件」是 ③ 的活，
/// 这里只负责把没见过的类型吼一声，好让真实数据自己告诉我们还有什么。
const KNOWN_TYPES: [&str; 4] = ["TEXT", "IMAGE", "GIF", "VIDEO"];

/// 只为日志去重，不参与任何逻辑。进程级，跑完即退出。
static SEEN_UNKNOWN: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// 领域列名 —— `select_sql!` 的 SELECT 列表和 [`scan`] 的取值点引用**同一组常量**。
/// 曾经 12 个列名各写两遍（SQL 一遍、`r.get("…")` 一遍），必须逐字相同才能跑通，
/// 编译期零校验；现在打错一个字母是编译错误，不再是运行期 `InvalidColumnName`。
const COL_MSG_ID: &str = "msg_id";
const COL_ROOM: &str = "room";
const COL_CORP: &str = "corp";
const COL_AT: &str = "at";
const COL_SENDER_ID: &str = "sender_id";
const COL_SENDER_ROLE: &str = "sender_role";
const COL_TEXT: &str = "text";
const COL_REPLY_TO: &str = "reply_to";
const COL_SCHEMA_VERSION: &str = "schema_version";
const COL_PARSER_VERSION: &str = "parser_version";
const COL_UPSTREAM_TYPE: &str = "upstream_type";
const COL_SRC_FILE: &str = "src_file";

/// 列名即领域名。上游字段名只允许出现在这里。
///
/// `schemaVersion` / `parserVersion` 显式 `CAST ... AS BIGINT`：read_json_auto
/// 推出来的整数宽度跟着样本走，取值端要一个定死的物理类型。
///
/// **为什么是 `macro_rules!` 而不是 `const`**：这样下面那个 `format!` 能在**编译期**
/// 校验全部占位符。换成 `const` + `.replace("{since}", …)` 的话，占位符打错一个字母
/// 会原样带进 SQL、到 DuckDB 才报解析错；而且 `.replace` 是有先后顺序的 ——
/// 先插进去的内容会被后面几次 replace 再扫一遍。
/// SQL 仍然是文件顶上一个具名的东西，「字段名只出现在这里」这条约束一字未变。
macro_rules! select_sql {
    () => {
        r#"
WITH src AS (
    SELECT
        sourceMessageId                                  AS {msg_id},
        officialRoomId                                   AS {room},
        corpId                                           AS {corp},
        -- messageTime 是毫秒，/1000 转秒再喂 to_timestamp
        to_timestamp(messageTime / 1000) AT TIME ZONE '{tz}'  AS "{at}",
        sender.easyUserId                                AS {sender_id},
        sender.identityType                              AS {sender_role},
        COALESCE(NULLIF(analysisText, ''), content)      AS {text},
        semanticPayload.replyTo.sourceMessageId          AS {reply_to},
        CAST(schemaVersion AS BIGINT)                    AS {schema_version},
        CAST(parserVersion AS BIGINT)                    AS {parser_version},
        standardType                                     AS {upstream_type},
        filename                                         AS {src_file}
    FROM read_json_auto([{files}], format='newline_delimited', filename=true)
)
SELECT {msg_id}, {room}, {corp}, "{at}", {sender_id}, {sender_role}, {text}, {reply_to},
       {schema_version}, {parser_version}, {upstream_type}, {src_file}
FROM src
WHERE CAST("{at}" AS DATE) BETWEEN DATE '{since}' AND DATE '{until}'{extra}
ORDER BY "{at}", {msg_id}
"#
    };
}

/// 一次查询，边取行边变成领域对象，路上守五件事。文件列表为空直接返回空。
///
/// 连接用完即弃 —— `Connection::open_in_memory()` 只有毫秒级开销，换来的是
/// 「从任意线程调用都安全」，不用给每个线程发一个 cursor。
///
/// 过滤、投影、排序全下推给 DuckDB；这里只做「行 → 领域对象」和守卫，
/// **不把整表拉进内存再筛**。
fn scan(
    files: &[(String, PathBuf)],
    w: &Window,
    ids: Option<&[String]>,
    corp: &str,
    room: &str,
) -> Result<Vec<Message>> {
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
    // 窗口的两端是我们自己格式化出来的日期，直接进 SQL；msg_id 来自外部（webUI
    // 下钻），走 `?` 绑定。
    let extra = match ids {
        Some(ids) => format!(" AND {COL_MSG_ID} IN ({})", vec!["?"; ids.len()].join(", ")),
        None => String::new(),
    };
    let sql = format!(
        select_sql!(),
        tz = TZ,
        files = quoted,
        since = w.since(),
        until = w.until(),
        extra = extra,
        msg_id = COL_MSG_ID,
        room = COL_ROOM,
        corp = COL_CORP,
        at = COL_AT,
        sender_id = COL_SENDER_ID,
        sender_role = COL_SENDER_ROLE,
        text = COL_TEXT,
        reply_to = COL_REPLY_TO,
        schema_version = COL_SCHEMA_VERSION,
        parser_version = COL_PARSER_VERSION,
        upstream_type = COL_UPSTREAM_TYPE,
        src_file = COL_SRC_FILE,
    );

    let con = duckdb::Connection::open_in_memory()?;
    let mut stmt = con.prepare(&sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(ids.unwrap_or_default()))?;

    let mut out: Vec<Message> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut dupes = 0usize;

    while let Some(r) = rows.next()? {
        let m = message_from_row(r, &month_of, corp, room)?;

        // ⑤ 去重 —— raw 只增不删、上游可能重复投递，同一 msg_id 出现两次是真实会
        //    发生的。行已按 at 升序，所以首次出现的就是时间最早的那条。
        if !seen.insert(m.msg_id.clone()) {
            dupes += 1;
            continue;
        }
        out.push(m);
    }

    if dupes > 0 {
        // 静默去重 = 不知道自己在丢东西。重复是上游噪声不是数据损坏，不必整群失败。
        tracing::warn!(room, dupes, "去掉重复 msg_id");
    }
    Ok(out)
}

/// 一行 → 领域 [`Message`]，路上守四件事（①版本 / ②必填＋②b角色 / ③归属 / ④月份）。
///
/// 从 [`scan`] 拆出来，是为了让「SQL 构造 · DuckDB 交互 · 守卫」不再叠在一个 175 行的
/// 函数里 —— 守卫这一层的错误分类（`Upstream` 整轮死 / `Room` 该群跳过）是承重的。
/// 守卫⑤（去重）是跨行状态，留在 `scan` 的循环里。
fn message_from_row(
    r: &duckdb::Row<'_>,
    month_of: &BTreeMap<String, &str>,
    corp: &str,
    room: &str,
) -> Result<Message> {
    // 按**列名**取，不按下标 —— 列名即领域名，那个名字必须在读取点也成立。
    // 下标错位在这里是静默的（12 列有 8 列都是字符串，互换类型兼容、编译通过、
    // 守卫也放行）；列名与 SQL 引用同一组 COL_* 常量，打错是编译错误。
    let msg_id: String = r.get::<_, Option<String>>(COL_MSG_ID)?.unwrap_or_default();
    let r_room: String = r.get::<_, Option<String>>(COL_ROOM)?.unwrap_or_default();
    let r_corp: String = r.get::<_, Option<String>>(COL_CORP)?.unwrap_or_default();
    let at: Option<NaiveDateTime> = r.get(COL_AT)?;
    let sender_id: String = r
        .get::<_, Option<String>>(COL_SENDER_ID)?
        .unwrap_or_default();
    let raw_role: Option<String> = r.get(COL_SENDER_ROLE)?;
    let text: String = r.get::<_, Option<String>>(COL_TEXT)?.unwrap_or_default();
    let reply_to: Option<String> = r.get(COL_REPLY_TO)?;
    let schema_v: Option<i64> = r.get(COL_SCHEMA_VERSION)?;
    let parser_v: Option<i64> = r.get(COL_PARSER_VERSION)?;
    let upstream_type: Option<String> = r.get(COL_UPSTREAM_TYPE)?;
    let src_file: String = r
        .get::<_, Option<String>>(COL_SRC_FILE)?
        .unwrap_or_default();

    // ① 上游版本 —— 不匹配是**整轮**的事（上游解析器变了），不是某个群的事
    if schema_v != Some(SCHEMA_VERSION) || parser_v != Some(PARSER_VERSION) {
        return Err(IngestError::Upstream(format!(
            "{src_file}: schemaVersion/parserVersion = {schema_v:?}/{parser_v:?}，\
             期望 {SCHEMA_VERSION}/{PARSER_VERSION}。不做兼容层，直接停。"
        )));
    }

    // ② 必填字段 —— 缺了就是该群失败：承重不变量 6（溯源）在这个群上已经站不住。
    //
    // `text` 也在这一组里：契约头一条就是「`text` **恒非空**」，而它是 COALESCE
    // 兜完底的结果 —— 到这儿还是空，说明上游连占位符都没给，是上游形状变了。
    // NULL 和空串都落到这里：`content` 本身是空串时 COALESCE 返回的就是空串
    // 而不是 NULL，只判 NULL 漏得掉。
    let Some(at) = at.filter(|_| !msg_id.is_empty() && !sender_id.is_empty() && !text.is_empty())
    else {
        return Err(IngestError::Room(format!(
            "{src_file}: 有消息缺必填字段 \
             (msg_id={msg_id:?} sender_id={sender_id:?} text={text:?} at={at:?})"
        )));
    };

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

    Ok(Message {
        msg_id,
        room: r_room,
        corp: r_corp,
        at,
        sender_id,
        sender_role,
        text,
        reply_to,
    })
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

/// 一个群在窗口内的完整会话，按 `at` 升序。一次只在内存里持有一个群。
///
/// ⚠️ **窗口过滤按消息真实时间 `at` 做，不只靠路径。** 一个月文件里装着整月，
/// 路径只用来收窄 I/O，正确性靠 SQL 里那个 `BETWEEN`。
pub fn read_room(raw_root: &Path, corp: &str, room: &str, w: &Window) -> Result<Conversation> {
    let msgs = scan(&files(raw_root, corp, room, w), w, None, corp, room)?;
    Ok(Conversation {
        corp: corp.to_string(),
        room: room.to_string(),
        msg_counts: counts(&msgs),
        msgs,
    })
}

/// 按 `msg_id` 取原文，供 webUI 下钻。返回按 `at` 升序。
///
/// ⚠️ 窗口必须覆盖 `[occurred_on, date(last_msg_time)]`（`Window::span`）——
/// 一个事件的来源消息可以跨天（甚至跨月），只按归属日收窄会漏。
///
/// **给不全时不静默返回少的**，显式报出缺哪些 ID —— 主管核实首响时看到的条数
/// 对不上，比看到报错更糟。
// 唯一的读取点是 webUI 的下钻，那条旁路还没搬过来
pub fn read_by_ids(
    raw_root: &Path,
    corp: &str,
    room: &str,
    w: &Window,
    ids: &[String],
) -> Result<Vec<Message>> {
    let mut want: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if seen.insert(id.as_str()) {
            want.push(id.clone());
        }
    }
    if want.is_empty() {
        return Ok(Vec::new());
    }
    let msgs = scan(&files(raw_root, corp, room, w), w, Some(&want), corp, room)?;
    let got: HashSet<&str> = msgs.iter().map(|m| m.msg_id.as_str()).collect();
    let missing: BTreeSet<&String> = want
        .iter()
        .filter(|id| !got.contains(id.as_str()))
        .collect();
    if !missing.is_empty() {
        return Err(IngestError::Missing(format!(
            "{corp}/{room} 在 {}~{} 取不到 {} 个 msg_id：{missing:?}",
            w.since(),
            w.until(),
            missing.len()
        )));
    }
    Ok(msgs)
}
