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
//!   compute.rs  行类型 + Status / Attribution 两个口径枚举 + 算出那些行的纯函数
//! ```
//!
//! 行类型曾经单住 `rows.rs`。合回来是因为那 73 行里 40 行是**在描述算法**
//! （分母是哪一列、失败时是 `None` 不是 0），读的人两边来回翻。

mod compute;

pub use compute::{AgentRow, Attribution, GroupRow, Status, agent_rows, group_rows};

#[cfg(test)]
mod tests;
