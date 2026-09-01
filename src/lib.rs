//! chat2events —— 从企业微信群聊日志中抽取结构化业务事件（七阶段跑批，见 CLAUDE.md）。
//!
//! lib + bin 拆分的理由（2026-09-01 第二轮评审）：
//!   * 全仓唯一的 [`Result`] / [`BoxError`] 别名落点 —— 此前四个模块各写一份；
//!   * `daily::run` 等编排从此可被集成测试调用 —— 纯 bin crate 没有可导入的
//!     crate 根，`tests/` 结构上编不了。

// 目录模块（`<模块>/mod.rs`）与单文件模块混排，规则只有一条：**测试块 ≥ 100 行
// 就把测试拆成 `<模块>/tests.rs`**，其余留在文件底部的 `#[cfg(test)] mod tests`。
// `extract/` 是唯一一个连生产代码也分了文件的 —— 它一个人装了 6 件互不相干的事，
// 而对外仍然只有 `Event` / `SegmentModel` / `extract` 三样（拆的是导航，不是深度）。
pub mod classify;
pub mod config;
pub mod daily;
pub mod extract;
pub mod ingest;
pub mod llm;
pub mod metrics;
pub mod pull;
pub mod store;
pub mod window;

#[cfg(test)]
pub(crate) mod testutil;

/// 动态错误：跨层编排（`daily` / `extract` / `llm` / `main`）用它。
/// 需要按变体分流处置的模块用自己的枚举（[`ingest::IngestError`] / [`pull::PullError`]）。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = BoxError> = std::result::Result<T, E>;
