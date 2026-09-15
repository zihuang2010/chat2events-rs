//! ⑤ 的领域类型 —— 词表的一个类、一条打标结果、模型这一批的输出外壳。
//!
//! 三样都不认识 prompt、缓存和 MySQL：`TaxonomyType` 是 `b_merchant_group_taxonomy`
//! 的四列，`Label` 是一条打标结果，`Assignment` 是模型答复里的一行。

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

/// 一条 summary 的打标结果 —— **一个类**，落 `event_type` 那一列。
///
/// ⚠️ **曾经是多标签**（主类 + 全集，一个事件最多挂 3 个类，副类落 `event_types`
/// JSON 列）。2026-09-14 整套拿掉：副类不进任何指标、不进任何筛选、不进任何索引，
/// 只在事件抽屉里显示一行，却牵着 prompt 的两条规则、`validate` 的四条守卫、
/// 缓存的数组格式和 `write_labels` 的分组理由。**要重新加回来，先想清楚
/// `uk_agent_daily` 那一列**：一个事件计进 N 行就会让 `SUM(event_count) > 事件数`，
/// 客服主管拿它当处理量会得到一个虚高但看起来正常的数字 —— 跟承重不变量 5
/// 要防的那种错同一个形状。副类当年不进指标正是因为这个。
///
/// 构造保证 `type_id` 属于「词表 ∪ `{__untyped__}`」—— [`super::model::validate`]
/// 是唯一入口，下游拿到 `Label` 不必再校验一遍。
/// `Ord` 是给 `store::retag_room` 分组用的：同一个类的行并成一条 UPDATE。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Label(pub(super) String);

impl Label {
    /// 进 `event_type`、进语义键、进全部指标。
    pub fn type_id(&self) -> &str {
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
    /// 这一行归哪个类。**单值**：它进 JsonSchema，所以「一行给了两个类」在结构化
    /// 输出里根本表达不出来 —— 那四条多标签守卫因此不是删掉了，是不可表达了。
    pub(super) type_id: String,
}
