//! ⑥ 指标 metrics —— 两张指标表的行的唯一来源（写库 SQL 在 ⑦ `store`）。
//!
//! **纯函数模块：零 IO、零 SQL、零 duckdb。** 两个来源 —— **事件级**指标读 [`crate::extract::Event`]，
//! **消息级**指标读 `Conversation.msg_counts`（搭 ① 的同一趟车算好的）。
//!
//! 指标表**不受分片冻结约束** —— 它依赖的事实全都还在，随时可整体重算。所以「换归属
//! 口径」「词表升版重打标」都不用重跑 LLM。
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里）：
//!
//! ```text
//! metrics/
//!   rows.rs     两张表的行类型 + Status / Attribution 两个口径枚举
//!   compute.rs  从 Event / msg_counts 算出那些行的纯函数
//! ```

mod compute;
mod rows;

pub use compute::{agent_rows, group_rows};
pub use rows::{AgentRow, Attribution, GroupRow, Status};

#[cfg(test)]
mod tests;
