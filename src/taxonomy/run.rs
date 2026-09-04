//! 归纳与人审之间**共用**的两件事：取 summary，拿草稿试打并落 review.md。
//! （跟 `main` / `daily` 同一条规矩：编排住在 lib 里，不住在入口文件里。）
//!
//! ⚠️ 原来这里还有一个 `run()`，是 A 路径（LLM map-reduce）整趟归纳的编排。
//! A 2026-09-02 删、B（embedding + HDBSCAN）2026-09-03 删 —— 机器归纳整条线
//! 都放弃了。**词表现在全靠人手写**，所以剩下的只有这两件。

use super::{draft::Draft, review};
use crate::{Result, classify::Classifier, config::Config, llm::Llm, store};
use chrono::NaiveDate;
use sqlx::MySqlPool;
use std::path::Path;

/// 写词表时的参考 —— `(去重后的 summary, 出现次数)`，高频在前。
/// 人手写 `taxonomy_vN.toml` 之前先看这个：**有哪些说法、各出现多少次**。
///
/// 这是个直通函数：`store` 是 `pub(crate)`（「写库 SQL 一条不许外流」写在可见性上），
/// 而 `examples/taxonomy.rs` 的 `summaries` 子命令要拿这批数字给人看。
/// 与其为它把整个 `store` 放出去，不如在这里开一个只读的口子。
pub async fn summaries(
    pool: &MySqlPool,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<Vec<(String, i64)>> {
    store::read_summary_counts(pool, since, until).await
}

/// 拿一份草稿真打一遍标，落一个 `review_<version>.md`，返回它的路径。
///
/// **试打不是额外开销**：答案进结果缓存，正式 `recompute` 时直接命中。
/// ⚠️ 这句话**依赖 `Classifier::new` 归一词表顺序** —— 缓存文件名是渲染好的
/// prompt 的指纹，而这里传的是草稿原序、`recompute` 那边是库里的 `ORDER BY`。
/// 归一那一行没了，两边就各写各的缓存文件，这句话当场变成假的。
pub async fn review_draft(
    config: &Config,
    llm: &Llm,
    draft: &Draft,
    sums: &[(String, i64)],
    out_dir: &Path,
) -> Result<std::path::PathBuf> {
    let classifier = Classifier::new(
        &draft.version,
        draft.types.clone(),
        llm.clone(),
        &config.classify.cache_dir,
    )?;
    let report = review::review(&classifier, sums).await?;
    tracing::info!(
        untyped_pct = format!("{:.2}", report.untyped_share() * 100.0),
        empty_types = report.rows.iter().filter(|r| r.events == 0).count(),
        "试打完成"
    );
    let path = out_dir.join(format!("review_{}.md", draft.version));
    if let Some(p) = path.parent()
        && !p.as_os_str().is_empty()
    {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, review::render(&report))?;
    Ok(path)
}
