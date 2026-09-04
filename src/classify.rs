//! ⑤ 分类 classify —— `summary` ＋ 词表 -> [`Labels`]（主类 + 全集）。
//!
//! 一个事件可以同时属于两件事，但**指标只按主类算** —— 理由见 [`Labels`]。
//!
//! **确定性是硬约束**：同样的 `summary` ＋ 同样的词表 → 永远同样的 type。所以每次 event
//! 落库都算，包括分片删重写那一次 —— **标签不刻在 `Event` 上，是每次算出来的**，
//! 分片重写因此不会丢标签。
//!
//! ⚠️ **确定性由缓存保证，不由算法保证。** v1 的算法是「让模型从封闭词表里选」，
//! 而 `temperature = 0` 并不保证同输入同输出。非冻结区每天重写 `[T-2, T-1]`，
//! 同一批 event 会被反复打标 —— 没有跨运行的持久缓存，报表就会**抖动而非修正**，
//! 正是承重不变量 1 要防的那件事。**[`Cache`] 是承重件，不是优化。**
//!
//! **打标发生在事务外**：由 `daily` 算好后传给 ⑥ 和 ⑦。三件事一起解决 ——
//! 长事务（缓存未命中 = 持锁发 N 次模型请求）、依赖成环（`store` 不 import 这里，
//! 这里也不 import `store` —— 词表由调用方读好传进来）、算两遍（⑥ 指标和 ⑦ 落库
//! 共用同一份结果）。
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
//! 等于「`b_merchant_group_taxonomy` 里 `v0` 没有行」，于是它就是 `types.is_empty()`
//! 那一行 if。仍然是一个适配器 = 假想接缝，按本仓库自己的判据不写 trait。

use crate::{
    BoxError,
    llm::{Llm, LlmError, Turn},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    sync::Mutex,
};

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
/// [`CLASSIFY_MAX_TOKENS`] 有 3 倍余量。**这里不做自适应二分**（那套是
/// ③ 的）：真撞上截断说明别的地方坏了，该显式失败而不是切了继续跑。
const BATCH: usize = 50;

/// 打标输出的上限，[`Classifier::new`] 里包一次全程生效。一批 [`BATCH`] 行 ×
/// 每行一个小对象，正常不过 2000 token —— 6000 是 3 倍余量。
///
/// **不用配置里的 64000**（那是给 ③ 的）：上限一大，模型跑飞（strict JSON schema
/// 下随机陷入重复生成，实测中招率约三分之一）就没有任何东西拦得住，只能安静生成
/// 到撞满 `timeout_secs = 300`，再报一个无从下手的 `Timeout` —— 试打 870 条
/// summary 是 18 批，按这个中招率**几乎每趟都要挂死几回**，正是「归纳跑一半
/// 卡住五分钟然后整趟报废」的主因。贴着实际需求给，跑飞秒撞 `Truncated`，
/// `extract_retry` 重发即可。
const CLASSIFY_MAX_TOKENS: u32 = 6000;

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
/// **没有 `centroid`。** 那一列在 DDL 里可空，是给「将来真有向量路径」留的；
/// 今天两条归纳路径产出的都只有名字和描述（`schema.sql:136`：`description` 必填
/// 就是为了让 classify 不依赖向量也能工作）。哪天真用上了再加。
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

    /// 从落盘的字符串列表还原（打标缓存、类心模型两处都走它）。
    /// **空的一律拒绝** —— 非空是构造保证，一条空的落进来会让
    /// [`Labels::primary`] 越界 panic。这条规矩只该写一遍。
    pub(crate) fn from_saved(v: Vec<String>) -> Option<Self> {
        (!v.is_empty()).then_some(Self(v))
    }

    /// 测试专用构造。生产路径只有 [`validate`] 和 [`Self::from_saved`] 造 `Labels`，
    /// 两处都亲手保证了非空 —— 这个口子只对 `cfg(test)` 开，不给生产代码。
    #[cfg(test)]
    pub(crate) fn for_test(primary: &str) -> Self {
        Self(vec![primary.to_string()])
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

/// 一次运行构造一次。拿住三样状态：词表 · 预渲染的 system prompt · 结果缓存。
pub struct Classifier {
    version: String,
    types: Vec<TaxonomyType>,
    /// 合法答案集 = 词表全部 `type_id` ∪ `{__untyped__}`。校验用。
    known: BTreeSet<String>,
    /// 构造时渲染一次 —— 它只依赖词表，而词表一次运行内不变。
    system: String,
    llm: Llm,
    cache: Mutex<Cache>,
    /// 打标缓存和类心模型放在一起、共用同一个「版本 + 指纹」文件名。
    model_path: std::path::PathBuf,
    /// **本地类心，第二道**（缓存之后、模型之前）。`None` = 还没训练过，
    /// 退回全部问模型 —— 那正是 v0 和第一次 `recompute` 之前的正常状态。
    matcher: Option<crate::nearest::Nearest>,
    /// 最近与次近之差要达到它才敢用本地答案。`matcher` 为 `None` 时无意义。
    margin: f32,
}

impl Classifier {
    /// 缓存文件是 `<cache_dir>/<version>-<指纹>.ndjson`。
    ///
    /// **指纹是承重的，版本号不够。** 缓存要保证的是「同样的输入给同样的答案」，
    /// 而输入是 *(summary, 词表, prompt)* 三样 —— 版本号只是个**承诺**说词表没变，
    /// 归纳期人反复改草稿却一直叫 `v1` 时那个承诺就是假的，旧答案会被当成新答案端出来。
    /// 指纹取的是**渲染好的 system prompt** 的 sha256 前 8 字节：词表改一个字、
    /// prompt 改一个字，都换一个文件，旧答案自然失效。改 prompt 也算数是对的 ——
    /// 换了问法就不再保证同样的答案。
    ///
    /// 缓存目录建不出来 / 文件打不开 → **在这里就报错**，不等到第一次打标。
    /// 跑批是无人值守的，配错路径要在进程起来的头几秒暴露。
    pub fn new(
        version: &str,
        mut types: Vec<TaxonomyType>,
        llm: Llm,
        cache_dir: &Path,
    ) -> Result<Self, BoxError> {
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
        let fp: String = hex(&digest(&system)).chars().take(8).collect();
        let cache = Cache::open(&cache_dir.join(format!("{version}-{fp}.ndjson")))?;
        Ok(Self {
            version: version.to_string(),
            types,
            known,
            system,
            // 上限贴着打标的真实输出给，不吃配置里的 64000 —— 见 CLASSIFY_MAX_TOKENS
            llm: llm.with_max_tokens(CLASSIFY_MAX_TOKENS),
            cache: Mutex::new(cache),
            // 模型文件跟缓存共用「版本 + prompt 指纹」：词表或 prompt 改一个字，
            // 旧类心自动失效 —— 跟缓存同一条理由，版本号只是个可能作假的承诺。
            model_path: cache_dir.join(format!("{version}-{fp}-nearest.json")),
            matcher: None,
            margin: 0.0,
        })
    }

    /// 类心模型该放哪。`recompute` 训练完往这写，`daily` 起来时从这读。
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// 挂上本地类心。**两条路（`daily` / `recompute`）必须挂同一个模型、同一个
    /// `margin`**，否则冻结区和 `[T-2, T-1]` 会用两套口径，边界上同类事件标签不同 ——
    /// 那正是承重不变量 1 要防的抖动。所以 `margin` 走配置不走命令行。
    pub fn attach(&mut self, matcher: crate::nearest::Nearest, margin: f32) {
        self.matcher = Some(matcher);
        self.margin = margin;
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

    /// 给一批 summary 打标，返回**与入参一一对应、等长**的标签。
    ///
    /// 失败即 `Err` —— 调用方（`daily::run_room`）把它和抽取失败同等对待：该群本轮
    /// 一行不写。**绝不降级成全 `__untyped__`**：那是拿「归不上去」冒充「没算出来」，
    /// 承重不变量 4 明令禁止。
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
            // 临界区里不 await：锁只覆盖内存查表，模型请求在锁外。
            let c = self.cache.lock().expect("缓存锁不跨 await，不会中毒");
            // `pending` 单独用一个集合，不去 `todo` 里线性找 —— `taxonomy::review`
            // 一次会把**全量**去重后的 summary 交进来（可能上万条），线性查重就是
            // O(n²) 次 32 字节比较，那一步会从秒级变成分钟级。
            let mut pending: BTreeSet<[u8; 32]> = BTreeSet::new();
            for (k, s) in keys.iter().zip(summaries) {
                if answers.contains_key(k) {
                    continue;
                }
                match c.map.get(k) {
                    Some(t) => {
                        answers.insert(*k, t.clone());
                    }
                    None => {
                        if pending.insert(*k) {
                            todo.push((*k, s));
                        }
                    }
                }
            }
        }

        // **第二道：本地类心。** 在锁外跑 —— predict 是纯 CPU，握着缓存锁做它
        // 会把并发的群任务串起来。差距不够的留在 `todo` 里去问模型，
        // **绝不硬塞最近的那个类**（承重不变量 4）。
        // 本地答案**不写缓存**：缓存是模型答案的存档，混进类心答案之后
        // 调高 `margin` 也再改不回来了。
        if let Some(m) = &self.matcher {
            let margin = self.margin;
            todo.retain(|(k, s)| match m.predict(s) {
                Some(h) if h.margin >= margin => {
                    answers.insert(*k, h.labels.clone());
                    false
                }
                _ => true,
            });
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
            let mut c = self.cache.lock().expect("缓存锁不跨 await，不会中毒");
            for ((k, _), t) in chunk.iter().zip(got) {
                c.remember(k, &t)?;
                answers.insert(*k, t);
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
                            "打标输出重发后仍被截断 —— BATCH 对 CLASSIFY_MAX_TOKENS \
                             的余量算错了，不是数据问题",
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
                        "上一轮的输出没通过校验：\n{msg}\n\n请按上面的报错修正，重新输出全部行的分类。"
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
) -> Result<Vec<Labels>, String> {
    let mut errs: Vec<String> = Vec::new();
    let mut got: HashMap<usize, Labels> = HashMap::new();
    for a in assignments {
        let i = a.index as usize;
        if !(1..=n).contains(&i) {
            errs.push(format!("index {i} 超出本批范围 1-{n}"));
            continue;
        }
        if let Some(bad) = a.type_ids.iter().find(|t| !known.contains(*t)) {
            errs.push(format!(
                "#{i} 的 type_id「{bad}」不在词表里；只能用列出的那些，归不上去请填 {UNTYPED}"
            ));
            continue;
        }
        if a.type_ids.is_empty() {
            errs.push(format!("#{i} 的 type_ids 是空的，至少要给一个"));
            continue;
        }
        if a.type_ids.len() > MAX_TYPES {
            errs.push(format!(
                "#{i} 给了 {} 个类，最多 {MAX_TYPES} 个；拿不准就只给主类",
                a.type_ids.len()
            ));
            continue;
        }
        if a.type_ids.iter().collect::<BTreeSet<_>>().len() != a.type_ids.len() {
            errs.push(format!("#{i} 的 type_ids 里有重复"));
            continue;
        }
        if a.type_ids.len() > 1 && a.type_ids.iter().any(|t| t == UNTYPED) {
            errs.push(format!(
                "#{i} 把 {UNTYPED} 和别的类混在一起了 —— 归不上去时它只能单独出现"
            ));
            continue;
        }
        if got.insert(i, Labels(a.type_ids)).is_some() {
            errs.push(format!("#{i} 给了不止一行结果，每行只要一行"));
        }
    }
    let missing: Vec<String> = (1..=n)
        .filter(|i| !got.contains_key(i))
        .map(|i| format!("#{i}"))
        .collect();
    if !missing.is_empty() {
        errs.push(format!(
            "漏了 {} 行：{}。每一行都必须有结果",
            missing.len(),
            missing.join(" ")
        ));
    }
    if errs.is_empty() {
        Ok((1..=n)
            .map(|i| got.remove(&i).expect("上面刚查过没有缺行"))
            .collect())
    } else {
        Err(errs.join("\n"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 结果缓存 —— 内容寻址，append-only NDJSON。**承重件**（见模块头）。
// ─────────────────────────────────────────────────────────────────────────────

/// 缓存键是 `sha256(summary)`，**不是 event_id**。
///
/// 用 event_id 会跟分片删重写直接冲突：重跑某个群某天，event 全删重建、id 全变，
/// 落盘的那批答案立刻变成没人认领的孤儿，而新 event 又没有标签。
///
/// 存 hash 不存原文的第二个理由是 PII：`summary` 里有客户姓名和地址
/// （脱敏明确不掩它们），而缓存**只增不减**——原文一旦进去就是永久的。
///
/// ⚠️ **这条只管住这个文件，管不住整个 `cache_dir`。** 同目录的
/// `*-nearest.json`（[`crate::nearest::Nearest::save`]）里 `vocab` 是字符 bigram，
/// 姓名片段可读回来 —— 那份按 0600 落盘，这份不需要。
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
    map: HashMap<[u8; 32], Labels>,
    file: File,
}

impl Cache {
    fn open(path: &Path) -> Result<Self, BoxError> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut map = HashMap::new();
        if let Ok(f) = File::open(path) {
            let mut bad = 0usize;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Entry>(&line)
                    .ok()
                    // 空 `t` 当坏行 —— 判据在 `Labels::from_saved` 里，只写一遍。
                    .and_then(|e| Some((unhex(&e.h)?, Labels::from_saved(e.t)?)))
                {
                    Some((k, t)) => {
                        map.insert(k, t);
                    }
                    // **坏行跳过，不是整轮死。** 缓存缺一条只是多发一次请求（安全）；
                    // 而进程被 SIGKILL 打断时最后一行半截写入是真会发生的。
                    // 但绝不静默：跳过多少条要看得见。
                    None => bad += 1,
                }
            }
            if bad > 0 {
                tracing::warn!(bad, path = %path.display(), "打标缓存有坏行，已跳过（会多发几次请求，不影响正确性）");
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { map, file })
    }

    /// 先落盘再入内存 —— 反过来的话，写文件失败会留下一条「这轮记得、下轮忘了」的
    /// 答案，而那正是确定性要防的抖动。
    fn remember(&mut self, k: &[u8; 32], t: &Labels) -> Result<(), BoxError> {
        let line = serde_json::to_string(&Entry {
            h: hex(k),
            t: t.0.clone(),
        })?;
        writeln!(self.file, "{line}")?;
        self.map.insert(*k, t.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn flat(r: Result<Vec<Labels>, String>) -> Vec<Vec<String>> {
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
        let llm = crate::llm::Llm::new(
            &toml::from_str(
                r#"model = "m"
                   base_url = "http://localhost:1/v1"
                   reasoning_effort = "none"
                   temperature = 0.0
                   max_tokens = 4000
                   timeout_secs = 1
                   connect_timeout_secs = 1"#,
            )
            .unwrap(),
            "k".into(),
        )
        .unwrap();
        let dir = crate::testutil::fresh_root("classify", "order");
        let path = |types: Vec<TaxonomyType>| {
            Classifier::new("v1", types, llm.clone(), &dir)
                .unwrap()
                .model_path()
                .to_path_buf()
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
                .contains("不在词表里")
        );
        assert!(
            validate(asg(&[(1, &["a"])]), 2, &k)
                .unwrap_err()
                .contains("漏了 1 行：#2")
        );
        assert!(
            validate(asg(&[(1, &["a"]), (3, &["b"])]), 2, &k)
                .unwrap_err()
                .contains("超出本批范围 1-2")
        );
        assert!(
            validate(asg(&[(1, &["a"]), (1, &["b"]), (2, &["a"])]), 2, &k)
                .unwrap_err()
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
        let err = |p: &[(u32, &[&str])]| validate(asg(p), 1, &k).unwrap_err();
        assert!(err(&[(1, &[])]).contains("空的"));
        assert!(err(&[(1, &["a", "b", "a"])]).contains("重复"));
        assert!(err(&[(1, &[UNTYPED, "a"])]).contains("单独出现"));
        // MAX_TYPES = 3，给 4 个
        let k4: BTreeSet<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert!(
            validate(asg(&[(1, &["a", "b", "c", "d"])]), 1, &k4)
                .unwrap_err()
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
        let mut c = Cache::open(&p).unwrap();
        c.remember(&k1, &lab(&["cancel", "change_worker"])).unwrap();
        drop(c);

        std::fs::write(
            &p,
            format!("{}{{半截\n", std::fs::read_to_string(&p).unwrap()),
        )
        .unwrap();

        let c = Cache::open(&p).unwrap();
        // 缓存存的是**全集**不是主类 —— 只存主类的话副类每次都要重问，
        // 「同 summary 同答案」就只保住了一半。
        assert_eq!(c.map.get(&k1), Some(&lab(&["cancel", "change_worker"])));
        assert_eq!(c.map.len(), 1, "坏行不该变成一条答案");
    }

    #[test]
    fn hex_round_trips() {
        let k = digest("x");
        assert_eq!(unhex(&hex(&k)), Some(k));
        assert_eq!(unhex("nothex"), None);
    }

    /// 同一批里重复的 summary 只占一个请求位，且结果按原始顺序一一对应还原。
    /// 去重靠 `pending` 集合而不是在 `todo` 里线性找 —— 全量试打时那是 O(n²)。
    #[test]
    fn duplicates_within_one_batch_collapse_to_one_slot() {
        let sums = ["甲", "乙", "甲", "丙", "乙"];
        let keys: Vec<[u8; 32]> = sums.iter().map(|s| digest(s)).collect();
        let mut pending: BTreeSet<[u8; 32]> = BTreeSet::new();
        let todo: Vec<&str> = sums
            .iter()
            .zip(&keys)
            .filter(|(_, k)| pending.insert(**k))
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(todo, ["甲", "乙", "丙"]);
    }

    /// 词表变了就必须换缓存文件 —— 同一个版本号下改草稿是归纳期的常态，
    /// 而旧答案被当成新答案端出来是静默的错。
    #[test]
    fn a_changed_taxonomy_gets_a_different_cache_file() {
        let one = render_system(&[ty("a")]);
        let two = render_system(&[ty("b")]);
        assert_ne!(digest(&one), digest(&two));
    }

    /// 词表为空 == v0：全 `__untyped__`，而且**一个请求都不发**
    /// （`Llm` 在这里根本没被构造，发请求就会 panic 在 unwrap 上）。
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
            Llm::new(&cfg.llm, "sk-none".into()).unwrap(),
            &dir,
        )
        .unwrap();
        assert_eq!(c.type_count(), 0);
        assert_eq!(
            c.classify(&["甲", "乙"]).await.unwrap(),
            [lab(&[UNTYPED]), lab(&[UNTYPED])]
        );
    }

    /// 挂上类心之后，**够自信的那些一条请求都不发** —— 这条测试就是靠
    /// 「发请求必然失败」来断言的：`Llm` 拿的是假 key，真去问模型这里就红了。
    ///
    /// 钉住的是 `classify` 里那三道的顺序和短路：缓存 → 类心 → 模型。
    #[tokio::test]
    async fn a_confident_centroid_answers_without_asking_the_model() {
        let dir = crate::testutil::fresh_root("classify", "centroid");
        let cfg: crate::config::Config = toml::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap(),
        )
        .unwrap();
        let mut c = Classifier::new(
            "v1",
            vec![ty("urge"), ty("swap")],
            Llm::new(&cfg.llm, "sk-none".into()).unwrap(),
            &dir,
        )
        .unwrap();

        let seeds: Vec<(String, Labels)> = [
            ("商家催促订单安装，平台已通知师傅联系客户预约。", "urge"),
            ("商家催促另一订单安装，平台已通知师傅联系客户预约。", "urge"),
            ("商家催促两个订单师傅接单，平台表示正在加速调度中。", "urge"),
            ("商家要求为订单更换能正常上门安装的师傅。", "swap"),
            ("商家要求更换对接师傅，平台核实后表示会换人。", "swap"),
            ("商家要求换一个师傅上门，平台已安排换人。", "swap"),
        ]
        .into_iter()
        .map(|(s, t)| (s.to_string(), lab(&[t])))
        .collect();
        // margin = 0：只要类心认得出就用本地答案，于是 `todo` 会被清空。
        c.attach(crate::nearest::Nearest::train(&seeds), 0.0);

        let got = c
            .classify(&["商家催促订单安装，平台已通知师傅。"])
            .await
            .unwrap();
        assert_eq!(got, [lab(&["urge"])]);
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
