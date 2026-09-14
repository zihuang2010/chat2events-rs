//! 应用状态 —— `serve` / `budget` / `cache` 三个文件共用的那一份。
//!
//! 单住一个文件是因为它是**所有人都要的东西**：此前它住在 `serve.rs` 的实现里，
//! 于是两个中间件（`budget::admit` / `cache::cached`）要 `use super::serve::WebState`
//! 向上伸手去拿，而 `serve.rs` 反过来又从它们 import 7 个名字 —— `web/` 内部的
//! 依赖图上凭空多了两条反向边。挪出来之后是一条直线：
//! `state ← {budget, cache, query, serve}`。
//!
//! ⚠️ **没有 `raw_root`**：原文下钻改读 `b_merchant_group_event.source_messages`
//! 之后，只读工作台**只依赖 MySQL 一个东西** —— 不碰文件系统，也就不需要
//! 「镜像在不在 / 同步没同步 / 相对路径的 cwd 对不对」那一整类失败。

use super::{cache::Cache, config::WebLimits};
use sqlx::MySqlPool;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// `#[derive(Clone)]` 是必需的 —— axum 拿它当单一整体 state，
/// `router()` 里两个 `from_fn_with_state` 各 clone 一次、`with_state` 再收走一次。
#[derive(Clone)]
pub(super) struct WebState {
    pub pool: MySqlPool,
    pub corp: String,
    pub limits: WebLimits,
    pub requests: Arc<Semaphore>,
    pub cache: Arc<Cache>,
}
