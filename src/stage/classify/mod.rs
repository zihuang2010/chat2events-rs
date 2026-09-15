//! ⑤ 分类 classify —— `summary` ＋ 词表 -> [`Label`]（一个类）。
//!
//! ⚠️ **曾经是多标签**（一个事件最多 3 个类，副类落 `event_types` 那一列）。
//! 2026-09-14 整套拿掉，理由和「要加回来先想清楚什么」都在 [`Label`] 上。
//!
//! **确定性是硬约束**：同样的摘要与分类策略复用已缓存标签。
//! 事实保存后由独立队列安排打标；标签不属于 `Event` 事实类型。
//!
//! ⚠️ **确定性由缓存保证，不由算法保证。** v1 的算法是「让模型从封闭词表里选」，
//! 而 `temperature = 0` 并不保证同输入同输出。非冻结区每天重写 `[T-3, T-2]`，
//! 同一批 event 会被反复打标 —— 没有跨运行的持久缓存，报表就会**抖动而非修正**，
//! 正是承重不变量 1 要防的那件事。**[`Cache`] 是承重件，不是优化。**
//!
//! 模型调用不持数据库事务；`daily/labeling.rs` 调度批次并回写标签，全部成功后计算指标。
//! 本模块只处理摘要、词表和标签，不读取业务 MySQL；SQLite 仅保存分类历史答案。
//!
//! ## 两种 `__untyped__`，两条完全不同的路
//!
//! | 情形 | 结果 | 为什么 |
//! |---|---|---|
//! | 词表为空（`v0`） | 全 `__untyped__`，**不发请求** | 还没有词表，**系统状态** |
//! | 有词表但模型归不上去 | 该条 `__untyped__` | **数据信号**，超阈值即需升版 |
//! | 模型编了一个词表里没有的 `type_id` | **校验失败 → 重问 → 仍失败则该批次失败** | 编造不是「归不上去」。放行会污染上面那个信号，而升版决策就看它 |
//! | 模型漏了某一行 | 同上 | 「没算出来」绝不许表现成一个正常取值（承重不变量 4） |
//!
//! ⚠️ **没有 `Classifier` trait，仍然是有意的。** `docs/architecture.md` 说「v1 落地时
//! 再引接缝……纯函数拿不住三样状态」，那句话真正要的是**「一次运行构造一次的对象」**，
//! 一个 struct 已经满足。而 v0 **不需要第二个实现** —— 「还没有词表」在库里精确地
//! 等于「显式选择 v0 且词表没有行」。正式版本缺词表是启动错误。
//! 当前唯一生产路径是大模型，训练分类器待人工审核样本充足后再实现。

//!
//! **文件布局**（`mod.rs` 只装模块文档、声明和导出，生产代码一律在兄弟文件里；
//! 拆成目录只为导航 —— 接口一字未动，对外仍然是下面那几样）：
//!
//! ```text
//! classify/
//!   types.rs  TaxonomyType · Label · Assignment —— 领域类型，不认识 prompt 和缓存
//!   （`Label` 是单值；多标签 2026-09-14 移除，见它的文档注释）
//!   check.rs  词表加载守卫：check_version / check_types（库与草稿共用）
//!   model.rs  Classifier · render_system · validate —— 模型协议，端点知识在 llm
//!   cache.rs  Cache —— 内容寻址 SQLite，确定性的承重件
//! ```

mod cache;
mod check;
mod model;
mod types;

pub(crate) use check::{check_types, check_version};
// `DESC_MAX` 只有 `taxonomy::draft` 的测试用 —— 跟着那边的 `#[cfg(test)]` 走，
// 否则这条 re-export 在生产构建里就是个 unused warning。
#[cfg(test)]
pub(crate) use check::DESC_MAX;
pub(crate) use model::BATCH;
pub use model::Classifier;
pub use types::{CURRENT_VERSION, Label, TaxonomyType, UNTYPED};

#[cfg(test)]
mod tests;
