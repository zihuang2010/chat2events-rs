//! ⑤ 的领域类型 —— 词表的一个类、一条打标结果、模型这一批的输出外壳。
//!
//! 三样都不认识 prompt、缓存和 MySQL：`TaxonomyType` 是 `b_merchant_group_taxonomy`
//! 的四列，`Labels` 是「主类 + 全集」，`Assignment` 是模型答复里的一行。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 当前词表版本。**v0 不是缺陷，是明确的上线阶段** —— 系统在任何阶段都能完整跑通，
/// 不需要等词表。升版是人工动作（插词表行 → 改这个常量 → 跑 `recompute` 那个 bin）。
/// 2026-09-03 走到 **v1**：43 个二级类 × 9 个一级，词表人手写。
pub const CURRENT_VERSION: &str = "v1";

/// 两种 `__untyped__` 严格区分：
///   * `v0` + `__untyped__` = 还没有词表，**系统状态**
///   * `vN` + `__untyped__` = 有词表但归不上去，**数据信号**（覆盖不足，超阈值即需升版）
pub const UNTYPED: &str = "__untyped__";

/// 词表里的一个类型 —— 四列与 `b_merchant_group_taxonomy` 对齐。
///
/// **词表是两级的，但只有二级是「类型」**：`type_id` / `name` 是叶子，
/// `parent_name` 只是叶子上的一个分组属性。一级不单独成行、不进 `event_type`、
/// 不进任何语义键 —— 它在两个地方起作用：分类 prompt 里分组列出（[`render_system`]），
/// 和报表按一级汇总时 JOIN 词表取它。
///
/// 不含类心或训练状态；当前模型直接依据名称和说明从封闭词表中选择。
///
/// 一个定义同时是「库里长什么样」和「草稿文件长什么样」，两处不会漂。
///
/// ⚠️ **不再 derive `JsonSchema`。** 那是给机器归纳那条线用的（模型直接输出成
/// 这个类型），2026-09-03 归纳舍弃后没有任何模型输出它 —— 词表现在全靠人手写。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyType {
    pub type_id: String,
    /// 一级分类名。逐字进 system prompt，所以它跟 `name` 受同一套校验
    /// （`taxonomy::draft::Draft::check`）。
    pub parent_name: String,
    pub name: String,
    pub description: String,
}

/// 一条 summary 的打标结果 —— **主类 + 全集**。
///
/// 一个事件确实可能同时属于两件事（「要求换人，顺带争议空跑费」），但
/// **指标只按主类算**：`uk_agent_daily` 的语义键里 `event_type` 是一列，
/// 一个事件计进 N 行就会让 `SUM(event_count) > 事件数` —— 客服主管拿它当处理量
/// 会得到一个虚高但看起来正常的数字，跟承重不变量 5 要防的那种错同一个形状。
/// 副类只落 `event_types` 这一列，给 webUI 下钻用，不进任何指标。
///
/// 构造保证**非空**，所以 [`Labels::primary`] 不会 panic。
/// `Ord` 是给 `store::retag_room` 分组用的：重打标要按**整个 Labels** 分组，
/// 按主类分组会把「主类相同、副类不同」的两批 summary 并进一条 UPDATE。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Labels(pub(super) Vec<String>);

impl Labels {
    /// 主类。进 `event_type`、进语义键、进全部指标。
    pub fn primary(&self) -> &str {
        &self.0[0]
    }

    /// 全集，第一个就是主类。落 `event_types` JSON 列。
    pub fn all(&self) -> &[String] {
        &self.0
    }
}

/// 模型这一批返回的 JSON 外壳。
#[derive(JsonSchema, Deserialize, Debug)]
pub(super) struct Assignments {
    pub(super) assignments: Vec<Assignment>,
}

#[derive(JsonSchema, Deserialize, Debug)]
pub(super) struct Assignment {
    /// 段内 1-based 行号，跟 ③ 给模型的 `#N` 同一套 —— 模型全程不接触任何 ID。
    pub(super) index: u32,
    /// 按贴切程度排序，**第一个是主类**。多数事件只有一个。
    pub(super) type_ids: Vec<String>,
}
