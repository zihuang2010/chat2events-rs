//! chat2events —— 从企业微信群聊日志中抽取结构化业务事件（七阶段跑批，见 CLAUDE.md）。
//!
//! lib + bin 拆分的理由（2026-09-01 第二轮评审）：
//!   * 全仓唯一的 [`Result`] / [`BoxError`] 别名落点 —— 此前四个模块各写一份；
//!   * `daily::run` 等编排从此可被集成测试调用 —— 纯 bin crate 没有可导入的
//!     crate 根，`tests/` 结构上编不了。

// **多文件模块用 `mod.rs`，且 `mod.rs` 只装三样：模块文档、`mod` 声明、`pub use`
// 导出。** 一行生产代码都不放 —— 于是「这个模块对外是什么」在一屏之内读完，
// 「它内部怎么实现」全在兄弟文件里，两个问题不再挤在同一个文件的头尾。
//
// 什么时候会有那个目录，规则只有两条：
//   * **生产代码装了几件互不相干的事** → 按职责拆成兄弟文件（今天六个模块都是这样）。
//     **接口一字未动 —— 拆的是导航，不是深度。**
//   * **测试块 ≥ 100 行** → 拆成 `<模块>/tests.rs`。**目录模块的子文件不套用这条**
//     —— 它们的单元测试留在各自文件底部（再拆一层目录，导航成本反超收益）；
//     跨文件的测试和共享 fixture 才进 `<模块>/tests.rs`。
//
// 单文件模块（`config` / `window` / `llm` / `store` / `testutil`）
// **不为了统一而建目录** —— 它们各自只干一件事，一个 `mod.rs` 加一个兄弟文件
// 换不来任何导航收益，只多一层。
//
// 跨兄弟文件用的项标 `pub(super)`，不是 `pub`：那是「同一个模块内部的事」和
// 「这个模块对外的承诺」之间的分界，写在可见性上而不是靠自觉。
//
// **`pub` 的模块 = crate 外有真实读者的那几个**（`main` / `examples`）。
// `metrics` / `mirror` / `store` 只被编排层消费，收 `pub(crate)` ——
// 「写库 SQL 一条不许外流」从口头约定变成可见性声明。集成测试的入口是
// `daily::run`（pub），不受影响；哪天 `tests/` 要直接驱动 store / mirror 再放开。
//
// ⚠️ `classify` 在 v1 之后转成 `pub`：`taxonomy` 的归纳与审阅、`recompute` 的重打标
// 都拿 `Classifier` 和 `TaxonomyType` 说话，而 `examples/` 里那几个人工工具是它们的
// 入口。转 `pub` 不破坏上面那条规矩 —— `classify` 只操作本地答案缓存，读业务词表是 `store`
// 的事，词表由调用方读好传进来（那同时也是「classify 不 import store」的由来）。
//
// **编排住在 lib 里，不住在 example**（跟 `main` / `daily` 同一条规矩）：
// `taxonomy` 和 `recompute` 是两个人工触发的进程，`examples/*.rs` 只负责
// 读配置、建资源、调它们。
pub mod classify;
pub mod config;
pub mod daily;
pub mod extract;
pub mod ingest;
pub mod llm;
pub(crate) mod metrics;
pub(crate) mod mirror;
pub mod recompute;
mod rejection;
pub(crate) mod store;
pub mod taxonomy;
pub mod web;
pub mod window;

#[cfg(test)]
pub(crate) mod testutil;

/// 动态错误：跨层编排（`daily` / `extract` / `llm` / `main`）用它。
/// 需要按变体分流处置的模块用自己的枚举（`ingest::IngestError` / `mirror::MirrorError`）。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = BoxError> = std::result::Result<T, E>;

/// `JoinSet` 收割。任务本身不会 panic（每条失败路径都返回 `Err`），所以 `JoinError`
/// 只可能是 bug —— 「构造已经保证、不可能为假」那一档，按硬规则用 `expect`。
/// `mirror` 与 `daily` 共用这一份 —— 曾经两处各写一遍，两句 expect 文案已开始漂移。
pub(crate) fn join<T>(j: Option<std::result::Result<T, tokio::task::JoinError>>) -> T {
    j.expect("set.len() > 0 时必有一个可 join")
        .expect("任务的每条失败路径都返回 Err，不 panic")
}
