//! 词表加载守卫 —— **数据库与人工草稿走同一份校验**。
//!
//! 调用方有三个：`Classifier::new`（库里读回来的）、`taxonomy::draft::Draft::check`
//! （人手写的 TOML）、`store::read_taxonomy`。三处共用这一份，词表的形态契约就只有一处。
//!
//! 拦的多数不是 DDL 能拦的东西，而是**自举注入**：`name` / `parent_name` /
//! `description` 都会逐字进 `model::render_system` 的 system prompt，
//! 一个换行就能在封闭列表中间另起一行冒充规则。

use super::types::{TaxonomyType, UNTYPED};
use crate::BoxError;
use std::collections::BTreeSet;

/// 并列词 —— 名字里出现其中任何一个，就说明这个类装了不止一件事。
///
/// 「取消订单与换人」这种名字一旦进了词表，指标里那两件事就**永远分不开** ——
/// 它是语义键的一列，拆开等于升版重打标。所以在这里拒，不在下游补救。
const COMPOSITE: &[char] = &['与', '和', '及', '、', '/', '／'];

/// description 的字数上限。库里是 `TEXT`，管不住任何东西 —— 这个上限管的是
/// **它会逐字进 `classify` 的 system prompt**（`render_system`）：模型写的描述落库之后，
/// `daily` 每轮读回来拼进 prompt 永久生效。一句话说清「什么该归到这里」用不了 200 字，
/// 而没有上限时它可以长到把封闭列表的规则挤到模型注意力之外。
pub(crate) const DESC_MAX: usize = 200;

/// `version` 的形态白名单。**不是长度校验。**
///
/// 那个 16 是 `VARCHAR(16)` 的列宽，不是安全边界：`"a\nDROP TABLE t;#"` 也只有
/// 16 字符，而 `taxonomy::to_sql` 的表头把 version 逐字拼进 `-- 词表 {v}` 那行注释 ——
/// 换行一闭合，下一行就是可执行的 SQL，`#` 又把行尾吃掉让它自我闭合，
/// 而产物是人拿建表账号 `mysql < taxonomy_v1.sql` 执行的，看不到任何报错。
///
/// 同一条白名单顺带关掉另外两个口子：`taxonomy_<version>.toml` /
/// `review_<version>.md` 的产物路径穿越，和 `<version>-<指纹>.ndjson` 的缓存路径穿越。
pub(crate) fn check_version(version: &str) -> Result<(), BoxError> {
    if version.is_empty()
        || version.len() > 16
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        return Err(format!(
            "version「{version}」形态不合法：只允许 ASCII 字母/数字/下划线/点，\
             1~16 字节（库里是 VARCHAR(16)，且它会进 SQL 文本和文件名）"
        )
        .into());
    }
    Ok(())
}

pub(crate) fn check_types(types: &[TaxonomyType]) -> Result<(), BoxError> {
    if types.is_empty() {
        return Err("词表一个类都没有，请确认指定版本已入库；只有显式 v0 允许空词表".into());
    }
    // **逐类的问题一次报全，不是遇到第一个就停。** 20 多个类的词表里改一个跑一次，
    // 两个坏名字就要三轮才过 —— 一次看完全部问题改一遍便宜得多。
    let mut bad: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for t in types {
        if t.type_id == UNTYPED {
            bad.push(format!(
                "type_id 不能是 {UNTYPED} —— 它是「归不上去」的保留值，不是一个类"
            ));
        }
        // 库里是 VARCHAR(64)，且它会进 uk_agent_daily 六列语义键 —— 形态要稳。
        if t.type_id.is_empty()
            || t.type_id.len() > 64
            || !t
                .type_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            bad.push(format!(
                "type_id「{}」形态不合法：只允许小写字母/数字/下划线，1~64 字节",
                t.type_id
            ));
        }
        if !seen.insert(&t.type_id) {
            bad.push(format!("type_id「{}」重复", t.type_id));
        }
        if t.name.trim().is_empty() || t.name.chars().count() > 128 {
            bad.push(format!("「{}」的 name 要非空且 ≤128 字", t.type_id));
        }
        if t.name.contains(['\n', '\r']) {
            bad.push(format!("「{}」的 name 含换行，必须写成一行", t.type_id));
        }
        // 一级分类名。库里 VARCHAR(64)，而它在 prompt 里是 `## {parent_name}` 那行 ——
        // **换行比 description 的换行更危险**：它能整段伪造分组结构，甚至在两个
        // 一级之间另起一行冒充规则。所以这里跟 name 一样查并列词，另外必须拒换行。
        if t.parent_name.trim().is_empty() || t.parent_name.chars().count() > 64 {
            bad.push(format!("「{}」的 parent_name 要非空且 ≤64 字", t.type_id));
        } else if t.parent_name.contains(['\n', '\r']) {
            bad.push(format!(
                "「{}」的 parent_name 含换行 —— 它是分类 prompt 里的分组标题行，\
                 换行会让它看起来像另一个分组或另一条规则。写成一行",
                t.type_id
            ));
        } else if let Some(c) = t.parent_name.chars().find(|c| COMPOSITE.contains(c)) {
            bad.push(format!(
                "「{}」的 parent_name「{}」含并列词「{c}」—— 一级分类也只表达一件事。\
                 请换一个能概括的单一名字",
                t.type_id, t.parent_name
            ));
        }
        if let Some(c) = t.name.chars().find(|c| COMPOSITE.contains(c)) {
            // 首选动作是「换个名字」不是「拆成两个类」：「修改联系与地址信息」
            // → 「修改订单信息」换个词就完事，类数根本不用动。
            bad.push(format!(
                "「{}」的 name「{}」含并列词「{c}」—— 一个类只表达一件事。\
                 请换一个能概括这件事的单一名字（**别改类的数量**）；\
                 实在概括不了，才拆成两个类",
                t.type_id, t.name
            ));
        }
        // description 必填是 DDL 写死的：它让 classify 不依赖向量也能工作。
        //
        // ⚠️ 上限和「不许换行」拦的不是 DDL（那是 TEXT），是**自举注入**：这段文本
        // 落库之后，`daily` 每轮读回来逐字拼进 `classify` 的 system prompt 永久生效。
        // 一个带换行的 description 能在封闭列表中间另起一行冒充规则；一个超长的
        // description 能把真正的规则挤出模型的注意力。爆炸半径有界（`validate`
        // 是封闭集合，造不出新标签），但**系统性错标**恰好会污染未分类率 ——
        // 而升版决策唯一的依据就是它。
        if t.description.trim().is_empty() {
            bad.push(format!("「{}」的 description 不能为空", t.type_id));
        } else if t.description.chars().count() > DESC_MAX {
            bad.push(format!(
                "「{}」的 description 有 {} 字，超过 {DESC_MAX} —— 它会逐字进分类 prompt，\
                 一句话说清「什么该归到这个类」就够了",
                t.type_id,
                t.description.chars().count()
            ));
        } else if t.description.contains(['\n', '\r']) {
            bad.push(format!(
                "「{}」的 description 含换行 —— 它会逐字进分类 prompt 的封闭列表，\
                 换行会让它看起来像另一条规则。写成一行",
                t.type_id
            ));
        }
    }
    if !bad.is_empty() {
        return Err(bad.join("\n").into());
    }
    Ok(())
}
