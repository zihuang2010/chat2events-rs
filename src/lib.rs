//! chat2events —— 从企业微信群聊日志中抽取结构化业务事件（七阶段跑批，见 CLAUDE.md）。
//!
//! lib + bin 拆分的理由：
//!   * 全仓唯一的 [`Result`] / [`BoxError`] 别名落点 —— 此前四个模块各写一份；
//!   * 六个入口（`src/main.rs` ＋ `src/bin/` 五个）要 import 同一套编排 ——
//!     纯 bin crate 里它们只能各抄一份，或者靠 `#[path]` 互相塞。
//!
//! ⚠️ 这里曾经写着「`daily::run` 从此可被 `tests/` 调用」—— **仓库里没有 `tests/`**，
//! 全部测试都是模块内的 `#[cfg(test)]`。那条理由今天是空的，拆分本身仍然成立。

// ─────────────────────────────────────────────────────────────────────────────
// 目录组织
// ─────────────────────────────────────────────────────────────────────────────
//
// **`src/` 下四类东西，分类写在路径上，不写在注释里**：
//
//   stage/     七阶段的六个模块 —— 一轮跑批的全部「处理」
//   process/   把它们串起来的三个「进程」编排：daily · taxonomy · recompute
//   web/       只读旁路（跑批不知道它存在）
//   根目录     内核：boot · config · llm · window · worktime · rejection
//
// 这四类此前平摊在 crate 根的 15 个 `mod` 声明上，靠这段注释区分 —— 注释在，
// 规则就在；注释漂了，平面还在。现在读 `use crate::stage::extract` 就知道它是哪一类。
// **内核留在根上**：它们被两类都用，往下塞进任何一组都是错的归属。
//
// **多文件模块用 `mod.rs`，且 `mod.rs` 只装三样：模块文档、`mod` 声明、`pub use`
// 导出。** 一行生产代码都不放 —— 于是「这个模块对外是什么」在一屏之内读完，
// 「它内部怎么实现」全在兄弟文件里，两个问题不再挤在同一个文件的头尾。
// `stage/mod.rs` 与 `process/mod.rs` 也守这一条。
//
// 什么时候会有那个目录，规则只有两条：
//   * **生产代码装了几件互不相干的事** → 按职责拆成兄弟文件。
//     **接口一字未动 —— 拆的是导航，不是深度。**
//     反过来也成立：`metrics` 合回单文件，是因为目录里只剩一个生产孩子之后，
//     那一层不再换任何东西、只留下一跳，`mod.rs` 退化成纯转发。
//   * **测试块 ≥ 100 行** → 拆成 `<模块>/tests.rs`。**豁免的是「非 `mod.rs` 的文件」**
//     —— 目录模块的子文件（`extract/redact.rs` 之流）单元测试留在各自底部，
//     再拆一层目录导航成本反超收益；跨文件的测试和共享 fixture 才进 `<模块>/tests.rs`
//     （`mirror/tests.rs` 的 `MonthFile` fixture 就是这一条）。
//
// ⚠️ 第二条曾经写成「**目录模块的**子文件不套用」，于是它和「单文件模块不建目录」
// 在 `config.rs`（测试 177 行）和 `llm.rs`（145 行）上**互相矛盾** —— 两条规则各指一个
// 相反的方向，谁按哪条都能被反驳，规则于是不可执行。裁决取前者：
// **Rust 2018+ 不需要 `mod.rs`**，`config.rs` 配一个 `config/tests.rs` 文件数不变、
// 目录里也不会多出第三个文件，「多一层」那个代价本来就不成立。
//
// 单文件模块**不为了统一而建目录** —— 但「测试块撑到 100 行」不是「为了统一」，
// 那是上面那条规则本身。今天 `window` / `worktime` / `rejection` / `boot` 的测试
// 都在阈值下，所以它们仍是单文件；`config` / `llm` 各带一个 `tests.rs`。
//
// 跨兄弟文件用的项标 `pub(super)`，不是 `pub`：那是「同一个模块内部的事」和
// 「这个模块对外的承诺」之间的分界，写在可见性上而不是靠自觉。
//
// **`pub` 的模块 = crate 外有真实读者的那几个**（`main` / `src/bin/` / `examples`）。
// `stage::metrics` / `stage::mirror` / `stage::store` 只被编排层消费，收 `pub(crate)`
// ——「写库 SQL 一条不许外流」从口头约定变成可见性声明。**外层 `pub mod stage`
// 不削弱这一点**：内层的 `pub(crate)` 照样把它们挡在 crate 边界内。
//
// ⚠️ `classify` 在 v1 之后转成 `pub`：`taxonomy` 的归纳与审阅、`recompute` 的重打标
// 都拿 `Classifier` 和 `TaxonomyType` 说话，而 `src/bin/` 里那几个人工工具是它们的
// 入口。转 `pub` 不破坏上面那条规矩 —— `classify` 只操作本地答案缓存，读业务词表是
// `store` 的事，词表由调用方读好传进来（那同时也是「classify 不 import store」的由来）。
//
// **编排住在 lib 里，不住在入口文件里**：`src/main.rs` 和 `src/bin/*.rs` 只负责
// `boot::Boot` 起进程、调 `process::` 下面某个函数。

// 七阶段（① mirror ①② ingest ③④ extract ⑤ classify ⑥ metrics ⑦ store）
pub mod stage;

// 把七阶段串起来的编排：daily · taxonomy · recompute
pub mod process;

// 只读旁路 —— 跑批不知道它存在
pub mod web;

// 内核：全部阶段与进程都用得到，各自只干一件事
pub mod boot;
pub mod config;
pub mod llm;
mod rejection;
pub mod window;
pub mod worktime;

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
