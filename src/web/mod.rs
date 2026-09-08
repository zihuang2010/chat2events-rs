//! webUI 只读旁路：MySQL 一致快照与 ingest 原文读取，**不写表、不调用模型**。
//!
//! 跑批不知道它存在 —— 它只从 MySQL 和 ① 的端口取数，独立进程启动（`src/bin/webui.rs`）。
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里）：
//!
//! ```text
//! web/
//!   config.rs  只读入口自己的配置与只读连接池（跑批的 `config.rs` 里没有任何 `Web*`）
//!   serve.rs   应用状态 · 路由 · 四个 handler
//!   budget.rs  并发准入 · 行数与字节预算 · 响应封顶 · WebError（超限一律拒绝，不截断）
//!   query.rs   一致快照 · meta · 日期区间 · event 的 SELECT —— 只读 SQL 全在这里
//! ```

mod budget;
pub mod config;
mod query;
mod serve;

pub use serve::serve;

#[cfg(test)]
mod tests;
