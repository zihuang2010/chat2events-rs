//! `daily` 进程 —— 每日跑批的编排。
//!
//! 拉取 → 读取与抽取 → 保存事实 → 有界 channel → 独立打标与分类指标发布。
//! 抽取结束后排空打标队列，再退出。
//!
//! **编排住在这里，不住在 `main`。** `main` 是进程入口，它该做的只有
//! 「读配置、建资源、调 [`run()`]」。[`run()`] 是个普通 async 函数，测试直接调得到；
//! `#[tokio::main] async fn main()` 调不到。
//!
//! **失败隔离粒度 = 群 × 本次窗口 × 阶段**（承重不变量 3），失败语义
//! 对照表在 `store` 的模块注释里。整轮不因单群失败中止；运行结束汇报
//! 成功/失败群数，有失败就非零码退出（定时任务要看得见，别让 `run_failure`
//! 只躺在库里没人查）。
//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里）：
//!
//! ```text
//! daily/
//!   run.rs       两阶段编排、抽取群并发与事实保存
//!   labeling.rs  群任务消费、全局批次并发、独立标签更新与指标发布
//!                （**不叫 classify.rs**：⑤ 是 `crate::classify`，同名会让
//!                 `use super::classify` 和 `use crate::classify` 挤在同一屏）
//!   recover.rs   人工恢复：按群日补齐未完成分类，不重新抽取冻结事实
//!   tally.rs     抽取结果与预算记账
//! ```
//!
//! [`run`] 是日常跑批（窗口 `[T-(N+1), T-2]`）；[`run_span`] 让调用方自己给窗口，
//! 补跑历史走它（`examples/backfill.rs`）。

mod labeling;
mod recover;
mod run;
mod tally;

pub use recover::recover;
pub use run::{run, run_span};

#[cfg(test)]
mod tests;
