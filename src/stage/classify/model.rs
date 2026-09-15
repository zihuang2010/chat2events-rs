//! 分类的模型协议 —— 词表 → system prompt → 一批请求 → 校验。
//!
//! [`Classifier`] 一次运行构造一次，拿住词表、渲染好的 prompt、模型与缓存。
//! **没有 trait 是有意的**（见模块头）：v0 不需要第二个实现。
//!
//! [`validate`] 的报错文案是**回灌进下一轮 prompt 给模型读的**，改文案等于改 prompt；
//! `cache` 重新发布持久答案时也走它，所以它是 `pub(super)`。

use super::{
    cache::{Cache, digest, hex},
    check::{check_types, check_version},
    types::{Assignment, Assignments, Label, TaxonomyType, UNTYPED},
};
use crate::{
    BoxError,
    llm::{Llm, LlmError, Turn},
    rejection::Rejection,
};
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::Arc,
};
use tokio::sync::Mutex;

/// 一次请求塞几条 summary。走常量不走配置，跟 `store::BATCH` 一个规矩 ——
/// 没有第二个用例，配一个永远只有一个取值的旋钮是空的。
///
/// 50 条 × 每条 ≤100 字，输出是 50 个 `{index, type_id}` 小对象，对着
/// `[llm.classify].max_tokens`（6000）有 3 倍余量。**这里不做自适应二分**（那套是
/// ③ 的）：真撞上截断说明别的地方坏了，该显式失败而不是切了继续跑。
///
/// ⚠️ **调大它要同步核对 `[llm.classify].max_tokens`** —— 那个数是按这里的 50 算的。
pub(crate) const BATCH: usize = 50;

/// 允许一次「编了个不存在的 type_id / 漏了一行」的自我修正，不多给 —— 跟
/// `extract::model` 同一个数、同一个理由：逼急了模型会挑一个看起来合法的乱填。
const MAX_RETRIES: u32 = 1;

/// 一次运行构造一次。拿住词表、模型与缓存。
pub struct Classifier {
    version: String,
    types: Vec<TaxonomyType>,
    /// 合法答案集 = 词表全部 `type_id` ∪ `{__untyped__}`。校验用。
    known: BTreeSet<String>,
    /// 构造时渲染一次 —— 它只依赖词表，而词表一次运行内不变。
    system: String,
    llm: Llm,
    pub(super) cache: Arc<Mutex<Cache>>,
}

impl Classifier {
    /// 缓存按版本、词表、提示词及模型请求配置隔离；构造时固定本轮分类策略。
    pub fn new(
        version: &str,
        mut types: Vec<TaxonomyType>,
        llm: Llm,
        cache_dir: &Path,
    ) -> Result<Self, BoxError> {
        check_version(version)?;
        if version == "v0" {
            if !types.is_empty() {
                return Err("v0 表示未建词表，不能包含分类定义".into());
            }
        } else {
            check_types(&types).map_err(|e| format!("词表 {version} 校验失败：{e}"))?;
        }
        // ⚠️ **输出上限不再在这里钳位。** 此前这一行是 `llm.with_max_tokens(6000)` ——
        // 无条件压小，config 写什么都不算数。那让 `[llm]` 里那个数变成了谎言，
        // 而这个仓库的规矩是「配置错要在第一秒炸，不许被代码悄悄修正」。
        // 现在唯一的真相是 `[llm.classify].max_tokens`，论证搬进了 config.toml，
        // 而「打标预算必须小于抽取预算」由 `config::load_from_dir` 的断言在启动期守着。
        // **调用方有义务传 classify 那份 `Llm`** —— 传成抽取那份会带着抽取的输出预算
        // 出门，跑飞要多烧一倍时间才撞顶。
        //
        // **顺序归一在这里，不在调用方。** 指纹取的是渲染好的 prompt，而 `render_system`
        // 是按给定顺序逐行拼的 —— 顺序变一位，指纹就变，缓存就换一个文件。这条不变量
        // 曾经只钉在 `store::read_taxonomy` 的 `ORDER BY type_id` 上，而本函数有四个
        // 生产调用点，两个不走库（`taxonomy::review_draft` / 对拍工具）传的是草稿原序：
        // 试打写 `v1-AAAA.ndjson`、正式 recompute 读 `v1-BBBB.ndjson`，
        // 「试打不是额外开销，答案进缓存 recompute 直接命中」那句话就永远不成立。
        // 按 (一级, type_id) 排 —— `render_system` 按一级分组列出，同组必须连续。
        // 只按 type_id 排的话，`urge_accept` 和 `urge_dispatch` 中间会插进别的一级。
        types.sort_by(|a, b| (&a.parent_name, &a.type_id).cmp(&(&b.parent_name, &b.type_id)));
        let known: BTreeSet<String> = types
            .iter()
            .map(|t| t.type_id.clone())
            .chain([UNTYPED.to_string()])
            .collect();
        let system = render_system(&types);
        let fingerprint = digest(&serde_json::to_string(&(&system, llm.cache_identity()?))?);
        let fp = hex(&fingerprint);
        let cache = Cache::open(&cache_dir.join(format!("{version}-{fp}.ndjson")), &known)?;
        tracing::info!(
            taxonomy_version = version,
            model = llm.model_name(),
            prompt_fingerprint = %hex(&digest(&system)),
            taxonomy_fingerprint = %hex(&digest(&serde_json::to_string(&types)?)),
            policy_fingerprint = %fp,
            "大模型分类策略就绪"
        );
        Ok(Self {
            version: version.to_string(),
            types,
            known,
            system,
            llm,
            cache: Arc::new(Mutex::new(cache)),
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// 词表里有几个类。0 = v0（还没有词表）。
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// 词表本体 —— `taxonomy::review` 要拿 `type_id` 换中文名写进审阅报告。
    pub fn types(&self) -> &[TaxonomyType] {
        &self.types
    }

    /// 数据库标签必须与当前策略的持久答案一致，不能把旧模型答案灌进新策略缓存。
    pub(crate) async fn check_saved_answers(
        &self,
        answers: &[(&str, Label)],
    ) -> Result<(), BoxError> {
        if self.types.is_empty() {
            return Ok(());
        }
        for chunk in answers.chunks(BATCH) {
            let keys: Vec<_> = chunk.iter().map(|(summary, _)| digest(summary)).collect();
            let cache = self.cache.clone();
            let accepted = tokio::task::spawn_blocking(move || {
                let cache = cache.blocking_lock();
                keys.iter()
                    .map(|key| cache.get(key))
                    .collect::<Result<Vec<_>, _>>()
            })
            .await??;
            if accepted
                .iter()
                .zip(chunk)
                .any(|(accepted, (_, saved))| accepted.as_ref() != Some(saved))
            {
                return Err(
                    "已保存标签与策略缓存不符或缓存缺失，停止补标，请核对配置并恢复原策略缓存"
                        .into(),
                );
            }
        }
        Ok(())
    }

    /// 恢复只补尚未完成的标签；已发布批次必须保留原答案，不能按新策略悄悄重问。
    pub(crate) fn saved_labels(
        &self,
        type_id: Option<String>,
        version: Option<String>,
    ) -> Result<Option<Label>, BoxError> {
        match (type_id, version) {
            (None, None) => Ok(None),
            (Some(type_id), Some(version)) if version == self.version => Ok(Some(
                validate(vec![Assignment { index: 1, type_id }], 1, &self.known)
                    .map_err(|_| "已保存标签不满足当前词表契约，请显式重打标")?
                    .pop()
                    .expect("单个合法标签"),
            )),
            _ => Err("标签列不完整或词表版本不一致，请使用词表升版重打标".into()),
        }
    }

    /// 给一批 summary 打标，返回**与入参一一对应、等长**的标签。
    ///
    /// 失败即 `Err`，不生成兜底标签。日常跑批由独立打标队列调度每个批次；
    /// 人工试打和重打标可传入多批摘要，仍复用同一套去重、缓存和校验。
    pub async fn classify(&self, summaries: &[&str]) -> Result<Vec<Label>, BoxError> {
        // v0：还没有词表。不查缓存、不发请求、不写文件。
        if self.types.is_empty() {
            return Ok(vec![Label(UNTYPED.to_string()); summaries.len()]);
        }

        let keys: Vec<[u8; 32]> = summaries.iter().map(|s| digest(s)).collect();
        let mut answers: HashMap<[u8; 32], Label> = HashMap::new();
        // 缓存没有的，**按内容去重**再问 —— 同一批里重复的 summary 只占一个请求位。
        let mut todo: Vec<([u8; 32], &str)> = Vec::new();
        {
            // 历史答案按键读盘，查询和提交都放在阻塞线程；模型请求始终在锁外。
            let cache = self.cache.clone();
            let lookup = keys.clone();
            let mut cached = tokio::task::spawn_blocking(move || {
                let c = cache.blocking_lock();
                lookup
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|key| Ok((key, c.get(&key)?)))
                    .collect::<Result<HashMap<_, _>, BoxError>>()
            })
            .await??;
            // 同一批只处理每个摘要一次，命中缓存与否都共用这份去重记录。
            let mut seen = BTreeSet::new();
            for (k, s) in keys.iter().zip(summaries) {
                if !seen.insert(*k) {
                    continue;
                }
                match cached.remove(k).flatten() {
                    Some(t) => {
                        answers.insert(*k, t);
                    }
                    None => todo.push((*k, s)),
                }
            }
        }

        if !todo.is_empty() {
            tracing::debug!(
                total = summaries.len(),
                hit = answers.len(),
                miss = todo.len(),
                "打标缓存"
            );
        }
        for chunk in todo.chunks(BATCH) {
            let batch: Vec<&str> = chunk.iter().map(|(_, s)| *s).collect();
            let got = self.ask(&batch).await?;
            let entries: Vec<_> = chunk.iter().map(|(key, _)| *key).zip(got).collect();
            let cache = self.cache.clone();
            let accepted =
                tokio::task::spawn_blocking(move || cache.blocking_lock().commit(entries))
                    .await
                    .expect("缓存文件错误通过 Result 返回")?;
            for ((key, _), label) in chunk.iter().zip(accepted) {
                answers.insert(*key, label);
            }
        }

        Ok(keys
            .iter()
            .map(|k| {
                answers
                    .get(k)
                    .expect("构造保证：每个 key 要么命中缓存，要么刚被这一轮填上")
                    .clone()
            })
            .collect())
    }

    /// 一批的真实请求 —— 校验不过就回灌报错重问一次，仍不过则该批次失败。
    /// 形状照抄 `extract::model::LiveModel::call`，理由也一样：报错文案是给**模型**
    /// 读的，它要照着自我修正。
    async fn ask(&self, batch: &[&str]) -> Result<Vec<Label>, BoxError> {
        let listing: String = batch
            .iter()
            .enumerate()
            .map(|(i, s)| format!("#{} {}\n", i + 1, s))
            .collect();
        let mut turns = vec![Turn::User(listing)];
        let mut attempt = 0u32;
        loop {
            // `extract_retry`：跑飞 / 超时当坏运气重发（③ 才把它们当切分信号，这里不是）。
            let got: crate::llm::Extracted<Assignments> = self
                .llm
                .extract_retry(&self.system, &turns)
                .await
                .map_err(|e| {
                    // 重发额度烧完仍截断 = 真不是坏运气了；保住它的名字，
                    // 别退化成一次「JSON 解析失败」。
                    match e {
                        LlmError::Truncated => BoxError::from(
                            "打标输出重发后仍被截断 —— BATCH 对 \
                             [llm.classify].max_tokens 的余量算错了，不是数据问题",
                        ),
                        other => Box::new(other),
                    }
                })?;

            match validate(got.data.assignments, batch.len(), &self.known) {
                Ok(v) => return Ok(v),
                Err(msg) if attempt < MAX_RETRIES => {
                    // 静默重试等于不知道模型在编 type_id。这条 warn 是唯一的信号。
                    tracing::warn!(
                        n = batch.len(),
                        attempt,
                        "打标输出没过校验，回灌报错重问：{msg}"
                    );
                    turns.push(Turn::Assistant(got.raw));
                    turns.push(Turn::User(format!(
                        "上一轮的输出没通过校验：\n{}\n\n请按上面的报错修正，重新输出全部行的分类。",
                        msg.verbatim()
                    )));
                    attempt += 1;
                }
                Err(msg) => {
                    return Err(format!("打标校验重试 {MAX_RETRIES} 次后仍不通过：{msg}").into());
                }
            }
        }
    }
}

/// 词表 -> system prompt。**封闭列表**：允许的答案全在里面，`__untyped__` 也是其中之一。
fn render_system(types: &[TaxonomyType]) -> String {
    let mut s = String::from(
        "你是一个事件分类器。下面是**封闭**的事件类型词表，按一级分类分组，你只能从中选择：\n\n",
    );
    // **依赖 `Classifier::new` 已按 (parent_name, type_id) 排好序** —— 同组连续，
    // 所以这里只要在一级变化时插一个标题，不用建中间的分组结构。
    let mut cur = "";
    for t in types {
        if t.parent_name != cur {
            // 组间空一行，但第一组前不空 —— 抬头那行后面已经有一个空行了
            let gap = if cur.is_empty() { "" } else { "\n" };
            s.push_str(&format!("{gap}## {}\n", t.parent_name));
            cur = &t.parent_name;
        }
        s.push_str(&format!(
            "- {} | {}：{}\n",
            t.type_id, t.name, t.description
        ));
    }
    s.push_str(&format!(
        "\n- {UNTYPED} | 未归类：以上类型都不合适时用它\n\n\
         用户会给你若干行事件摘要，每行形如 `#N 摘要正文`，N 从 1 开始连续编号。\n\
         为**每一行**输出一个 type_id，以 index=N 输出。\n\n\
         规则：\n\
         1. type_id 必须逐字来自上面的列表，不要改写、不要翻译、不要发明新的。\n\
            **`##` 开头的是一级分类，只用来分组，本身不是可选答案** —— 先看摘要属于\n\
            哪个一级，再在它下面挑一个二级 type_id。\n\
         2. **每行只有一个 type_id。** 一条摘要如果讲了不止一件事，只给最主要的\n\
            那一件 —— 挑「这个事件本质上是什么」，不是「提到过什么」。\n\
         3. 归不上去就用 {UNTYPED} —— 硬塞进一个不合适的类，比承认归不上去更糟。\n\
         4. 每一行都必须有结果，不许漏行、不许合并、不许多给行。\n\
         5. 只看摘要本身描述的**事情是什么**，不要因为句式相近就归成一类。\n"
    ));
    s
}

/// 校验模型这一批的输出。**不通过 = 该批次失败**，不做字段级兜底修补。
///
/// 报错文案是回灌进下一轮 prompt 给模型读的，改文案等于改 prompt。
///
/// 三条规则各自的理由：
///   * **行号越界 / 重复** —— 模型只看到 `#N`，越界即编造（跟承重不变量 6 同一个形状）。
///   * **漏行** —— 「没算出来」绝不许表现成一个正常取值（承重不变量 4）。
///   * **未知 type_id** —— 编造不是「归不上去」。映射成 `__untyped__` 会污染
///     「`vN` + `__untyped__` 占比」这个信号，而升版决策正是看它。
///
/// ⚠️ 这里曾经还有四条多标签守卫（空列表 / 超过上限 / 行内重复 / `__untyped__`
/// 混着别的类）。多标签拿掉之后 [`Assignment::type_id`] 是单值，四种情形在结构化
/// 输出里**表达不出来** —— 它们不是被放行了，是不存在了。
pub(super) fn validate(
    assignments: Vec<Assignment>,
    n: usize,
    known: &BTreeSet<String>,
) -> Result<Vec<Label>, Rejection> {
    let mut errs: Vec<(&'static str, String)> = Vec::new();
    let mut got: HashMap<usize, Label> = HashMap::new();
    for a in assignments {
        let i = a.index as usize;
        if !(1..=n).contains(&i) {
            errs.push(("序号越界", format!("index {i} 超出本批范围 1-{n}")));
            continue;
        }
        if !known.contains(&a.type_id) {
            errs.push((
                "未知 type_id",
                format!(
                    "#{i} 的 type_id「{}」不在词表里；只能用列出的那些，归不上去请填 {UNTYPED}",
                    a.type_id
                ),
            ));
            continue;
        }
        if got.insert(i, Label(a.type_id)).is_some() {
            errs.push(("序号重复", format!("#{i} 给了不止一行结果，每行只要一行")));
        }
    }
    let missing: Vec<String> = (1..=n)
        .filter(|i| !got.contains_key(i))
        .map(|i| format!("#{i}"))
        .collect();
    if !missing.is_empty() {
        errs.push((
            "缺行",
            format!(
                "漏了 {} 行：{}。每一行都必须有结果",
                missing.len(),
                missing.join(" ")
            ),
        ));
    }
    if errs.is_empty() {
        Ok((1..=n)
            .map(|i| got.remove(&i).expect("上面刚查过没有缺行"))
            .collect())
    } else {
        Err(Rejection::new(errs))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{asg, flat, known, ty, ty_under};
    use super::*;

    /// 词表按一级分组进 prompt：同一个一级下的类连续排在一个 `##` 标题下，
    /// 一级出现几次就有几个标题 —— 排序归一没做对的话这里会多出重复标题。
    #[test]
    fn the_prompt_groups_types_under_their_parent() {
        let s = render_system(&{
            let mut t = vec![
                ty_under("履约催促", "urge_dispatch"),
                ty_under("服务变更", "add_item"),
                ty_under("履约催促", "urge_accept"),
            ];
            t.sort_by(|a, b| (&a.parent_name, &a.type_id).cmp(&(&b.parent_name, &b.type_id)));
            t
        });
        assert_eq!(s.matches("## 履约催促").count(), 1, "{s}");
        assert_eq!(s.matches("## 服务变更").count(), 1, "{s}");
        // 同一级下的两个类之间不许插进另一个一级的标题
        let (a, b) = (
            s.find("urge_accept").unwrap(),
            s.find("urge_dispatch").unwrap(),
        );
        assert!(!s[a.min(b)..a.max(b)].contains("##"), "{s}");
        // 一级只是分组标题，不能被当成可选答案 —— prompt 要明说
        assert!(s.contains("`##` 开头的是一级分类"), "{s}");
    }

    /// 编造 / 漏行 / 越界 / 重复，四种都必须是失败，**不许兜底成 `__untyped__`**：
    /// 那会拿「没算出来」冒充一个正常取值（承重不变量 4），并污染升版判据。
    #[test]
    fn fabrication_gaps_and_duplicates_all_fail_validation() {
        let k = known();
        assert_eq!(
            flat(validate(asg(&[(1, "a"), (2, UNTYPED)]), 2, &k)),
            ["a", UNTYPED]
        );
        assert!(
            validate(asg(&[(1, "a"), (2, "zzz")]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("不在词表里")
        );
        assert!(
            validate(asg(&[(1, "a")]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("漏了 1 行：#2")
        );
        assert!(
            validate(asg(&[(1, "a"), (3, "b")]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("超出本批范围 1-2")
        );
        assert!(
            validate(asg(&[(1, "a"), (1, "b"), (2, "a")]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("不止一行结果")
        );
    }

    /// 结果按 index 归位，不按模型给的顺序 —— 乱序返回是允许的。
    #[test]
    fn results_are_ordered_by_index_not_by_reply_order() {
        assert_eq!(
            flat(validate(
                asg(&[(3, "b"), (1, "a"), (2, UNTYPED)]),
                3,
                &known()
            )),
            ["a", UNTYPED, "b"]
        );
    }

    /// system prompt 必须把 `__untyped__` 也列成合法答案 —— 漏了它，模型就只能
    /// 在真实类型里硬选一个，「归不上去」这个信号会永远为 0。
    #[test]
    fn the_system_prompt_offers_untyped_as_a_legal_answer() {
        let s = render_system(&[ty("cancel")]);
        assert!(s.contains("cancel | cancel：描述"));
        assert!(s.contains(UNTYPED));
        // **单标签也必须在 prompt 里写明**。JsonSchema 那边已经是单值，模型给不出
        // 第二个类，但不明说的话它会把两件事**揉进一个 type_id 的选择里犹豫** ——
        // 规则要的是「挑最主要的那件」，那是个取舍指令，schema 表达不了。
        assert!(s.contains("每行只有一个 type_id"), "{s}");
        assert!(
            !s.contains("主类"),
            "多标签的措辞不该再出现在 prompt 里：{s}"
        );
    }
}
