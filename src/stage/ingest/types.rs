//! 领域类型 —— **这个文件不认识上游**：没有 DuckDB、没有 SQL、没有一个 camelCase
//! 字段名。上游 `identityType` 那两个字面量只在 [`Role::parse`] 出现一次，
//! 那是「上游只有这两个值」这条契约的唯一成立点。

use chrono::{NaiveDate, NaiveDateTime};
use std::{collections::BTreeMap, fmt};

/// 群里的两方，**都是客服**（`CONTEXT.md`：群里没有终端消费者）。
/// 上游 `identityType` 只有这两个值，实测 100% 填充。
///
/// **收成枚举而不是裸字符串，是因为错法是静默的**：`== "INTERNAL"` 打错一个字母，
/// [`crate::stage::extract`] 的 `labels` 会把平台客服全标成「商家X」、`assemble` 的 `agents`
/// 恒空、`first_agent_reply_time` 恒 `None`、⑥ 的首响 p50/p90 全 `NULL` ——
/// 三条链路一起坏，而编译器一句话都不说，报表只是安静地偏小。
///
/// 「上游只有这两个值」这条契约从此**只在 `Role::parse` 那一处成立一次**，
/// 下游全部是 `match`，打错是编译错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 平台客服 —— 受理诉求、协调师傅上门。指标口径里的 `agent` 就是这一边。
    Internal,
    /// 商家客服 —— 把订单诉求发到群里。事件多数由这一边发起。
    External,
}

impl Role {
    /// 上游 `identityType` -> 领域角色。**唯一的解析点**。
    ///
    /// 认不出就是 `None`，由调用方判该群失败 —— **不能兜底成任意一边**：
    /// 判成 `Internal` 会把商家算进 `agents`，判成 `External` 会让平台的回复不再算首响，
    /// 两个方向都是静默把指标写歪。
    ///
    /// 两个调用点：`read::message_from_row`（读上游 NDJSON）和 `store::read_events`
    /// （从 MySQL 读回 `asker_role` 列）。后者读的正是 [`Role::as_str`] 写下去的字面量，
    /// 所以走同一个 `match` —— 让 `store` 自己再写一遍 `== "INTERNAL"`，就等于把
    /// 「上游只有这两个值」这条契约复制到第二处，而它错法是静默的。
    pub fn parse(s: &str) -> Option<Self> {
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

/// 每个字段都有读取点。
///
/// 端口上每多一个死字段，就是向未来每一个适配器收一次税 —— `mentions` /
/// `plain_text` 曾经在这里，读取点分别是 0 / 只当兜底。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_id: String,
    pub room: String,
    pub corp: String,
    pub at: NaiveDateTime,
    pub sender_id: String,
    /// 内部员工账号，仅用于落库关联账号信息，不进入模型提示词。
    pub official_user_id: Option<String>,
    pub sender_role: Role,
    /// 上游 `standardType` 原样。`None` = 上游没给。
    ///
    /// ⚠️ **它曾经被删过**（连同 `mentions` / `plain_text`），理由是零读取点。
    /// 这次加回来是因为**有了真读取点**：`assemble` 靠它把非文本消息的正文换成
    /// 占位符（`[图片]` 等）。删它之前先确认那处没了。
    ///
    /// 「什么算非文本」由 [`Message::placeholder`] 一处判定，不在别处再写一遍。
    pub msg_type: Option<String>,
    pub text: String,
    pub reply_to: Option<String>,
}

impl Message {
    /// 非文本消息的展示占位符；文本消息返回 `None`（正文照常用）。
    ///
    /// ⚠️ **白名单式判定：只有认得出的媒体类型才掩掉。** `None`（上游没给类型）和
    /// 没见过的新类型一律按文本放行 —— 猜错的方向不对称：把文本误判成图片会**静默
    /// 丢掉真正的正文**，而把新媒体类型放过去只是多存一点字节，且 [`super::read`]
    /// 的 `SEEN_UNKNOWN` 日志本来就会为新类型喊一声。与承重不变量 7「只掩锚点确定
    /// 的东西」同一条道理。
    ///
    /// 媒体 URL / OCR 文本都不进库：URL 带签名会过期，存了也点不开。
    pub fn placeholder(&self) -> Option<&'static str> {
        match self.msg_type.as_deref() {
            Some("IMAGE") => Some("[图片]"),
            Some("GIF") => Some("[表情]"),
            Some("VIDEO") => Some("[视频]"),
            _ => None,
        }
    }
}

/// **接口粒度 = 群 × 一次运行的完整会话 = 失败隔离粒度 = ③ 的输入。** 四者必须相等。
///
/// ⚠️ **没有 `corp` / `room` 字段**：全项目零读取点 —— `daily::run_room` 一路带着自己的
/// 那两个参数（它得先有 corp/room 才调得动 `read_room`），`Event` 的那两列来自
/// `Message`。照 `CONTEXT.md`「已删除的字段」那张表的先例删掉：**删的是税，不是功能**。
/// ⚠️ **不 derive `Clone`**：零使用点，而它是一个能静默复制整群未脱敏正文的口子。
/// 真需要第二份的那天再加，顺便说明为什么需要。
#[derive(Debug)]
pub struct Conversation {
    pub msgs: Vec<Message>,
    /// 每天 (消息条数, 去重发言人数)。搭 `msgs` 的同一趟车算出来 ——
    /// 这个群的消息本来就已经在内存里（③ 要用），这里没有多读一个字节。
    /// 副作用：⑥ 指标从此零 IO、零 SQL、零 duckdb，是纯函数模块。
    pub msg_counts: BTreeMap<NaiveDate, (usize, usize)>,
    /// 每天每个平台客服（`Role::Internal`）发了多少条。同上，搭同一趟车。
    ///
    /// **和 `msg_counts` 分开而不是塞进它的元组**：那个是群级（分母含商家侧），
    /// 这个只数 INTERNAL 一边，两者相加得不到对方，键也不同。
    pub agent_msg_counts: BTreeMap<(NaiveDate, String), usize>,
}

/// 两种失败的**处置方式不同**，所以必须在类型上分开：
/// `Upstream` 整轮退出、`Room` 该群跳过一行不写（承重不变量 3）。
///
/// ⚠️ **`Missing` 已删。** 它是 `read_by_ids` 的「下钻取不全」，而下钻改读
/// `b_merchant_group_event.source_messages` 之后没有这种状态了。
#[derive(Debug)]
pub enum IngestError {
    /// 上游解析器变了 —— 整轮失败退出，不做兼容层。
    Upstream(String),
    /// 该群失败：整体跳过、一行不写、记 `run_failure`。
    Room(String),
    Db(duckdb::Error),
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upstream(m) | Self::Room(m) => f.write_str(m),
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

pub(super) type Result<T> = std::result::Result<T, IngestError>;
