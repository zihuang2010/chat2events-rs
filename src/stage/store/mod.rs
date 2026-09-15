//! ⑦ 落库 store —— **MySQL 唯一写入方。所有写库 SQL 都在这个目录里，一条都不许外流。**
//!
//! 不是「存储层抽象接口」—— MySQL 是当前唯一目标，这里只保证写库代码集中在一处。
//! 建表走手写的 `schema.sql`，人工执行一次；本模块**不碰 DDL**。
//!
//! **一个群一次抽取的事实写入 = 一个事务**（承重不变量 2）。标签由独立阶段按批更新。
//! `occurred_on = date(first_msg_time)`，而「首条来源消息是哪条」由模型判断 ——
//! 同一个 event 会在 `T-3` / `T-2` 两个分片之间移动。分两个事务提交，中间失败就会造成
//! 它**一个分片都不在**，或者**两个分片都在**。
//!
//! `b_merchant_group_agent_msg_daily`（客服日消息量）**不在下面这张表里** ——
//! 它跟着 `group` 表走，不跟着 `event` / `agent` 走：消息数不依赖抽取也不依赖词表，
//! 所以抽取失败照写、打标与重打标不碰。理由见 `metrics::AgentMsgRow`。
//!
//! 失败隔离粒度 = 群 × 本次窗口，三种失败语义不同（承重不变量 3 / 4 / 5）：
//!
//! | 失败在哪 | `event` / `agent` 表 | `group` 表 | `run_failure` |
//! |---|---|---|---|
//! | **拉取** | 不写 | **不写**（连消息都不全，写 0 就是拿 0 冒充「没算出来」） | 写 |
//! | **抽取** | 不写 | 写，`extraction_status='failed'`，事件级 NULL | 写 |
//! | **打标** | 保留已存事实和成功批次标签，不发布客服分类指标 | `classification_status='failed'` | 写 |
//! | **没轮到**（整轮预算用完） | 不写 | **不写**（同拉取：消息根本没读过） | 写 |
//! | 无（`Ok([])`）| 按分片删重写 | 写，`ok`，事件级 **0** | — |
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里；
//! 「写库 SQL 一条不许外流」约束的是**不出这个目录**，不是「必须挤进一个文件」）：
//!
//! ```text
//! store/
//!   sql.rs     Shard（定位）· 表名 · 列名 · 批大小 · 占位符 —— 零业务逻辑
//!              （表名 `pub(crate)`：`web/cache.rs` 的数据戳要用同一份）
//!   facts.rs   write_room —— 事实列唯一写入方，不变量 1（冻结）/ 2（两分片同事务）
//!   labels.rs  标注列与分类指标唯一写入方，不变量 5（整群发布）
//!   read.rs    只读取数（daily::recover · daily::retry · taxonomy · recompute）
//!
//! ⚠️ **webUI 的取数不在这里，在 `web/query.rs` —— 有意分家。** 只读旁路只从
//! 本文件拿两样：`read_taxonomy` 和 `check_schema`。**不要把两边合并**：合并会把
//! webUI 那一千多行只读 SQL 拖进 `pub(crate) mod store`，撞 CLAUDE.md 的「跑批不
//! 知道 webUI 存在」，并把两个生命周期（T+2 跑批 / 随时可查的看板）焊死在一起。
//!   schema.rs  启动期自检：六张表在不在、列对不对
//! ```

mod facts;
mod labels;
mod read;
mod schema;
mod sql;

pub use facts::{prune_source_messages, record_failure, write_room};
pub use labels::{fail_classification, finish_classification, retag_room, update_event_labels};
pub use read::{
    classify_failure_span, read_event_labels, read_event_rooms, read_events, read_summary_counts,
    read_taxonomy, unfinished_days, unrepaired_extract_failures,
};
pub use schema::{check_schema, refresh_statistics};
pub use sql::Shard;
// 表名给 `web/cache.rs` 的数据戳查询用 —— 理由见 `sql.rs` 那几个常量的文档注释。
pub(crate) use sql::{T_EVENT, T_FAILURE, T_GROUP, T_TAXONOMY};

#[cfg(test)]
mod tests;
