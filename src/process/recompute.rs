//! 词表升版后的**重打标** —— 人工触发，`src/bin/recompute.rs` 是它的入口。
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
    config::Config,
    join,
    llm::Llm,
    stage::classify::{CURRENT_VERSION, Classifier, Labels},
    stage::metrics::{self, Attribution},
    stage::store,
    window::Window,
};
use chrono::NaiveDate;
use sqlx::MySqlPool;
use std::sync::Arc;
use tokio::task::JoinSet;

/// 把 `[since, until]` 内所有群的 event 按 `version` 这版词表重打一遍标，
/// 并重算客服日指标。
///
/// `concurrency` 由命令行指定；所有未缓存摘要交给大模型，不自动训练。
pub async fn run(
    config: &Config,
    llm: &Llm,
    pool: &MySqlPool,
    version: &str,
    since: NaiveDate,
    until: NaiveDate,
    concurrency: usize,
) -> Result<()> {
    assert!(concurrency >= 1, "并发度至少 1");
    store::check_schema(pool).await?;

    let types = store::read_taxonomy(pool, version).await?;
    // v0 不用于历史重打标；正式词表统一由 Classifier 构造时校验。
    if version == "v0" {
        return Err("重打标不支持 v0".into());
    }
    if version != CURRENT_VERSION {
        tracing::warn!(
            version,
            current = CURRENT_VERSION,
            "重打标用的版本和 classify::CURRENT_VERSION 不一致：明天的跑批会把 \
             日常跑批窗口按 {} 重新打回去，那些日期将和其余日期用不同的词表。\
             升版的正确顺序是「插词表 → 改 CURRENT_VERSION 重新编译部署 → 跑这个」。",
            CURRENT_VERSION
        );
    }

    let classifier = Classifier::new(version, types, llm.clone(), &config.classify.cache_dir)?;
    // `Arc` 只为跨任务共享；分类请求不持有缓存锁，提交时统一采纳答案。
    let classifier = Arc::new(classifier);
    // 「群 × 这段区间」是重打标全程的作用域。`Window::span` 顺带兜住「since > until」
    // —— 那两个数来自命令行，错了该在第一秒炸，不该跑到 SQL 里去查一个空区间。
    let days = Window::span(since, until);
    let rooms = store::read_event_rooms(pool, since, until).await?;
    tracing::info!(
        version,
        types = classifier.type_count(),
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
        let (c, p, d) = (classifier.clone(), pool.clone(), days.clone());
        set.spawn(async move {
            let r = retag_room(&p, &c, &corp, &room, &d).await;
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
    days: &Window,
) -> Result<u64> {
    let shard = store::Shard::new(corp, room, days);
    // `ids` 是给 [`store::retag_room`] 定位行用的，**不进 `Event`**（事实列契约）。
    let (ids, events) = store::read_events(pool, shard).await?;
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
    store::retag_room(pool, shard, &ids, &labels, classifier.version(), &agent).await
}
