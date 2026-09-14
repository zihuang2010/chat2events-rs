//! 三个进程 —— 把 `crate::stage` 的六个阶段串起来的**编排**。
//!
//! | 进程 | 触发 | 干什么 | 失败语义 |
//! |---|---|---|---|
//! | [`daily`] | 每日定时（`src/main.rs`） | 拉取 → 抽取保存 → channel → 独立打标与指标发布 | 群 × 日**分阶段**隔离，整轮继续 |
//! | [`taxonomy`] | **人工**（`src/bin/taxonomy.rs`） | **只产词表**，不写 `b_merchant_group_event` | 失败无所谓，不阻塞任何人 |
//! | [`recompute`] | **人工**（`src/bin/recompute.rs`） | 词表升版后重打标：**只写标注列** | 群隔离，一个群一个事务，整轮继续 |
//!
//! **编排住在这里，不住在入口文件里。** `src/main.rs` 和 `src/bin/*.rs` 该做的只有
//! 「`boot::Boot` 起进程、调下面某个函数」—— 那几个函数是普通 async 函数，测试直接
//! 调得到，`#[tokio::main] async fn main()` 调不到。
//!
//! `daily::recover`（人工补标）住在 `daily` 里而不是单独一个进程：它复用的是
//! 同一套群日状态与打标路径，只是不重新抽取冻结事实。

pub mod daily;
pub mod recompute;
pub mod taxonomy;
