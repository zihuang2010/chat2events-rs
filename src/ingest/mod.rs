//! ① 摄取 ingest ＋ ② 会话 conversation —— 唯一知道上游字段名和路径布局的地方。
//!
//! 端口（换数据源那天：新写一个文件实现这三个函数，其余阶段一行不动）：
//!
//! ```text
//! list_rooms(raw_root, window)                    -> [(corp, room)]
//! read_room(raw_root, corp, room, window)         -> Conversation
//! read_by_ids(raw_root, corp, room, window, ids)  -> [Message]
//! ```
//!
//! **契约**（这段文档注释就是「端口」本身 —— 不写 trait，判据是「一个适配器 =
//! 假想接缝」。真出现第二个数据源那天再提取，那时契约已被验证过）：
//!
//!   * `Message.text` **恒非空**，是这条消息可读的正文；非文本消息给
//!     `[图片消息]` 这样的占位符，保上下文连贯。
//!     「`analysisText` 可能是空串」是**上游的形状**，兜底做在下面的 SELECT 里，
//!     领域里只有一个文本字段。
//!   * `Message.at` 是**业务本地时区**的时间戳，不是 UTC。
//!   * `read_room` 返回的 `msgs` 按 `at` 升序，`corp` / `room` 唯一，
//!     且同一个 `msg_id` **只出现一次**。
//!   * 上游一行读不出必填字段（`msg_id` / `sender_id` / `at` / `text`）→ **该群失败**
//!     （[`IngestError::Room`]），不丢弃、不兜底。丢弃等于用残缺数据覆盖完整数据。
//!   * `schemaVersion` / `parserVersion` 不匹配 → **整轮失败退出**
//!     （[`IngestError::Upstream`]），不做兼容层。
//!
//! **四样东西不出本文件**：DuckDB 连接 · SQL · 路径布局 · 上游字段语义。
//! 上游 camelCase 字段名只允许出现在下面那条 `SELECT ... AS ...` 里。
//!
//! 本地布局是 OSS 的**字节级镜像**（一个群一个月一个文件，见 ADR-0005）：
//!
//! ```text
//! <raw_root>/<yyyyMM>/<corpId>/<officialRoomId>.ndjson
//! ```
//!
//! 所以「已经拉到第几字节」= 文件大小，不需要任何额外的状态存储。

use crate::window::Window;
use chrono::{NaiveDate, NaiveDateTime};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt, fs,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

const SCHEMA_VERSION: i64 = 1;
const PARSER_VERSION: i64 = 1;

/// 归属日按业务本地时区算，不按 UTC。跨零点的归属会差一天。
const TZ: &str = "Asia/Shanghai";

/// 样本里出现过的类型。**不做过滤** —— 「什么算业务事件」是 ③ 的活，
/// 这里只负责把没见过的类型吼一声，好让真实数据自己告诉我们还有什么。
const KNOWN_TYPES: [&str; 4] = ["TEXT", "IMAGE", "GIF", "VIDEO"];

/// 只为日志去重，不参与任何逻辑。进程级，跑完即退出。
static SEEN_UNKNOWN: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// 路径布局的两个字面量。只有 [`room_path`] 和 [`months`] 该用到它们 ——
/// 布局是**拼**出来的，没有第二处再去**拆**它。
const EXT: &str = "ndjson";
const MONTH_FMT: &str = "%Y%m";

/// 领域列名 —— [`select_sql!`] 的 SELECT 列表和 [`scan`] 的取值点引用**同一组常量**。
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

/// 群里的两方，**都是客服**（`CONTEXT.md`：群里没有终端消费者）。
/// 上游 `identityType` 只有这两个值，实测 100% 填充。
///
/// **收成枚举而不是裸字符串，是因为错法是静默的**：`== "INTERNAL"` 打错一个字母，
/// [`crate::extract`] 的 `labels` 会把平台客服全标成「商家X」、`assemble` 的 `agents`
/// 恒空、`first_agent_reply_time` 恒 `None`、⑥ 的首响 p50/p90 全 `NULL` ——
/// 三条链路一起坏，而编译器一句话都不说，报表只是安静地偏小。
///
/// 「上游只有这两个值」这条契约从此**只在 [`Role::parse`] 那一处成立一次**，
/// 下游全部是 `match`，打错是编译错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 平台客服 —— 受理诉求、协调师傅上门。指标口径里的 `agent` 就是这一边。
    Internal,
    /// 商家客服 —— 把订单诉求发到群里。事件多数由这一边发起。
    External,
}

impl Role {
    /// 上游 `identityType` -> 领域角色。**唯一的解析点**（[`scan`] 的取值处）。
    ///
    /// 认不出就是 `None`，由调用方判该群失败 —— **不能兜底成任意一边**：
    /// 判成 `Internal` 会把商家算进 `agents`，判成 `External` 会让平台的回复不再算首响，
    /// 两个方向都是静默把指标写歪。
    fn parse(s: &str) -> Option<Self> {
        match s {
            "INTERNAL" => Some(Self::Internal),
            "EXTERNAL" => Some(Self::External),
            _ => None,
        }
    }

    /// 落库用。与上游 `identityType` 的取值**逐字相同** —— `event.asker_role`
    /// 那一列存的就是它，库里的历史数据和新写进去的必须对得上。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "INTERNAL",
            Self::External => "EXTERNAL",
        }
    }
}

/// 八个字段，每一个都有读取点。
///
/// 端口上每多一个死字段，就是向未来每一个适配器收一次税 —— `msg_type` /
/// `mentions` / `plain_text` 曾经在这里，读取点分别是 0 / 0 / 只当兜底。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_id: String,
    pub room: String,
    pub corp: String,
    pub at: NaiveDateTime,
    pub sender_id: String,
    pub sender_role: Role,
    pub text: String,
    pub reply_to: Option<String>,
}

/// **接口粒度 = 群 × 一次运行的完整会话 = 失败隔离粒度 = ③ 的输入。** 四者必须相等。
#[derive(Debug, Clone)]
// ③⑦ 已经搬完，但两个字段仍然没有读取点：`daily::run_room` 一路带着自己的
// `corp` / `room` 参数，`Event` 的那两列来自 `Message`。保留它们是因为
// `CONTEXT.md` 的领域契约里 `Conversation` 就是这个形状，且 webUI 下钻
// （唯一还没搬的旁路）拿到 `Conversation` 时要用。
#[allow(dead_code)]
pub struct Conversation {
    pub corp: String,
    pub room: String,
    pub msgs: Vec<Message>,
    /// 每天 (消息条数, 去重发言人数)。搭 `msgs` 的同一趟车算出来 ——
    /// 这个群的消息本来就已经在内存里（③ 要用），这里没有多读一个字节。
    /// 副作用：⑥ 指标从此零 IO、零 SQL、零 duckdb，是纯函数模块。
    pub msg_counts: BTreeMap<NaiveDate, (usize, usize)>,
}

/// 三种失败的**处置方式不同**，所以必须在类型上分开：
/// `Upstream` 整轮退出、`Room` 该群跳过一行不写（承重不变量 3）、`Missing` 是下钻的事。
#[derive(Debug)]
pub enum IngestError {
    /// 上游解析器变了 —— 整轮失败退出，不做兼容层。
    Upstream(String),
    /// 该群失败：整体跳过、一行不写、记 `run_failure`。
    Room(String),
    /// 下钻取不全 —— 显式报出缺哪些，不静默少返回。
    Missing(String),
    Db(duckdb::Error),
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upstream(m) | Self::Room(m) | Self::Missing(m) => f.write_str(m),
            Self::Db(e) => write!(f, "DuckDB: {e}"),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<duckdb::Error> for IngestError {
    fn from(e: duckdb::Error) -> Self {
        Self::Db(e)
    }
}

type Result<T> = std::result::Result<T, IngestError>;

// ─────────────────────────────────────────────────────────────────────────────
// 路径布局 —— 只有本文件知道。`pull` 写文件时也问这里要路径，两边必须同一个函数。
// ─────────────────────────────────────────────────────────────────────────────

/// 窗口覆盖的月份。每月头几天窗口会跨月，那时这里返回两个。
///
/// 上游按**消息月份**分文件（索引表 `file_month` 的注释），所以窗口里每一天
/// 的消息都落在 `months(w)` 这几个文件里，不需要额外余量。
/// 这条前提由 `scan` 的守卫④兜着 —— 不成立就是显式失败。
pub fn months(w: &Window) -> Vec<String> {
    // 窗口天数升序（Window 构造保证）⇒ 格式化出的月份天然有序，只需去重
    let mut m: Vec<String> = w.days().iter().map(|d| d.format(MONTH_FMT).to_string()).collect();
    m.dedup();
    m
}

pub fn room_path(raw_root: &Path, month: &str, corp: &str, room: &str) -> PathBuf {
    raw_root.join(month).join(corp).join(format!("{room}.{EXT}"))
}

/// 窗口覆盖的月份里有文件的 (corp, room)。分片键含 corpid，只返回 room 不够。
///
/// ⚠️ 「有文件」不等于「窗口内有消息」—— 判断后者要读文件，那是 [`read_room`]
/// 的活。窗口内一条消息都没有的群，[`read_room`] 返回空 `msgs`，由调用方跳过。
pub fn list_rooms(raw_root: &Path, w: &Window) -> Vec<(String, String)> {
    // BTreeSet 顺便排序：调用方按固定顺序跑批，日志和失败列表才可比。
    let mut found = BTreeSet::new();
    for m in months(w) {
        // 目录不存在 = 那个月没拉过，不是错误。
        let Ok(corps) = fs::read_dir(raw_root.join(&m)) else {
            continue;
        };
        for corp in corps.flatten() {
            let Ok(rooms) = fs::read_dir(corp.path()) else {
                continue;
            };
            for room in rooms.flatten() {
                let p = room.path();
                if p.extension().is_some_and(|e| e == EXT)
                    && let Some(stem) = p.file_stem()
                {
                    found.insert((
                        corp.file_name().to_string_lossy().into_owned(),
                        stem.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
    }
    found.into_iter().collect()
}

/// 存在的那几个月文件，**连同它属于哪个月**。跨月时某个月可能没有（新建群 / 已解散），
/// 而 DuckDB 对不存在的路径直接报错，所以必须先过一遍 `is_file`。
///
/// 月份跟着路径一起返回，是为了让 [`scan`] 的月份守卫**不必再从路径里反解一次** ——
/// 这里刚用它拼出的东西，没有道理让下游拆回来。
fn files(raw_root: &Path, corp: &str, room: &str, w: &Window) -> Vec<(String, PathBuf)> {
    months(w)
        .into_iter()
        .map(|m| {
            let p = room_path(raw_root, &m, corp, room);
            (m, p)
        })
        .filter(|(_, p)| p.is_file())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// 读
// ─────────────────────────────────────────────────────────────────────────────

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
        // 按**列名**取，不按下标 —— 列名即领域名，那个名字必须在读取点也成立。
        // 下标错位在这里是静默的（12 列有 8 列都是字符串，互换类型兼容、编译通过、
        // 守卫也放行）；列名与 SQL 引用同一组 COL_* 常量，打错是编译错误。
        let msg_id: String = r.get::<_, Option<String>>(COL_MSG_ID)?.unwrap_or_default();
        let r_room: String = r.get::<_, Option<String>>(COL_ROOM)?.unwrap_or_default();
        let r_corp: String = r.get::<_, Option<String>>(COL_CORP)?.unwrap_or_default();
        let at: Option<NaiveDateTime> = r.get(COL_AT)?;
        let sender_id: String = r.get::<_, Option<String>>(COL_SENDER_ID)?.unwrap_or_default();
        let raw_role: Option<String> = r.get(COL_SENDER_ROLE)?;
        let text: String = r.get::<_, Option<String>>(COL_TEXT)?.unwrap_or_default();
        let reply_to: Option<String> = r.get(COL_REPLY_TO)?;
        let schema_v: Option<i64> = r.get(COL_SCHEMA_VERSION)?;
        let parser_v: Option<i64> = r.get(COL_PARSER_VERSION)?;
        let upstream_type: Option<String> = r.get(COL_UPSTREAM_TYPE)?;
        let src_file: String = r.get::<_, Option<String>>(COL_SRC_FILE)?.unwrap_or_default();

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
        let Some(at) =
            at.filter(|_| !msg_id.is_empty() && !sender_id.is_empty() && !text.is_empty())
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
        //           真要根治得让 pull 认识 messageTime，那会破坏「上游字段名只在这里」。
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
            tracing::info!(room, r#type = t, "遇到样本外的消息类型，原样通过（正文走 content 兜底）");
        }

        // ⑤ 去重 —— raw 只增不删、上游可能重复投递，同一 msg_id 出现两次是真实会
        //    发生的。行已按 at 升序，所以首次出现的就是时间最早的那条。
        if !seen.insert(msg_id.clone()) {
            dupes += 1;
            continue;
        }
        out.push(Message {
            msg_id,
            room: r_room,
            corp: r_corp,
            at,
            sender_id,
            sender_role,
            text,
            reply_to,
        });
    }

    if dupes > 0 {
        // 静默去重 = 不知道自己在丢东西。重复是上游噪声不是数据损坏，不必整群失败。
        tracing::warn!(room, dupes, "去掉重复 msg_id");
    }
    Ok(out)
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
#[allow(dead_code)]
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
    let missing: BTreeSet<&String> = want.iter().filter(|id| !got.contains(id.as_str())).collect();
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

// ─────────────────────────────────────────────────────────────────────────────
// 自检 —— 断言要能在 CI 里跑：cargo test。
// 测试是 `ingest` 的子模块（`ingest/tests.rs`），私有项照常可见，
// 「单元测试跟着被测代码走」的实质不变，分出去的只是那 300 行的物理位置。
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
