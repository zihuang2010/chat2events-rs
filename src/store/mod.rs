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
//!   facts.rs   write_room —— 事实列唯一写入方，不变量 1（冻结）/ 2（两分片同事务）
//!   labels.rs  标注列与分类指标唯一写入方，不变量 5（整群发布）
//!   read.rs    只读取数（daily::recover · taxonomy · recompute · web）
//!   schema.rs  启动期自检：五张表在不在、列对不对
//! ```

mod facts;
mod labels;
mod read;
mod schema;
mod sql;

pub use facts::write_room;
pub use labels::{fail_classification, finish_classification, retag_room, update_event_labels};
pub use read::{
    read_event_labels, read_event_rooms, read_events, read_summary_counts, read_taxonomy,
    unfinished_days,
};
pub use schema::check_schema;
pub use sql::Shard;

#[cfg(test)]
mod tests;
