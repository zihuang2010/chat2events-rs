//! webUI 只读旁路：MySQL 一致快照与事件下钻，**不写表、不调用模型**。
//!
//! ⚠️ 这里曾经写着「与 ingest 原文读取」—— `web/` **零 `use crate::stage::ingest`**。
//! 原文下钻改读 `b_merchant_group_event.source_messages` 之后（`ingest` 的
//! `read_by_ids` 随之删掉），只读工作台只依赖 MySQL 一个东西。
//!
//! 跑批不知道它存在 —— 它只从 MySQL 和 ① 的端口取数，独立进程启动（`src/bin/webui.rs`）。
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里）：
//!
//! ```text
//! web/
//!   config.rs  只读入口自己的配置与只读连接池（跑批的 `config.rs` 里没有任何 `Web*`）
//!   state.rs   WebState —— serve / budget / cache 三个文件共用的应用状态
//!   serve.rs   路由 · handler（只把参数、限额和取数接起来，**零 SQL**）
//!   params.rs  请求形状：Period · Paging · Filters · Sla · Grouping · Bind · 反序列化
//!   budget.rs  并发准入 · 行数与字节预算 · 响应封顶 · WebError（超限一律拒绝，不截断）
//!   cache.rs   白天的响应缓存：库里的「数据戳」一变整个作废，戳太新（跑批在写）不存
//!   scope.rs   SQL 片段与绑定值成对产出 —— 「第 n 个 `?` 配第 n 个绑定」由构造保证
//!   query.rs   一致快照 · meta · 日期区间 · event 的 SELECT —— 只读 SQL 全在这里
//! ```

mod budget;
mod cache;
pub mod config;
mod params;
mod query;
mod scope;
mod serve;
mod state;

pub use serve::serve;

#[cfg(test)]
mod tests;
