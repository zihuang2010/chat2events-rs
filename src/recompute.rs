//! 词表升版后的**重打标** —— 人工触发，`examples/recompute.rs` 是它的入口。
//!
//! 跟 `daily` 是两个进程，别混：
//!   * `daily` 每天自动跑，**分片删重写**事实列 + 现算标注列。
//!   * 这里人工触发，**只写标注列**（`event_type` / `taxonomy_version`），
//!     事实列一个字节不动 —— 所以冻结区照样能重打标，那正是标注列的定义
//!     （「任何时候可写，但只有词表升版这一个原因」）。
//!
//! **按群分批**（硬规则：不把大数据集读进内存）—— 重打标一个季度上千个群，
//! 一次读全量就是几百万个 `Event` 同时在内存里。失败隔离粒度跟着也是群：
//! 一个群出事记一条错、整轮继续，最后非零码退出。
//!
//! **群与群之间并发**（并行只加在群与群之间）。曾经是串行 ——
//! 理由「一次性人工动作 + 缓存让重跑免费」在**首次**重打标上不成立：那一轮缓存全空。
//! 实测去重率只有 8~21%（三份样本 466 / 724 / 1932 条 summary），所以 25 万事件
//! 就是 25 万次真实打标。墙钟由**输出吞吐**定（`config.toml` 实测单流 105~116 tok/s）：
//! 4.5M 输出 token ≈ 11.4 流·小时 —— 并发 12 才勉强压进 1 小时，取 16 留余量。
//!
//! ⚠️ **并发度必须 ≤ `mysql.max_connections`**，理由见 `config.toml` 那条注释：
//! 小于它就会有群在写库时排队等连接，`acquire_timeout` 一到报的是「落库失败」，
//! 那个群整轮作废、刚烧完的 token 白花。
//!
//! ⚠️ 并行只在**群之间**。⑤ 内部（一个群的多个 50 条批次）仍然串行 ——
//! 不是因为有依赖（打标批次之间没有便签），是因为今天不需要：真出现单群占全量
//! 两成的掉队户，再把并行下推进 `classify`，那是另一件事、另一份实测。

use crate::{
    Result,
    classify::{CURRENT_VERSION, Classifier, Labels},
    config::Config,
    join,
    llm::Llm,
    metrics::{self, Attribution},
    nearest::Nearest,
    store,
};
use chrono::NaiveDate;
use sqlx::MySqlPool;
use std::sync::Arc;
use tokio::task::JoinSet;

/// 把 `[since, until]` 内所有群的 event 按 `version` 这版词表重打一遍标，
/// 并重算客服日指标。
///
/// `concurrency` / `seed` 走命令行：它们是**每次跑的决定**（当天的端点额度、
/// 这一版词表要用多少种子），不是稳定取值。
///
/// **`margin` 反过来，走配置** —— `daily` 每天也要用同一个值，两边不一致就会让
/// 冻结区和 `[T-2, T-1]` 用两套口径（承重不变量 1）。
///
/// `seed = 0` 退回老路：不训练、不写模型文件、全部问模型。
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: &Config,
    llm: &Llm,
    pool: &MySqlPool,
    version: &str,
    since: NaiveDate,
    until: NaiveDate,
    concurrency: usize,
    seed: usize,
) -> Result<()> {
    assert!(concurrency >= 1, "并发度至少 1");
    store::check_schema(pool).await?;

    let types = store::read_taxonomy(pool, version).await?;
    // **空词表在这里是错误，不是 v0。** `daily` 那边空词表=「还没上词表」的正常状态；
    // 但专门跑一趟把所有 event 重打成 `__untyped__` 没有任何意义，多半是版本号打错了。
    if types.is_empty() {
        return Err(format!(
            "词表 {version} 在 b_merchant_group_taxonomy 里一行都没有 —— \
             先人工插入词表（examples/taxonomy.rs 的 emit-sql）再重打标"
        )
        .into());
    }
    if version != CURRENT_VERSION {
        tracing::warn!(
            version,
            current = CURRENT_VERSION,
            "重打标用的版本和 classify::CURRENT_VERSION 不一致：明天的跑批会把 \
             [T-2, T-1] 这两天按 {} 重新打回去，那两天将和其余日期用不同的词表。\
             升版的正确顺序是「插词表 → 改 CURRENT_VERSION 重新编译部署 → 跑这个」。",
            CURRENT_VERSION
        );
    }

    // ⚠️ **训练期这个 `Classifier` 必须还没挂类心** —— 种子标签要是模型给的答案，
    // 拿旧类心去标种子等于用上一版模型教自己，训完只会把旧偏差固化下来。
    let mut classifier = Classifier::new(version, types, llm.clone(), &config.classify.cache_dir)?;
    let mut centroids = 0;
    if seed > 0 {
        let m = train(pool, &classifier, since, until, seed).await?;
        m.save(classifier.model_path())?;
        tracing::info!(path = %classifier.model_path().display(), "类心模型已写出，daily 下一轮起自动加载");
        centroids = m.class_count();
        classifier.attach(m, config.classify.margin);
    }
    // `Arc` 只为跨任务共享 —— `Classifier` 内部的缓存锁本来就是 `Mutex`，
    // 临界区不跨 await（见 `classify::Classifier::classify`），并发下依旧成立。
    let classifier = Arc::new(classifier);
    let rooms = store::read_event_rooms(pool, since, until).await?;
    tracing::info!(
        version,
        types = classifier.type_count(),
        centroids,
        margin = config.classify.margin,
        rooms = rooms.len(),
        concurrency,
        %since,
        %until,
        "开始重打标"
    );

    // 背压形状照 `daily::run_rooms`：`set.len()` 到并发上限就先收一个再放一个。
    // **不复用那个函数** —— 它绑死了 `Outcome` / `IngestError` / `Tally` 三个类型，
    // 为它加泛型比这里抄 10 行贵。
    let mut set: JoinSet<RoomResult> = JoinSet::new();
    let mut t = Tally::default();
    for (corp, room) in rooms.iter().cloned() {
        if set.len() >= concurrency {
            t.record(join(set.join_next().await), rooms.len());
        }
        // `MySqlPool` 的 clone 是池的 `Arc` 克隆，不是新连接。
        let (c, p) = (classifier.clone(), pool.clone());
        set.spawn(async move {
            let r = retag_room(&p, &c, &corp, &room, since, until).await;
            (corp, room, r)
        });
    }
    while !set.is_empty() {
        t.record(join(set.join_next().await), rooms.len());
    }

    // ⚠️ MySQL 默认只数**值真的变了**的行 —— 重跑一次这里是 0，那是幂等不是失败。
    tracing::info!(
        ok = t.ok,
        failed = t.failed,
        changed = t.changed,
        "重打标完成"
    );
    if t.failed > 0 {
        return Err(format!("{} 个群重打标失败，见上面的 error 日志", t.failed).into());
    }
    Ok(())
}

/// 一个群跑完了：它是谁、改了几行（或为什么失败）。
type RoomResult = (String, String, Result<u64>);

/// 记账。两个排空点（循环里的背压、循环后的收尾）复用同一份 —— 抄两遍迟早抄岔，
/// 跟 `daily::tally` 是同一个理由。
#[derive(Default)]
struct Tally {
    ok: usize,
    failed: usize,
    changed: u64,
    done: usize,
}

impl Tally {
    /// **承重不变量 3 的处置点**：任一群失败 → 记一条错、整轮继续。
    fn record(&mut self, (corp, room, r): RoomResult, total: usize) {
        match r {
            Ok(n) => {
                self.ok += 1;
                self.changed += n;
            }
            Err(e) => {
                self.failed += 1;
                tracing::error!(corp = %corp, room = %room, "重打标失败，跳过该群：{e}");
            }
        }
        self.done += 1;
        if self.done.is_multiple_of(50) {
            tracing::info!(done = self.done, total, "重打标进度");
        }
    }
}

async fn retag_room(
    pool: &MySqlPool,
    classifier: &Classifier,
    corp: &str,
    room: &str,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<u64> {
    // `ids` 是给 [`store::retag_room`] 定位行用的，**不进 `Event`**（事实列契约）。
    let (ids, events) = store::read_events(pool, corp, room, since, until).await?;
    if events.is_empty() {
        return Ok(0);
    }
    let sums: Vec<&str> = events.iter().map(|e| e.summary.as_str()).collect();
    let labels = classifier.classify(&sums).await?;
    // ⑥ 只吃主类，⑦ 拿整份 `Labels`（主类 + 全集）—— 见 `classify::Labels`。
    let types: Vec<&str> = labels.iter().map(Labels::primary).collect();

    // 口径跟 `daily` 一致 —— 换口径是另一件事，不该藏在重打标里顺手做了。
    let agent = metrics::agent_rows(
        corp,
        room,
        &events,
        &types,
        classifier.version(),
        Attribution::default(),
    );
    store::retag_room(
        pool,
        corp,
        room,
        since,
        until,
        &ids,
        &labels,
        classifier.version(),
        &agent,
    )
    .await
}

/// 「LLM 教一次」：拿**高频前 `seed` 条**去重摘要问模型，用答案训练类心。
///
/// 取高频而不是随机，是因为 `read_summary_counts` 已经按出现次数降序给好了 ——
/// 高频那一头覆盖的事件最多，同样的种子预算买到的覆盖率最高。
///
/// ⚠️ 这一步**把全部去重摘要读进内存**（25 万条约几十 MB）。硬规则说的是
/// 不把大数据集读进内存，这里破一次例：词表归纳在同一张表上做的是
/// 同一件事，而重打标是人工触发的一次性进程，不是无人值守的跑批。
///
/// 种子答案会顺带填进打标缓存，所以**第二次跑这一步几乎不花钱**。
async fn train(
    pool: &MySqlPool,
    classifier: &Classifier,
    since: NaiveDate,
    until: NaiveDate,
    seed: usize,
) -> Result<Nearest> {
    let counts = store::read_summary_counts(pool, since, until).await?;
    let take = seed.min(counts.len());
    tracing::info!(
        distinct = counts.len(),
        seed = take,
        "开始训练类心：先让模型标种子"
    );
    let sums: Vec<&str> = counts.iter().take(take).map(|(s, _)| s.as_str()).collect();
    let labels = classifier.classify(&sums).await?;
    let pairs: Vec<(String, Labels)> = sums.iter().map(|s| s.to_string()).zip(labels).collect();
    evaluate(&pairs);
    Ok(Nearest::train(&pairs))
}

/// 留出集自检 —— **跑之前先看这张表再定 `margin`**。
///
/// 每 5 条抽 1 条当验证集，用另外 4/5 训练，报几个阈值下的两个数：
///   * **覆盖率** —— 有多少条敢本地贴（剩下的回落问模型，是成本不是错误）
///   * **一致率** —— 敢贴的那些里，跟模型答案一样的占多少（**这才是质量**）
///
/// 一致率上不去就把 `margin` 调高（覆盖率随之下降），或者干脆 `seed = 0`
/// 退回全部问模型。**它只打印不拦人** —— 拿多少一致率换多少钱是人的决定。
fn evaluate(pairs: &[(String, Labels)]) {
    let (train, hold): (Vec<_>, Vec<_>) = pairs
        .iter()
        .enumerate()
        .partition(|(i, _)| !i.is_multiple_of(5));
    let train: Vec<(String, Labels)> = train.into_iter().map(|(_, p)| p.clone()).collect();
    if train.is_empty() || hold.is_empty() {
        tracing::warn!("种子太少，跳过留出集自检");
        return;
    }
    let m = Nearest::train(&train);
    tracing::info!(
        train = train.len(),
        hold = hold.len(),
        centroids = m.class_count(),
        "留出集自检（margin / 覆盖率 / 一致率）"
    );
    // **留出集只跑一遍 predict**，五个阈值共用这一份结果。
    // 此前是阈值套在外层、`predict` 在里层，同一条摘要被算五遍 —— 而 `margin`
    // 跟阈值无关（它是最近类心与次近之差，`nearest.rs`），阈值只是拿它比大小。
    let scored: Vec<(f32, bool)> = hold
        .iter()
        .filter_map(|(_, (s, want))| m.predict(s).map(|h| (h.margin, h.labels == want)))
        .collect();

    for th in [0.0f32, 0.02, 0.05, 0.10, 0.20] {
        let (mut covered, mut agree) = (0usize, 0usize);
        for (margin, ok) in &scored {
            if *margin >= th {
                covered += 1;
                if *ok {
                    agree += 1;
                }
            }
        }
        let pct = |x: usize, n: usize| {
            if n == 0 {
                0.0
            } else {
                x as f64 / n as f64 * 100.0
            }
        };
        tracing::info!(
            margin = th,
            coverage = format!("{:.1}%", pct(covered, hold.len())),
            agreement = format!("{:.1}%", pct(agree, covered)),
            "留出集"
        );
    }
}
