//! ⑤ 分类 classify —— `summary` ＋ 词表 -> [`Labels`]（主类 + 全集）。
//!
//! 一个事件可以同时属于两件事，但**指标只按主类算** —— 理由见 [`Labels`]。
//!
//! **确定性是硬约束**：同样的摘要与分类策略复用已缓存标签。
//! 事实保存后由独立队列安排打标；标签不属于 `Event` 事实类型。
//!
//! ⚠️ **确定性由缓存保证，不由算法保证。** v1 的算法是「让模型从封闭词表里选」，
//! 而 `temperature = 0` 并不保证同输入同输出。非冻结区每天重写 `[T-3, T-2]`，
//! 同一批 event 会被反复打标 —— 没有跨运行的持久缓存，报表就会**抖动而非修正**，
//! 正是承重不变量 1 要防的那件事。**[`Cache`] 是承重件，不是优化。**
//!
//! 模型调用不持数据库事务；`daily/classify.rs` 调度批次并回写标签，全部成功后计算指标。
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

use crate::{
    BoxError,
    llm::{Llm, LlmError, Turn},
    rejection::Rejection,
};
use rusqlite::OptionalExtension;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read},
    path::Path,
    sync::Arc,
};
use tokio::sync::Mutex;

/// 当前词表版本。**v0 不是缺陷，是明确的上线阶段** —— 系统在任何阶段都能完整跑通，
/// 不需要等词表。升版是人工动作（插词表行 → 改这个常量 → 跑 `examples/recompute.rs`）。
/// 2026-09-03 走到 **v1**：43 个二级类 × 9 个一级，词表人手写。
pub const CURRENT_VERSION: &str = "v1";

/// 两种 `__untyped__` 严格区分：
///   * `v0` + `__untyped__` = 还没有词表，**系统状态**
///   * `vN` + `__untyped__` = 有词表但归不上去，**数据信号**（覆盖不足，超阈值即需升版）
pub const UNTYPED: &str = "__untyped__";

/// 一次请求塞几条 summary。走常量不走配置，跟 `store::BATCH` 一个规矩 ——
/// 没有第二个用例，配一个永远只有一个取值的旋钮是空的。
///
/// 50 条 × 每条 ≤100 字，输出是 50 个 `{index, type_ids}` 小对象，对着
/// `[llm.classify].max_tokens`（6000）有 3 倍余量。**这里不做自适应二分**（那套是
/// ③ 的）：真撞上截断说明别的地方坏了，该显式失败而不是切了继续跑。
///
/// ⚠️ **调大它要同步核对 `[llm.classify].max_tokens`** —— 那个数是按这里的 50 算的。
pub(crate) const BATCH: usize = 50;

/// 允许一次「编了个不存在的 type_id / 漏了一行」的自我修正，不多给 —— 跟
/// `extract::model` 同一个数、同一个理由：逼急了模型会挑一个看起来合法的乱填。
const MAX_RETRIES: u32 = 1;

/// 一个事件最多挂几个类。**这是个真实的信任边界，不是旋钮**：不封顶的话
/// 「可以多选」会退化成「全都选上」，主类就没意义了，而主类是指标唯一的口径。
const MAX_TYPES: usize = 3;

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
pub struct Labels(Vec<String>);

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
struct Assignments {
    assignments: Vec<Assignment>,
}

#[derive(JsonSchema, Deserialize, Debug)]
struct Assignment {
    /// 段内 1-based 行号，跟 ③ 给模型的 `#N` 同一套 —— 模型全程不接触任何 ID。
    index: u32,
    /// 按贴切程度排序，**第一个是主类**。多数事件只有一个。
    type_ids: Vec<String>,
}

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

/// 数据库与人工草稿都必须通过同一份词表校验。
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

/// 一次运行构造一次。拿住词表、模型与缓存。
pub struct Classifier {
    version: String,
    types: Vec<TaxonomyType>,
    /// 合法答案集 = 词表全部 `type_id` ∪ `{__untyped__}`。校验用。
    known: BTreeSet<String>,
    /// 构造时渲染一次 —— 它只依赖词表，而词表一次运行内不变。
    system: String,
    llm: Llm,
    cache: Arc<Mutex<Cache>>,
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
        // **调用方有义务传 classify 那份 `Llm`** —— 传成抽取那份会带着 64000 出门，
        // 跑飞时唯一会喊停的就只剩 `timeout_secs`。
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
        answers: &[(&str, Labels)],
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
        primary: Option<String>,
        all: Option<String>,
        version: Option<String>,
    ) -> Result<Option<Labels>, BoxError> {
        match (primary, all, version) {
            (None, None, None) => Ok(None),
            (Some(primary), Some(all), Some(version)) if version == self.version => {
                let got = validate(
                    vec![Assignment {
                        index: 1,
                        type_ids: serde_json::from_str(&all)?,
                    }],
                    1,
                    &self.known,
                )
                .map_err(|_| "已保存标签不满足当前词表契约，请显式重打标")?
                .pop()
                .expect("单个合法标签");
                if got.primary() != primary {
                    return Err("已保存主类与标签全集不一致".into());
                }
                Ok(Some(got))
            }
            _ => Err("标签列不完整或词表版本不一致，请使用词表升版重打标".into()),
        }
    }

    /// 给一批 summary 打标，返回**与入参一一对应、等长**的标签。
    ///
    /// 失败即 `Err`，不生成兜底标签。日常跑批由独立打标队列调度每个批次；
    /// 人工试打和重打标可传入多批摘要，仍复用同一套去重、缓存和校验。
    pub async fn classify(&self, summaries: &[&str]) -> Result<Vec<Labels>, BoxError> {
        // v0：还没有词表。不查缓存、不发请求、不写文件。
        if self.types.is_empty() {
            return Ok(vec![Labels(vec![UNTYPED.to_string()]); summaries.len()]);
        }

        let keys: Vec<[u8; 32]> = summaries.iter().map(|s| digest(s)).collect();
        let mut answers: HashMap<[u8; 32], Labels> = HashMap::new();
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
    async fn ask(&self, batch: &[&str]) -> Result<Vec<Labels>, BoxError> {
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
         为**每一行**输出一个 type_ids 列表，以 index=N 输出。\n\n\
         规则：\n\
         1. type_id 必须逐字来自上面的列表，不要改写、不要翻译、不要发明新的。\n\
            **`##` 开头的是一级分类，只用来分组，本身不是可选答案** —— 先看摘要属于\n\
            哪个一级，再在它下面挑一个二级 type_id。\n\
         2. **type_ids 按贴切程度排序，第一个是主类。** 一条摘要如果确实同时讲了\n\
            两件不同的事（比如既要求换人、又在争议费用），就把两个都列出来；\n\
            只是同一件事的不同侧面，只给一个。多数事件只有一个类。\n\
         3. 最多 {MAX_TYPES} 个，不许重复。**拿不准就只给主类** —— 多列一个不相干的类\n\
            比少列一个的代价大得多。\n\
         4. 归不上去就用 {UNTYPED}，且它只能**单独**出现，不许和别的类混在一起 ——\n\
            硬塞进一个不合适的类，比承认归不上去更糟。\n\
         5. 每一行都必须有结果，不许漏行、不许合并、不许多给行。\n\
         6. 只看摘要本身描述的**事情是什么**，不要因为句式相近就归成一类。\n"
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
///   * **空列表 / 超过 `MAX_TYPES` / 行内重复 / `__untyped__` 混着别的类** ——
///     多标签这件事只在这四条守住时才有意义：空列表会让 [`Labels::primary`] 越界，
///     不封顶会让「可以多选」退化成「全都选上」（主类随之失去意义，而它是指标唯一的
///     口径），而 `__untyped__` 混着真实类是自相矛盾 —— 归不上去就是归不上去。
fn validate(
    assignments: Vec<Assignment>,
    n: usize,
    known: &BTreeSet<String>,
) -> Result<Vec<Labels>, Rejection> {
    let mut errs: Vec<(&'static str, String)> = Vec::new();
    let mut got: HashMap<usize, Labels> = HashMap::new();
    for a in assignments {
        let i = a.index as usize;
        if !(1..=n).contains(&i) {
            errs.push(("序号越界", format!("index {i} 超出本批范围 1-{n}")));
            continue;
        }
        if let Some(bad) = a.type_ids.iter().find(|t| !known.contains(*t)) {
            errs.push((
                "未知 type_id",
                format!(
                    "#{i} 的 type_id「{bad}」不在词表里；只能用列出的那些，归不上去请填 {UNTYPED}"
                ),
            ));
            continue;
        }
        if a.type_ids.is_empty() {
            errs.push(("标签为空", format!("#{i} 的 type_ids 是空的，至少要给一个")));
            continue;
        }
        if a.type_ids.len() > MAX_TYPES {
            errs.push((
                "标签过多",
                format!(
                    "#{i} 给了 {} 个类，最多 {MAX_TYPES} 个；拿不准就只给主类",
                    a.type_ids.len()
                ),
            ));
            continue;
        }
        if a.type_ids.iter().collect::<BTreeSet<_>>().len() != a.type_ids.len() {
            errs.push(("标签重复", format!("#{i} 的 type_ids 里有重复")));
            continue;
        }
        if a.type_ids.len() > 1 && a.type_ids.iter().any(|t| t == UNTYPED) {
            errs.push((
                "未分类标签混用",
                format!("#{i} 把 {UNTYPED} 和别的类混在一起了 —— 归不上去时它只能单独出现"),
            ));
            continue;
        }
        if got.insert(i, Labels(a.type_ids)).is_some() {
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

// ─────────────────────────────────────────────────────────────────────────────
// 结果缓存 —— 内容寻址 SQLite，保留首次答案。**承重件**（见模块头）。
// ─────────────────────────────────────────────────────────────────────────────

/// 缓存键是 `sha256(summary)`，**不是 event_id**。
///
/// 用 event_id 会跟分片删重写直接冲突：重跑某个群某天，event 全删重建、id 全变，
/// 落盘的那批答案立刻变成没人认领的孤儿，而新 event 又没有标签。
///
/// 存 hash 不存原文的第二个理由是 PII：`summary` 里有客户姓名和地址
/// （脱敏明确不掩它们），而缓存**只增不减**——原文一旦进去就是永久的。
///
fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn hex(k: &[u8; 32]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// `t` 是**全集**（第一个是主类），不是主类一个字符串 —— 只缓存主类的话，
/// 副类每次都要重问，缓存就不再保证「同 summary 同答案」的那一半。
#[derive(Serialize, Deserialize)]
struct Entry {
    h: String,
    t: Vec<String>,
}

struct Cache {
    db: rusqlite::Connection,
    file: File,
    known: BTreeSet<String>,
    write_failed: bool,
}

impl Drop for Cache {
    fn drop(&mut self) {
        // 显式解锁：并行测试 fork 的短暂描述符继承也不能延长本对象的锁生命周期。
        if let Err(error) = self.file.unlock() {
            tracing::warn!("释放分类缓存锁失败：{error}");
        }
    }
}

impl Cache {
    fn open(path: &Path, known: &BTreeSet<String>) -> Result<Self, BoxError> {
        let started = std::time::Instant::now();
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        // 锁覆盖整个缓存生命周期；另一个进程不能拿旧内存快照继续追加答案。
        file.try_lock().map_err(|e| {
            format!(
                "无法独占分类缓存 {}，请确认没有其他跑批或重打标占用：{e}",
                path.display()
            )
        })?;
        let database = path.with_extension("sqlite");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        drop(options.open(&database)?);
        let mut db = rusqlite::Connection::open(&database)?;
        // 页缓存按 2 MiB 控制，禁用 mmap；历史答案留在磁盘，事务提交后才对调用方可见。
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-2048; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 {
            return Err("分类缓存格式版本高于当前程序，拒绝覆盖".into());
        }
        if version == 0 {
            // 旧 NDJSON 只导入一次，格式标记与答案同事务；中断后回滚，重试不会改变首个答案。
            let tx = db.transaction()?;
            tx.execute_batch(
                "CREATE TABLE answers (hash BLOB PRIMARY KEY, labels TEXT NOT NULL) WITHOUT ROWID;",
            )?;
            let mut reader = BufReader::new(&file);
            let mut line = Vec::new();
            let (mut bad, mut imported) = (0usize, 0usize);
            loop {
                line.clear();
                let n = (&mut reader)
                    .take(1024 * 1024)
                    .read_until(b'\n', &mut line)?;
                if n == 0 {
                    break;
                }
                if n == 1024 * 1024 {
                    return Err("旧分类缓存单行超过 1 MiB，拒绝无界加载".into());
                }
                // 不修改旧文件，保留升级前备份；未完成尾行不是已提交答案。
                if !line.ends_with(b"\n") {
                    bad += 1;
                    break;
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let entry = serde_json::from_slice::<Entry>(&line).ok().and_then(|e| {
                    let key = unhex(&e.h)?;
                    let labels = validate(
                        vec![Assignment {
                            index: 1,
                            type_ids: e.t,
                        }],
                        1,
                        known,
                    )
                    .ok()?
                    .pop()?;
                    Some((key, labels))
                });
                if let Some((key, labels)) = entry {
                    imported += tx.execute(
                        "INSERT OR IGNORE INTO answers (hash, labels) VALUES (?1, ?2)",
                        rusqlite::params![key.as_slice(), serde_json::to_string(labels.all())?],
                    )?;
                } else {
                    bad += 1;
                }
            }
            tx.execute_batch("PRAGMA user_version=1;")?;
            tx.commit()?;
            tracing::info!(
                imported,
                bad,
                "旧分类缓存导入完成；原文件保留，坏行与未完成尾行未导入"
            );
        }
        tracing::info!(
            ms = started.elapsed().as_millis(),
            path = %database.display(),
            "分类缓存已打开（按键读盘）"
        );
        Ok(Self {
            db,
            file,
            known: known.clone(),
            write_failed: false,
        })
    }

    fn get(&self, key: &[u8; 32]) -> Result<Option<Labels>, BoxError> {
        let raw: Option<String> = self
            .db
            .query_row(
                "SELECT labels FROM answers WHERE hash=?1",
                [key.as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|raw| {
            validate(
                vec![Assignment {
                    index: 1,
                    type_ids: serde_json::from_str(&raw)?,
                }],
                1,
                &self.known,
            )
            .map_err(|_| BoxError::from("持久分类答案损坏或与词表不符，拒绝重新请求以免答案漂移"))?
            .pop()
            .ok_or_else(|| "持久分类答案为空".into())
        })
        .transpose()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.db
            .query_row("SELECT COUNT(*) FROM answers", [], |r| r.get::<_, i64>(0))
            .unwrap() as usize
    }

    /// 同一摘要采用第一个已提交的答案；每个模型批次只同步一次磁盘。
    fn commit(&mut self, entries: Vec<([u8; 32], Labels)>) -> Result<Vec<Labels>, BoxError> {
        if self.write_failed {
            return Err("分类缓存此前写入失败，停止追加；请排查磁盘并重启本轮".into());
        }
        // 同批重复与跨批竞争都由唯一键决定；事务失败不返回未持久化答案。
        self.write_failed = true;
        let tx = self.db.transaction()?;
        let mut accepted = Vec::with_capacity(entries.len());
        for (key, labels) in entries {
            tx.execute(
                "INSERT OR IGNORE INTO answers (hash, labels) VALUES (?1, ?2)",
                rusqlite::params![key.as_slice(), serde_json::to_string(labels.all())?],
            )?;
            let raw: String = tx.query_row(
                "SELECT labels FROM answers WHERE hash=?1",
                [key.as_slice()],
                |r| r.get(0),
            )?;
            let got = validate(
                vec![Assignment {
                    index: 1,
                    type_ids: serde_json::from_str(&raw)?,
                }],
                1,
                &self.known,
            )
            .map_err(|_| "持久分类答案损坏，拒绝发布")?
            .pop()
            .expect("一个已校验答案");
            accepted.push(got);
        }
        tx.commit()?;
        self.write_failed = false;
        Ok(accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_classify_llm;
    use serde_json::{Value, json};

    fn http_model(
        replies: Vec<Value>,
        wait_for_all: bool,
    ) -> (String, std::thread::JoinHandle<Vec<Value>>) {
        crate::testutil::http_model(
            replies
                .into_iter()
                .map(|r| (200, crate::testutil::completion(&r.to_string(), "stop")))
                .collect(),
            wait_for_all,
        )
    }

    fn reply(types: &[&str]) -> Value {
        json!({"assignments":[{"index":1,"type_ids":types}]})
    }

    #[tokio::test]
    async fn rejected_model_evidence_is_only_sent_back_to_the_model() {
        let marker = "PRIVATE-ADDRESS-TEST";
        let (base, server) = http_model(vec![reply(&[marker]), reply(&[marker])], false);
        let root = crate::testutil::fresh_root("classify", "private-rejection");
        let classifier =
            Classifier::new("v1", vec![ty("a")], test_classify_llm(&base, "test"), &root).unwrap();
        let error = classifier.classify(&["商家要求改期"]).await.unwrap_err();
        let requests = server.join().unwrap();
        assert!(
            requests[1]["messages"].as_array().unwrap().iter().any(|m| {
                m["role"] == "user" && m["content"].as_str().is_some_and(|s| s.contains(marker))
            }),
            "模型应收到逐字纠错信息"
        );
        assert!(
            !error.to_string().contains(marker),
            "运维错误不应包含模型逐字证据：{error}"
        );
        assert!(!format!("{error:?}").contains(marker));
    }

    #[test]
    fn constructor_rejects_missing_or_invalid_published_taxonomy() {
        let dir = crate::testutil::fresh_root("classify", "validation");
        let llm = test_classify_llm("http://127.0.0.1:1/v1", "test");
        for (version, types) in [
            ("v1", vec![]),
            ("v0", vec![ty("a")]),
            ("../v1", vec![ty("a")]),
            ("v1", vec![ty("a"), ty("a")]),
            ("v1", vec![ty(UNTYPED)]),
        ] {
            assert!(Classifier::new(version, types, llm.clone(), &dir).is_err());
        }
        let mut invalid = ty("a");
        invalid.description = " ".into();
        assert!(Classifier::new("v1", vec![invalid], llm.clone(), &dir).is_err());
        let mut invalid = ty("a");
        invalid.name = "名称\n伪造规则".into();
        assert!(Classifier::new("v1", vec![invalid], llm, &dir).is_err());
        assert!(!dir.exists(), "词表失败应发生在创建缓存之前");
    }

    #[test]
    fn the_published_taxonomy_passes_the_database_loading_checks() {
        let draft: crate::taxonomy::Draft =
            toml::from_str(include_str!("../taxonomy_v1.toml")).unwrap();
        let dir = crate::testutil::fresh_root("classify", "published-taxonomy");
        let classifier = Classifier::new(
            &draft.version,
            draft.types.clone(),
            test_classify_llm("http://127.0.0.1:1/v1", "test"),
            &dir,
        )
        .unwrap();
        assert_eq!(classifier.type_count(), draft.types.len());
    }

    #[tokio::test]
    async fn concurrent_model_answers_return_the_same_committed_labels() {
        let (base, server) = http_model(vec![reply(&["a"]), reply(&["b"])], true);
        let dir = crate::testutil::fresh_root("classify", "concurrent-http");
        let llm = test_classify_llm(&base, "test");
        let classifier = Classifier::new("v1", vec![ty("a"), ty("b")], llm.clone(), &dir).unwrap();
        let (left, right) = tokio::join!(
            classifier.classify(&["同一摘要"]),
            classifier.classify(&["同一摘要"])
        );
        let left = left.unwrap();
        assert_eq!(left, right.unwrap());
        assert_eq!(server.join().unwrap().len(), 2);
        drop(classifier);
        // 端点已关闭，重开缓存仍必须返回同一答案。
        let reopened = Classifier::new("v1", vec![ty("a"), ty("b")], llm, &dir).unwrap();
        assert_eq!(reopened.classify(&["同一摘要"]).await.unwrap(), left);
    }

    #[tokio::test]
    async fn saved_answers_cannot_populate_a_different_policy_cache() {
        let dir = crate::testutil::fresh_root("classify", "restore-policy");
        let old = Classifier::new(
            "v1",
            vec![ty("a")],
            test_classify_llm("http://localhost:1/v1", "old"),
            &dir,
        )
        .unwrap();
        old.cache
            .lock()
            .await
            .commit(vec![(digest("已保存摘要"), lab(&["a"]))])
            .unwrap();
        let answers = [("已保存摘要", lab(&["a"]))];
        old.check_saved_answers(&answers).await.unwrap();
        let changed = Classifier::new(
            "v1",
            vec![ty("a")],
            test_classify_llm("http://localhost:1/v1", "new"),
            &dir,
        )
        .unwrap();
        assert!(changed.check_saved_answers(&answers).await.is_err());
        assert_eq!(
            changed.cache.lock().await.len(),
            0,
            "旧模型答案不能污染新策略缓存"
        );
    }

    #[tokio::test]
    async fn model_batches_deduplicate_and_keep_input_order() {
        let first = json!({"assignments":(1..=50).rev().map(|index| json!({"index":index,"type_ids":["a"]})).collect::<Vec<_>>()});
        let (base, server) = http_model(vec![first, reply(&["b"])], false);
        let dir = crate::testutil::fresh_root("classify", "batch-http");
        let classifier = Classifier::new(
            "v1",
            vec![ty("a"), ty("b")],
            test_classify_llm(&base, "test"),
            &dir,
        )
        .unwrap();
        classifier
            .cache
            .lock()
            .await
            .commit(vec![(digest("已缓存摘要"), lab(&["b"]))])
            .unwrap();
        let mut summaries: Vec<_> = (0..51).map(|i| format!("摘要{i}")).collect();
        summaries.extend([
            summaries[0].clone(),
            "已缓存摘要".into(),
            "已缓存摘要".into(),
        ]);
        let got = classifier
            .classify(&summaries.iter().map(String::as_str).collect::<Vec<_>>())
            .await
            .unwrap();
        assert_eq!(got.len(), 54);
        assert!(got[..50].iter().all(|label| label.primary() == "a"));
        assert_eq!(got[50].primary(), "b");
        assert_eq!(got[51].primary(), "a");
        assert!(got[52..].iter().all(|label| label.primary() == "b"));
        let requests = server.join().unwrap();
        // **打标请求必须带着 `[llm.classify]` 那份小预算出门** —— 这是全仓唯一一处
        // 运行时证明它的地方。此前钉的是 `CLASSIFY_MAX_TOKENS` 那个常量（代码里的
        // 无条件钳位），现在真相在 config.toml，所以钉**关系**不钉字面量：
        // 6000 调成 5000 不该让这条误报，而抄成抽取那个 64000 必须让它红。
        let cfg: crate::config::Config = toml::from_str(include_str!("../config.toml")).unwrap();
        assert_eq!(requests[0]["max_tokens"], cfg.llm.classify.max_tokens);
        assert!(
            cfg.llm.classify.max_tokens < cfg.llm.extract.max_tokens,
            "打标预算必须远小于抽取预算，否则跑飞时只剩 timeout_secs 喊停"
        );
        assert!(
            requests[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("a | a：描述")
        );
        assert_eq!(
            requests[0]["messages"][1]["content"]
                .as_str()
                .unwrap()
                .lines()
                .count(),
            50
        );
        assert_eq!(requests[1]["messages"][1]["content"], "#1 摘要50\n");
    }

    #[tokio::test]
    async fn invalid_model_output_is_retried_and_never_cached_as_untyped() {
        let (base, server) = http_model(vec![reply(&["unknown"]), reply(&["a"])], false);
        let dir = crate::testutil::fresh_root("classify", "retry-http");
        let classifier =
            Classifier::new("v1", vec![ty("a")], test_classify_llm(&base, "test"), &dir).unwrap();
        assert_eq!(classifier.classify(&["摘要"]).await.unwrap(), [lab(&["a"])]);
        let requests = server.join().unwrap();
        assert!(
            requests[1]["messages"][3]["content"]
                .as_str()
                .unwrap()
                .contains("不在词表里")
        );

        let (base, server) = http_model(vec![reply(&["unknown"]), reply(&["unknown"])], false);
        let classifier = Classifier::new(
            "v1",
            vec![ty("a")],
            test_classify_llm(&base, "test"),
            &dir.join("failed"),
        )
        .unwrap();
        assert!(classifier.classify(&["摘要"]).await.is_err());
        assert_eq!(classifier.cache.lock().await.len(), 0);
        server.join().unwrap();
    }

    #[test]
    fn cache_lock_rejects_a_second_process_and_releases_on_drop() {
        const ENV: &str = "C2E_CLASSIFY_LOCK_PROBE";
        if let Some(path) = std::env::var_os(ENV) {
            assert!(Cache::open(Path::new(&path), &known()).is_err());
            return;
        }
        let dir = crate::testutil::fresh_root("classify", "lock");
        let path = dir.join("v1.ndjson");
        let cache = Cache::open(&path, &known()).unwrap();
        assert!(Cache::open(&path, &known()).is_err());
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "classify::tests::cache_lock_rejects_a_second_process_and_releases_on_drop",
            ])
            .env(ENV, &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        drop(cache);
        assert!(Cache::open(&path, &known()).is_ok());
    }

    #[test]
    fn cache_write_failure_does_not_publish_answers_or_continue_appending() {
        let dir = crate::testutil::fresh_root("classify", "write-failure");
        let path = dir.join("v1.ndjson");
        let mut cache = Cache::open(&path, &known()).unwrap();
        cache.db.execute_batch("PRAGMA query_only=ON;").unwrap(); // 只读连接确定性制造事务写入错误。
        assert!(cache.commit(vec![(digest("a"), lab(&["a"]))]).is_err());
        assert_eq!(cache.len(), 0);
        assert!(
            cache
                .commit(vec![(digest("b"), lab(&["b"]))])
                .unwrap_err()
                .to_string()
                .contains("此前写入失败")
        );
    }

    #[test]
    fn legacy_answers_migrate_once_and_keep_the_first_answer() {
        let dir = crate::testutil::fresh_root("classify", "migration");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.ndjson");
        let key = digest("旧答案");
        let original = format!(
            "{}\n坏行\n{}\n半截",
            serde_json::to_string(&Entry {
                h: hex(&key),
                t: vec!["a".into()]
            })
            .unwrap(),
            serde_json::to_string(&Entry {
                h: hex(&key),
                t: vec!["b".into()]
            })
            .unwrap()
        );
        std::fs::write(&path, &original).unwrap();
        let mut cache = Cache::open(&path, &known()).unwrap();
        assert_eq!(cache.get(&key).unwrap(), Some(lab(&["a"])));
        assert_eq!(
            cache.commit(vec![(key, lab(&["b"]))]).unwrap(),
            [lab(&["a"])]
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "升级保留原缓存备份"
        );
        drop(cache);
        // 导入标记已提交，重启不再扫描旧文件，也不会重新解释已持久化答案。
        std::fs::write(&path, "已经归档").unwrap();
        assert_eq!(
            Cache::open(&path, &known()).unwrap().get(&key).unwrap(),
            Some(lab(&["a"]))
        );
    }

    #[test]
    fn disk_cache_uses_indexed_lookup_with_a_bounded_page_cache() {
        let count: usize = std::env::var("CHAT2EVENTS_CACHE_TEST_ENTRIES")
            .ok()
            .map(|v| v.parse().unwrap())
            .unwrap_or(2000);
        let dir = crate::testutil::fresh_root("classify", "capacity");
        let path = dir.join("v1.ndjson");
        let mut cache = Cache::open(&path, &known()).unwrap();
        for first in (0..count).step_by(BATCH) {
            cache
                .commit(
                    (first..(first + BATCH).min(count))
                        .map(|i| (digest(&i.to_string()), lab(&["a"])))
                        .collect(),
                )
                .unwrap();
        }
        drop(cache);
        let start = std::time::Instant::now();
        let cache = Cache::open(&path, &known()).unwrap();
        assert_eq!(cache.len(), count);
        for i in [0, count / 2, count.saturating_sub(1)] {
            assert_eq!(
                cache.get(&digest(&i.to_string())).unwrap(),
                Some(lab(&["a"]))
            );
        }
        let plan: String = cache
            .db
            .query_row(
                "EXPLAIN QUERY PLAN SELECT labels FROM answers WHERE hash=?1",
                [digest("0").as_slice()],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("SEARCH") && plan.contains("PRIMARY KEY"),
            "{plan}"
        );
        assert_eq!(
            cache
                .db
                .query_row("PRAGMA cache_size", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            -2048
        );
        eprintln!(
            "cache entries={count}, reopen_and_lookup_ms={}",
            start.elapsed().as_millis()
        );
    }

    #[test]
    fn different_models_versions_and_taxonomies_use_separate_cache_files() {
        let dir = crate::testutil::fresh_root("classify", "policy");
        for (version, model, types) in [
            ("v1", "model-a", vec![ty("a")]),
            ("v1", "model-b", vec![ty("a")]),
            ("v2", "model-a", vec![ty("a")]),
            ("v1", "model-a", vec![ty("b")]),
        ] {
            drop(
                Classifier::new(
                    version,
                    types,
                    test_classify_llm("http://127.0.0.1:1/v1", model),
                    &dir,
                )
                .unwrap(),
            );
        }
        assert_eq!(
            std::fs::read_dir(&dir)
                .unwrap()
                .filter(|entry| entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "sqlite"))
                .count(),
            4
        );
    }

    fn ty(id: &str) -> TaxonomyType {
        TaxonomyType {
            type_id: id.into(),
            parent_name: "一级".into(),
            name: id.into(),
            description: "描述".into(),
        }
    }

    fn ty_under(parent: &str, id: &str) -> TaxonomyType {
        TaxonomyType {
            parent_name: parent.into(),
            ..ty(id)
        }
    }

    fn known() -> BTreeSet<String> {
        [ty("a").type_id, ty("b").type_id, UNTYPED.to_string()]
            .into_iter()
            .collect()
    }

    /// `(行号, 该行的 type_ids)`
    fn asg(pairs: &[(u32, &[&str])]) -> Vec<Assignment> {
        pairs
            .iter()
            .map(|(i, ts)| Assignment {
                index: *i,
                type_ids: ts.iter().map(|t| (*t).to_string()).collect(),
            })
            .collect()
    }

    /// 校验结果拍平成 `Vec<Vec<&str>>`，好和字面量直接比。
    fn flat(r: Result<Vec<Labels>, Rejection>) -> Vec<Vec<String>> {
        r.unwrap().into_iter().map(|l| l.0).collect()
    }

    fn lab(ts: &[&str]) -> Labels {
        Labels(ts.iter().map(|t| (*t).to_string()).collect())
    }

    /// 缓存文件名**不许依赖词表传进来的顺序**。
    ///
    /// 从库里读的走 `ORDER BY type_id`，从草稿来的走 TOML 里的原序 —— 两边不一致时
    /// 试打和正式 recompute 会各写各的缓存文件，「试打不是额外开销」那句话就是假的。
    /// 一个请求都不发，纯 CPU。
    #[test]
    fn the_cache_path_does_not_depend_on_the_order_the_taxonomy_came_in() {
        // 一个请求都不发，Llm 只是构造指纹时的一个输入 —— 拿现成的 fixture 就够。
        let llm = test_classify_llm("http://localhost:1/v1", "m");
        let dir = crate::testutil::fresh_root("classify", "order");
        let path = |types: Vec<TaxonomyType>| {
            drop(Classifier::new("v1", types, llm.clone(), &dir).unwrap());
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<BTreeSet<_>>()
        };
        // **跨一级乱序**：只按 type_id 排的话 `b1` 会插到 `a1`/`a2` 中间，
        // 分组标题就会多出来一个，指纹随输入顺序变。
        let x = || {
            vec![
                ty_under("甲", "a1"),
                ty_under("乙", "b1"),
                ty_under("甲", "a2"),
            ]
        };
        let mut shuffled = x();
        shuffled.reverse();
        assert_eq!(path(x()), path(shuffled));
    }

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
            flat(validate(asg(&[(1, &["a"]), (2, &[UNTYPED])]), 2, &k)),
            [vec!["a"], vec![UNTYPED]]
        );
        assert!(
            validate(asg(&[(1, &["a"]), (2, &["zzz"])]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("不在词表里")
        );
        assert!(
            validate(asg(&[(1, &["a"])]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("漏了 1 行：#2")
        );
        assert!(
            validate(asg(&[(1, &["a"]), (3, &["b"])]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("超出本批范围 1-2")
        );
        assert!(
            validate(asg(&[(1, &["a"]), (1, &["b"]), (2, &["a"])]), 2, &k)
                .unwrap_err()
                .verbatim()
                .contains("不止一行结果")
        );
    }

    /// 多标签的四条守卫。**这四条守不住，多标签就是数据损坏而不是特性**：
    /// 空列表让 `primary()` 越界；不封顶让「可以多选」退化成「全都选上」，
    /// 主类随之失去意义（而它是指标唯一的口径）；`__untyped__` 混着真实类自相矛盾。
    #[test]
    fn multi_label_guards_reject_empty_overlong_duplicate_and_mixed_untyped() {
        let k = known();
        assert_eq!(
            flat(validate(asg(&[(1, &["a", "b"])]), 1, &k)),
            [vec!["a", "b"]],
            "主类在前，副类在后"
        );
        let err = |p: &[(u32, &[&str])]| validate(asg(p), 1, &k).unwrap_err().verbatim().to_owned();
        assert!(err(&[(1, &[])]).contains("空的"));
        assert!(err(&[(1, &["a", "b", "a"])]).contains("重复"));
        assert!(err(&[(1, &[UNTYPED, "a"])]).contains("单独出现"));
        // MAX_TYPES = 3，给 4 个
        let k4: BTreeSet<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert!(
            validate(asg(&[(1, &["a", "b", "c", "d"])]), 1, &k4)
                .unwrap_err()
                .verbatim()
                .contains(&format!("最多 {MAX_TYPES} 个"))
        );
    }

    /// 结果按 index 归位，不按模型给的顺序 —— 乱序返回是允许的。
    #[test]
    fn results_are_ordered_by_index_not_by_reply_order() {
        assert_eq!(
            flat(validate(
                asg(&[(3, &["b"]), (1, &["a"]), (2, &[UNTYPED])]),
                3,
                &known()
            )),
            [vec!["a"], vec![UNTYPED], vec!["b"]]
        );
    }

    /// 缓存跨进程可复现：写一轮、重开一次、答案一字不变（确定性靠的就是这个）。
    /// 顺带钉住坏行跳过而不是整轮死。
    #[test]
    fn the_cache_round_trips_and_survives_a_corrupt_line() {
        let dir = crate::testutil::fresh_root("classify", "cache");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("v1.ndjson");

        let k1 = digest("商家要求取消订单");
        let mut c = Cache::open(&p, &known()).unwrap();
        c.commit(vec![(k1, lab(&["a", "b"]))]).unwrap();
        drop(c);

        std::fs::write(
            &p,
            format!("{}{{半截\n", std::fs::read_to_string(&p).unwrap()),
        )
        .unwrap();

        let c = Cache::open(&p, &known()).unwrap();
        // 缓存存的是**全集**不是主类 —— 只存主类的话副类每次都要重问，
        // 「同 summary 同答案」就只保住了一半。
        assert_eq!(c.get(&k1).unwrap(), Some(lab(&["a", "b"])));
        assert_eq!(c.len(), 1, "坏行不该变成一条答案");
    }

    #[test]
    fn hex_round_trips() {
        let k = digest("x");
        assert_eq!(unhex(&hex(&k)), Some(k));
        assert_eq!(unhex("nothex"), None);
    }

    #[test]
    fn a_second_answer_cannot_replace_the_first_cached_answer() {
        let dir = crate::testutil::fresh_root("classify", "first-answer");
        let mut c = Cache::open(&dir.join("v1.ndjson"), &known()).unwrap();
        let key = digest("同一摘要");
        c.commit(vec![(key, lab(&["a"]))]).unwrap();
        c.commit(vec![(key, lab(&["b"]))]).unwrap();
        assert_eq!(c.get(&key).unwrap(), Some(lab(&["a"])));
    }

    #[test]
    fn an_unterminated_cache_tail_does_not_swallow_the_next_answer() {
        let dir = crate::testutil::fresh_root("classify", "tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.ndjson");
        std::fs::write(&path, b"{\"h\":\"unfinished").unwrap();
        let key = digest("新摘要");
        let mut c = Cache::open(&path, &known()).unwrap();
        c.commit(vec![(key, lab(&["a"]))]).unwrap();
        drop(c);
        let reopened = Cache::open(&path, &known()).unwrap();
        assert_eq!(reopened.get(&key).unwrap(), Some(lab(&["a"])));
    }

    /// 显式 v0 且词表为空：全 `__untyped__`，一个请求都不发。
    #[tokio::test]
    async fn an_empty_taxonomy_is_v0_and_asks_nothing() {
        let dir = crate::testutil::fresh_root("classify", "v0");
        let cfg: crate::config::Config = toml::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap(),
        )
        .unwrap();
        let c = Classifier::new(
            "v0",
            vec![],
            Llm::new(&cfg.llm, &cfg.llm.classify, "sk-none".into()).unwrap(),
            &dir,
        )
        .unwrap();
        assert_eq!(c.type_count(), 0);
        assert_eq!(
            c.classify(&["甲", "乙"]).await.unwrap(),
            [lab(&[UNTYPED]), lab(&[UNTYPED])]
        );
    }

    /// system prompt 必须把 `__untyped__` 也列成合法答案 —— 漏了它，模型就只能
    /// 在真实类型里硬选一个，「归不上去」这个信号会永远为 0。
    #[test]
    fn the_system_prompt_offers_untyped_as_a_legal_answer() {
        let s = render_system(&[ty("cancel")]);
        assert!(s.contains("cancel | cancel：描述"));
        assert!(s.contains(UNTYPED));
        // 多标签规则也必须在 prompt 里，否则模型永远只给一个
        assert!(s.contains("第一个是主类"), "{s}");
        assert!(s.contains("只能**单独**出现"), "{s}");
    }
}
