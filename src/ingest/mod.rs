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
//! **四样东西不出 `ingest/`，且各自只住一个文件**：DuckDB 连接 · SQL · 上游字段语义
//! 在 `read.rs`（camelCase 字段名只允许出现在那条 `SELECT ... AS ...` 里），
//! 路径布局在 `layout.rs`（只被拼一次，没有第二处再去拆它）。
//!
//! 本地布局是 OSS 的**字节级镜像**（一个群一个月一个文件，见 ADR-0005）：
//!
//! ```text
//! <raw_root>/<yyyyMM>/<corpId>/<officialRoomId>.ndjson
//! ```
//!
//! 所以「已经拉到第几字节」= 文件大小，不需要任何额外的状态存储。
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里）：
//!
//! ```text
//! ingest/
//!   types.rs   Role · Message · Conversation · IngestError —— 领域类型，不认识上游
//!   layout.rs  路径布局：months · room_path · list_rooms · files · prune（保留期）
//!   read.rs    DuckDB 连接 · SQL · 上游字段语义 · 五道守卫 · read_room / read_by_ids
//! ```
//!
//! 端口那三个函数里 `list_rooms` 住在 `layout.rs` —— 它只遍历目录、不碰 DuckDB。

mod layout;
mod read;
mod types;

pub use layout::{list_rooms, months, prune, room_path};
pub use read::{read_by_ids, read_room};
pub use types::{Conversation, IngestError, Message, Role};

#[cfg(test)]
mod tests;
