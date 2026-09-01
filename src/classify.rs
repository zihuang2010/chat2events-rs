//! ⑤ 分类 classify —— `summary` + 词表版本 -> `event_type`。
//!
//! **确定性是硬约束**：同样的 `summary` + 同样的词表 → 永远同样的 type。所以每次 event
//! 落库都算，包括分片删重写那一次 —— **标签不刻在 `Event` 上，是每次算出来的**，
//! 分片重写因此不会丢标签。
//!
//! **打标发生在事务外**：由 `daily` 算好后传给 ⑥ 和 ⑦。三件事一起解决 ——
//! 长事务（v1 缓存未命中 = 持锁发 N 次 embedding 请求）、依赖成环（`store` 不 import
//! 这里）、算两遍（⑥ 指标和 ⑦ 落库共用同一份结果）。
//!
//! ⚠️ **没有 `Classifier` trait，是有意的。** 本仓库自己的端口判据是「一个适配器 =
//! 假想接缝，两个 = 真接缝」，而 v1 需要词表表（`b_merchant_group_taxonomy`）、
//! embedding 和缓存 —— **今天一样都没有**，Python 版的 `make_classifier` 对 v0 以外
//! 也是直接抛 `NotImplementedError`。写个只有一个实现的 trait 壳，买到的只有
//! 「改签名要改两处」。
//!
//! ponytail: v1 落地时再引 trait —— 那时它要拿住三样状态（词表 / `sha256(summary)`
//! 结果缓存 / embedding 缓存），纯函数拿不住，接缝那时才是真的。

/// 当前词表版本。**v0 不是缺陷，是明确的上线阶段** —— 系统在任何阶段都能完整跑通，
/// 不需要等词表。升版是人工动作。
pub const CURRENT_VERSION: &str = "v0";

/// 两种 `__untyped__` 严格区分：
///   * `v0` + `__untyped__` = 还没有词表，**系统状态**
///   * `vN` + `__untyped__` = 有词表但归不上去，**数据信号**（覆盖不足，超阈值即需升版）
pub const UNTYPED: &str = "__untyped__";

/// v0：还没有词表，全部 `__untyped__`。
///
/// 不带缓存 —— 它是 O(1) 的常量函数，缓存要等到 v1 真的去发 embedding 请求。
pub fn classify(_summary: &str) -> &'static str {
    UNTYPED
}
