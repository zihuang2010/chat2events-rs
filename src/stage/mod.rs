//! 七阶段的六个模块 —— 一轮跑批从 OSS 到 MySQL 中间的全部处理。
//!
//! ```text
//! OSS ──① mirror──> 本地镜像 ──①② ingest──> Conversation
//!                                   │
//!                       ③ extract ──┴──> ④ assemble ──> Event
//!                                                        │
//!                                   ⑤ classify ──> Labels ┤
//!                                   ⑥ metrics  ──> 指标行 ┤
//!                                                        ▼
//!                                                   ⑦ store（MySQL）
//! ```
//!
//! **它们是「处理」不是「进程」**：谁在什么时候调它们由 `crate::process` 决定，
//! 这里一个阶段都不认识调度、窗口策略和退出码。`mod.rs` 只装模块文档和声明 ——
//! 各阶段的接口、端口判据与内部布局在自己的 `mod.rs` 里。
//!
//! **可见性照搬分组之前的那份判据**，一个字没松：
//! `metrics` / `mirror` / `store` 只被编排层消费，收 `pub(crate)` ——
//! 「写库 SQL 一条不许外流」从口头约定变成可见性声明。**外层是 `pub mod stage`
//! 不影响这一点**：内层的 `pub(crate)` 仍然把它们挡在 crate 边界内。

pub mod classify;
pub mod extract;
pub mod ingest;
pub(crate) mod metrics;
pub(crate) mod mirror;
pub(crate) mod store;
