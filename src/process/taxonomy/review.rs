//! 审阅报告 —— 拿草稿**真打一遍标**，把结果写成人读的 Markdown。
//!
//! 这一步不是可选的复核，是归纳的一半：模型产出的名字和描述好不好，只有看
//! 「按它分下来每类各接住了什么」才判断得了。人审的时候读的就是这份文件。
//!
//! **试打不是额外开销** —— 答案进 `classify` 的结果缓存，正式 `recompute` 时直接命中。
//! （前提是 `Classifier::new` 把词表顺序归一了，见那里；否则试打和 recompute
//! 各写各的缓存文件，这条就是空话。）
//!
//! 顺带把 `CONTEXT.md` 要求的**未分类率**第一次变成可查数字：
//! 「`vN` + `__untyped__` 占比超过阈值即需升版」，而那个阈值此前没有任何登记处。
//!
//! ⚠️ **未分类率按事件数加权，不按去重后的说法数。** 一条出现 300 次的摘要归不上去，
//! 和一条只出现 1 次的归不上去，对报表的影响差 300 倍。
//!
//! [`summaries`] 和 [`review_draft`] 曾经住在 `taxonomy/run.rs`。搬回来是因为
//! `review_draft` 干的正是试打（造 `Classifier` → 调 [`review`] → 落 `review_vN.md`），
//! 而 `mod.rs` 的文件布局块把「试打的实现」指的就是本文件 —— 同一个概念被门牌号
//! 分成了两半，读一次要跳两趟。`run.rs` 里此前还有个 A 路径（LLM map-reduce）的
//! 整趟归纳编排，2026-09-02 删；B 路径（embedding + HDBSCAN）2026-09-03 删。
//! **词表现在全靠人手写**，剩下的就是下面这两件，都属于试打这一件事。

use super::draft::Draft;
use crate::{
    BoxError, Result,
    config::Config,
    llm::Llm,
    stage::classify::{Classifier, UNTYPED},
    stage::store,
};
use chrono::NaiveDate;
use sqlx::MySqlPool;
use std::{collections::BTreeMap, path::Path};

/// 未分类样例最多列几条 —— 给人看「词表漏了什么」，不是给人看全量。
const UNTYPED_SAMPLES: usize = 40;
/// 每个类列几条代表样例。
const PER_TYPE_SAMPLES: usize = 3;

pub struct Report {
    pub version: String,
    /// 事件总数（按出现次数加权）与去重后的说法数。
    pub total_events: i64,
    pub total_distinct: usize,
    pub rows: Vec<TypeRow>,
    pub untyped_events: i64,
    pub untyped_distinct: usize,
    /// 归不上去的说法，高频在前。**这就是「词表漏了什么」的清单。**
    pub untyped_samples: Vec<(String, i64)>,
}

pub struct TypeRow {
    pub type_id: String,
    pub name: String,
    /// **人审必须看见它。** description 是模型写的，落库之后 `daily` 每轮读回来
    /// 逐字拼进 `classify` 的 system prompt 永久生效 —— 它才是真正喂给模型的东西。
    /// 报告此前只印名字和样例，等于把这条链上唯一的人工关卡蒙着眼睛。
    pub description: String,
    pub events: i64,
    pub distinct: usize,
    pub samples: Vec<String>,
}

impl Report {
    /// 未分类率，按事件加权。总数为 0 时是 0.0（没有事件谈不上覆盖不足）。
    pub fn untyped_share(&self) -> f64 {
        if self.total_events == 0 {
            0.0
        } else {
            self.untyped_events as f64 / self.total_events as f64
        }
    }
}

/// `summaries` 是 `(去重后的 summary, 出现次数)`，高频在前。
pub async fn review(
    classifier: &Classifier,
    summaries: &[(String, i64)],
) -> Result<Report, BoxError> {
    let sums: Vec<&str> = summaries.iter().map(|(s, _)| s.as_str()).collect();
    let tags = classifier.classify(&sums).await?;
    // `classify` 承诺等长，这里钉死它 —— 下面是 `zip`，短一截会**静默截断**，
    // 产出一个看起来完全正常的未分类率（而升版决策唯一的依据就是它）。
    // 另外两个消费 classify 输出的地方（`store::retag_room` / `metrics`）都有这条断言。
    assert_eq!(tags.len(), summaries.len(), "打标结果与入参不等长");

    let mut agg: BTreeMap<&str, (i64, usize, Vec<String>)> = BTreeMap::new();
    let mut untyped_samples = Vec::new();
    let (mut untyped_events, mut untyped_distinct, mut total_events) = (0i64, 0usize, 0i64);
    // 一个事件一个类，所以 `total_events` 恒等于事件总数 —— 未分类率的分母就是它。
    for ((s, n), t) in summaries.iter().zip(&tags) {
        let t = t.type_id();
        total_events += n;
        if t == UNTYPED {
            untyped_events += n;
            untyped_distinct += 1;
            if untyped_samples.len() < UNTYPED_SAMPLES {
                untyped_samples.push((s.clone(), *n));
            }
            continue;
        }
        let e = agg.entry(t).or_insert((0, 0, Vec::new()));
        e.0 += n;
        e.1 += 1;
        // 入参高频在前，所以前几条天然就是这个类最有代表性的说法
        if e.2.len() < PER_TYPE_SAMPLES {
            e.2.push(s.clone());
        }
    }

    let names: BTreeMap<&str, &str> = classifier
        .types()
        .iter()
        .map(|t| (t.type_id.as_str(), t.name.as_str()))
        .collect();
    let mut rows: Vec<TypeRow> = classifier
        .types()
        .iter()
        .map(|t| {
            let (events, distinct, samples) =
                agg.remove(t.type_id.as_str()).unwrap_or((0, 0, Vec::new()));
            TypeRow {
                type_id: t.type_id.clone(),
                name: names
                    .get(t.type_id.as_str())
                    .copied()
                    .unwrap_or("")
                    .to_string(),
                description: t.description.clone(),
                events,
                distinct,
                samples,
            }
        })
        .collect();
    // 多的在前 —— 人审先看高频类对不对，长尾类看不看得完都行
    rows.sort_by(|a, b| b.events.cmp(&a.events).then(a.type_id.cmp(&b.type_id)));

    Ok(Report {
        version: classifier.version().to_string(),
        total_events,
        total_distinct: summaries.len(),
        rows,
        untyped_events,
        untyped_distinct,
        untyped_samples,
    })
}

pub fn render(r: &Report) -> String {
    let mut s = format!(
        "# 词表 {} 审阅报告\n\n\
         - 事件总数（加权）：**{}**，去重后的说法数：{}\n\
         - 类型数：{}，其中**一条都没接住的**：{}\n\
         - **未分类率（按事件加权）：{:.2}%**（{} / {} 事件；去重后 {} 种说法）\n\n\
         > 未分类率就是「`{}` + `{}` 占比」—— 超过阈值即需升版。\
         上线前必须把那个阈值定死一个数写进 `CONTEXT.md`，否则没人会去看。\n\n\
         ## 各类型命中\n\n\
         > `description` 那一列**就是喂给分类模型的原话**（它逐字进 system prompt）。\
         看着它读样例：样例归得不对，多半是这句话没写清楚。\n\n\
         | 类型 | 名字 | 描述（进 prompt 的原话） | 事件数 | 占比 | 说法数 | 代表样例 |\n\
         |---|---|---|---:|---:|---:|---|\n",
        r.version,
        r.total_events,
        r.total_distinct,
        r.rows.len(),
        r.rows.iter().filter(|x| x.events == 0).count(),
        r.untyped_share() * 100.0,
        r.untyped_events,
        r.total_events,
        r.untyped_distinct,
        r.version,
        UNTYPED,
    );
    for x in &r.rows {
        let share = if r.total_events == 0 {
            0.0
        } else {
            x.events as f64 / r.total_events as f64 * 100.0
        };
        s.push_str(&format!(
            "| `{}` | {} | {} | {} | {:.1}% | {} | {} |\n",
            x.type_id,
            x.name,
            // `|` 会把 Markdown 表格拆列。description 是模型写的，不能假设它没有
            x.description.replace('|', "\\|"),
            x.events,
            share,
            x.distinct,
            x.samples.join("；")
        ));
    }
    s.push_str(&format!(
        "\n## 归不上去的（高频在前，最多 {UNTYPED_SAMPLES} 条）\n\n\
         **这就是「词表漏了什么」的清单。** 读它，不是读上面那张表。\n\n"
    ));
    if r.untyped_samples.is_empty() {
        s.push_str("（没有）\n");
    } else {
        for (x, n) in &r.untyped_samples {
            s.push_str(&format!("- {x}（{n} 次）\n"));
        }
        if r.untyped_distinct > r.untyped_samples.len() {
            s.push_str(&format!(
                "\n…还有 {} 种没列出来。\n",
                r.untyped_distinct - r.untyped_samples.len()
            ));
        }
    }
    s
}

/// 写词表时的参考 —— `(去重后的 summary, 出现次数)`，高频在前。
/// 人手写 `taxonomy_vN.toml` 之前先看这个：**有哪些说法、各出现多少次**。
///
/// 这是个直通函数：`store` 是 `pub(crate)`（「写库 SQL 一条不许外流」写在可见性上），
/// 而 `src/bin/taxonomy.rs` 的 `summaries` 子命令要拿这批数字给人看。
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
    let report = review(&classifier, sums).await?;
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
    std::fs::write(&path, render(&report))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(untyped: i64, total: i64) -> Report {
        Report {
            version: "v1".into(),
            total_events: total,
            total_distinct: 2,
            rows: vec![TypeRow {
                type_id: "cancel_order".into(),
                name: "取消订单".into(),
                description: "商家要求取消已下的安装单".into(),
                events: total - untyped,
                distinct: 1,
                samples: vec!["商家要求取消".into()],
            }],
            untyped_events: untyped,
            untyped_distinct: 1,
            untyped_samples: vec![("怪事一桩".into(), untyped)],
        }
    }

    /// 未分类率按**事件**加权，不按说法数 —— 这两个数在真实数据上差得很远。
    #[test]
    fn the_untyped_share_is_weighted_by_events_not_by_distinct_phrasings() {
        // 1 种说法 × 300 次未分类 / 共 400 事件 = 75%，而按说法数算是 50%
        assert!((rep(300, 400).untyped_share() - 0.75).abs() < 1e-9);
        assert_eq!(rep(0, 0).untyped_share(), 0.0, "没有事件时不该除零");
    }

    #[test]
    fn the_report_leads_with_the_untyped_share_and_lists_the_gaps() {
        let md = render(&rep(300, 400));
        assert!(md.contains("未分类率（按事件加权）：75.00%"), "{md}");
        assert!(md.contains("怪事一桩（300 次）"));
        assert!(md.contains("`cancel_order`"));
        // description 必须印出来 —— 它是真正喂给模型的东西，人审看不见就等于没有关卡
        assert!(md.contains("商家要求取消已下的安装单"), "{md}");
    }
}
