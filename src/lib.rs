//! chat2events —— 从企业微信群聊日志中抽取结构化业务事件（七阶段跑批，见 CLAUDE.md）。
//!
//! lib + bin 拆分的理由（2026-09-01 第二轮评审）：
//!   * 全仓唯一的 [`Result`] / [`BoxError`] 别名落点 —— 此前四个模块各写一份；
//!   * `daily::run` 等编排从此可被集成测试调用 —— 纯 bin crate 没有可导入的
//!     crate 根，`tests/` 结构上编不了。

pub mod config;
pub mod daily;
pub mod extract;
pub mod ingest;
pub mod llm;
pub mod pull;
pub mod window;

#[cfg(test)]
pub(crate) mod testutil;

/// 动态错误：跨层编排（`daily` / `extract` / `llm` / `main`）用它。
/// 需要按变体分流处置的模块用自己的枚举（[`ingest::IngestError`] / [`pull::PullError`]）。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = BoxError> = std::result::Result<T, E>;
