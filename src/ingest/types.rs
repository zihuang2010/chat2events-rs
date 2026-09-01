//! 领域类型 —— **这个文件不认识上游**：没有 DuckDB、没有 SQL、没有一个 camelCase
//! 字段名。上游 `identityType` 那两个字面量只在 [`Role::parse`] 出现一次，
//! 那是「上游只有这两个值」这条契约的唯一成立点。

use chrono::{NaiveDate, NaiveDateTime};
use std::{collections::BTreeMap, fmt};

/// 群里的两方，**都是客服**（`CONTEXT.md`：群里没有终端消费者）。
/// 上游 `identityType` 只有这两个值，实测 100% 填充。
///
/// **收成枚举而不是裸字符串，是因为错法是静默的**：`== "INTERNAL"` 打错一个字母，
/// [`crate::extract`] 的 `labels` 会把平台客服全标成「商家X」、`assemble` 的 `agents`
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
    /// 上游 `identityType` -> 领域角色。**唯一的解析点**（`read::message_from_row` 的取值处）。
    ///
    /// 认不出就是 `None`，由调用方判该群失败 —— **不能兜底成任意一边**：
    /// 判成 `Internal` 会把商家算进 `agents`，判成 `External` 会让平台的回复不再算首响，
    /// 两个方向都是静默把指标写歪。
    pub(super) fn parse(s: &str) -> Option<Self> {
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

pub(super) type Result<T> = std::result::Result<T, IngestError>;
