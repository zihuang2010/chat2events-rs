//! `daily` 进程 —— 每日跑批的编排。
//!
//! ① 拉取 → ①② 摄取/会话 → ③④ 抽取/装配 → ⑤⑥ 打标/指标 → ⑦ 落库。跑完即退出。
//!
//! **编排住在这里，不住在 `main`。** `main` 是进程入口，它该做的只有
//! 「读配置、建资源、调 [`run`]」。[`run`] 是个普通 async 函数，测试直接调得到；
//! `#[tokio::main] async fn main()` 调不到。
//!
//! **失败隔离粒度 = 群 × 本次窗口**（承重不变量 3），一共三种失败，语义不同 ——
//! 对照表在 [`crate::store`] 的模块注释里。整轮不因单群失败中止；运行结束汇报
//! 成功/失败群数，有失败就非零码退出（定时任务要看得见，别让 `run_failure`
//! 只躺在库里没人查）。

use crate::{
    Result,
    classify::{self, CURRENT_VERSION},
    config::Config,
    extract::{self, LiveModel},
    ingest::{self, IngestError},
    llm::Llm,
    metrics::{self, Attribution, Status},
    pull, store,
    window::Window,
};
use chrono::NaiveDate;
use sqlx::MySqlPool;
use std::{
    future::Future,
    time::{Duration, Instant},
};

/// 跑一轮。
pub async fn run(config: &Config, llm: &Llm, pool: &MySqlPool) -> Result<()> {
    // DDL 漂移要在第一秒暴露。Python 版踩过：改了 schema.sql 但库没迁移，
    // **抽取跑完 23 分钟才在落库那步炸掉**。跑批是无人值守的。
    store::check_schema(pool).await?;

    let run_date = chrono::Local::now().date_naive();
    let w = Window::new(run_date, config.ingest.lookback_days);
    // **整轮**预算，各阶段共用同一份 —— 每个阶段各给一份就不叫整轮预算了。
    let deadline = Instant::now() + Duration::from_secs(config.daily.round_deadline_secs);

    // ① 拉取 —— 把 OSS 上的月文件增量同步到本地 raw 区。
    // 索引表查不到是**整轮**失败（`?` 上抛）；单个群拉不下来只是这个群的事。
    let unsynced = pull::pull(config, pool, &w, deadline).await?;

    let t0 = Instant::now();
    let mut rooms = ingest::list_rooms(&config.ingest.raw_root, &w);
    // **拉取失败的群本轮一行不写**（承重不变量 3）。这一步同时挡住 CDN 陈旧缓存 ——
    // 拿到旧副本 → 字节数不够 → 该群失败，不会拿残缺数据去出指标。
    rooms.retain(|r| !unsynced.contains(r));

    // 拉取失败的群：**只记 run_failure，`group` 表整行缺失**。和抽取失败不同 ——
    // 那时消息是全的、msg_count 可信；这时连消息都不全，写一个 0 出去就是
    // 「用 0 表示没算出来」，正是承重不变量 4 禁止的事。
    let mut t = Tally { unsynced: unsynced.len(), ..Tally::default() };
    for (corp, room) in &unsynced {
        tracing::error!(corp = %corp, room = %room, "跳过（拉取失败），只记 run_failure");
        store::write_room(
            pool, run_date, corp, room, &w, None, &[], CURRENT_VERSION,
            Some("拉取失败，本轮不参与跑批"), &[], &[],
        )
        .await?;
    }

    let model = std::sync::Arc::new(LiveModel::new(llm.clone()));
    let root = config.ingest.raw_root.clone();
    let segment_msgs = config.extract.segment_msgs;
    run_rooms(
        &rooms,
        config.ingest.room_concurrency,
        deadline,
        &mut t,
        |corp, room| {
            let (root, pool, model, w) = (root.clone(), pool.clone(), model.clone(), w.clone());
            async move {
                run_room(&root, &pool, &model, run_date, &corp, &room, &w, segment_msgs).await
            }
        },
    )
    .await?;

    tracing::info!(
        rooms = rooms.len(),
        ok = t.ok,
        empty = t.empty,
        failed = t.failed,
        unsynced = t.unsynced,
        over_budget = t.over_budget,
        msgs = t.msgs,
        events = t.events,
        since = %w.since(),
        until = %w.until(),
        secs = t0.elapsed().as_secs_f64(),
        "跑批完成"
    );

    // 本轮没跑完必须看得见 —— 绿灯过去的话，指标少了一批群没有任何人会知道。
    if t.over_budget > 0 {
        return Err(format!(
            "整轮预算 {}s 用完，{} 个群没轮到。没跑完的群下一轮重来 —— \
             lookback_days ≥ 2 时漏掉的那天还在窗口里。",
            config.daily.round_deadline_secs, t.over_budget
        )
        .into());
    }
    if t.failed > 0 || t.unsynced > 0 {
        return Err(format!("{} 个群失败、{} 个群未同步，见 run_failure 表", t.failed, t.unsynced).into());
    }
    Ok(())
}

/// 并发跑一批群，一个群一个任务。**只管并发、背压、预算和记账** —— 一个群具体干什么
/// 由 `f` 决定（[`run`] 传的是 [`run_room`]）。
///
/// 把「循环」和「一个群干什么」分开，是为了让循环的三条性质**离线可测**：每个群恰好
/// 记一次 · 到点不算失败 · `Upstream` 整轮死（承重不变量 3）。
/// **这不是给 ⑦ 开接缝** —— `CLAUDE.md` 明确不建存储层抽象接口，`run_room` 里那句
/// `store::write_room` 是写死的。
///
/// **并行只加在群与群之间**（ADR-0004）—— 段之间必须串行（后一段要看前一段的便签）。
/// 在飞的最多 `concurrency` 个，背压靠 `JoinSet` 自己，跟 [`pull`] 一个写法，不用信号量。
///
/// ⚠️ **`room_concurrency` 今天量的是读取（本机核数），③ 接上之后约束变成端点 TPM，
/// 必须重新量**（ADR-0004 给了公式 `N ≈ TPM额度 / 14000`）。
///
/// `deadline` 是**整轮**的预算（`pull` 已经花掉一部分）。到点之后不再开新的群，
/// 在飞的跑完 —— 那是事务边界，砍在半路会让一个群只写进去一半（承重不变量 2）。
async fn run_rooms<F, Fut>(
    rooms: &[(String, String)],
    concurrency: usize,
    deadline: Instant,
    t: &mut Tally,
    f: F,
) -> Result<()>
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = std::result::Result<Outcome, IngestError>> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for (corp, room) in rooms.iter().cloned() {
        if Instant::now() >= deadline {
            t.over_budget += 1;
            continue;
        }
        if set.len() >= concurrency {
            t.record(join(set.join_next().await))?;
        }
        let fut = f(corp.clone(), room.clone());
        set.spawn(async move { (corp, room, fut.await) });
    }
    while let Some(j) = set.join_next().await {
        t.record(join(Some(j)))?;
    }
    Ok(())
}


/// 一个群的全程。**失败隔离粒度就在这里** —— ③④⑤⑥⑦ 任一步出事就地转成 `failed`，
/// 整轮不中止。
///
/// ⚠️ **`Conversation` 在这个任务里就地消费掉，绝不收集起来统一处理** ——
/// 否则 `room_concurrency` 这个内存上界就白设了。
#[allow(clippy::too_many_arguments)]
async fn run_room(
    raw_root: &std::path::Path,
    pool: &MySqlPool,
    model: &LiveModel,
    run_date: NaiveDate,
    corp: &str,
    room: &str,
    w: &Window,
    segment_msgs: usize,
) -> std::result::Result<Outcome, IngestError> {
    // ①② 读：DuckDB 是同步阻塞的，**必须挪出 runtime 线程** —— 留在上面跑的话，
    // 每读一个群，N 个在飞的模型调用全被卡在同一个线程上（硬规则点名的那条）。
    let (root, corp2, room2, w2) = (raw_root.to_path_buf(), corp.to_string(), room.to_string(), w.clone());
    let conv = tokio::task::spawn_blocking(move || ingest::read_room(&root, &corp2, &room2, &w2))
        .await
        .expect("read_room 的每条失败路径都返回 Err，不 panic")?;

    // 有文件但窗口内一条消息都没有 —— 这个群本轮没发生任何事，**不写任何行**。
    if conv.msgs.is_empty() {
        return Ok(Outcome::Empty);
    }
    let msgs = conv.msgs.len();

    // ③④ 抽取装配。`Ok(vec![])` 是合法结果（这几天确实没有业务事件），
    // 与 `Err` **绝不混淆**（承重不变量 4）。
    let (events, reason) = match extract::extract(&conv.msgs, model, segment_msgs).await {
        Ok(evs) => (Some(evs), None),
        Err(e) => {
            tracing::error!(corp = %corp, room = %room, "抽取失败，该群本轮 failed：{e}");
            (None, Some(e.to_string()))
        }
    };
    let status = if events.is_some() { Status::Ok } else { Status::Failed };
    let evs = events.as_deref().unwrap_or(&[]);

    // ⑤ 打标 —— **在事务外**算好传给 ⑥ 和 ⑦：在 store 里逐个调，v1 缓存未命中就是
    // 持锁发 N 次 embedding 请求，而且会让 store 反向依赖 classify。
    let types: Vec<&str> = evs.iter().map(|e| classify::classify(&e.summary)).collect();

    // ⑥ 指标：消息级搭 ① 的车（不依赖抽取，失败的群照样有），事件级读刚抽出来的事实。
    let group = metrics::group_rows(corp, room, w, &conv.msg_counts, events.as_deref(), status);
    let agent = metrics::agent_rows(corp, room, evs, &types, CURRENT_VERSION, Attribution::default());

    // ⑦ 落库 —— 一个群一个事务（承重不变量 2）
    store::write_room(
        pool, run_date, corp, room, w, events.as_deref(), &types, CURRENT_VERSION,
        reason.as_deref(), &group, &agent,
    )
    .await
    .map_err(|e| IngestError::Room(format!("落库失败：{e}")))?;

    Ok(match events {
        Some(evs) => Outcome::Ok { msgs, events: evs.len() },
        None => Outcome::Failed { msgs },
    })
}

/// 一个群跑完了的三种结局。抽取失败**已经落过库**（`group` 行 + `run_failure`），
/// 这里只是把数字带回去汇总。
#[derive(Debug)]
enum Outcome {
    /// 窗口内一条消息都没有 —— 不写任何行。
    Empty,
    Ok { msgs: usize, events: usize },
    Failed { msgs: usize },
}

/// 一个群跑完了：它是谁、结局如何（或读取阶段就失败了）。
type RoomResult = (String, String, std::result::Result<Outcome, IngestError>);

/// 跑批那一行日志要的几个数。收成一个类型是为了让 [`Self::record`] 在两个排空点
/// （循环里的背压、循环后的收尾）复用同一份处置逻辑 —— 那段逻辑分了「整轮死」和
/// 「群级跳过」两条通道，抄两遍迟早抄岔。
#[derive(Default, Debug)]
struct Tally {
    msgs: usize,
    events: usize,
    ok: usize,
    /// 窗口内没有消息 —— **既不是成功也不是失败**，一行都没写。
    empty: usize,
    failed: usize,
    /// 拉取阶段就失败的群，只记了 `run_failure`。
    unsynced: usize,
    /// 整轮预算用完、根本没开始的群。**跟 `failed` 分开计** —— 那是"跑了但坏了"，
    /// 这是"没轮到"，两者下一轮的处置一样，但看日志时的诊断完全不同。
    over_budget: usize,
}

impl Tally {
    fn record(&mut self, (corp, room, r): RoomResult) -> Result<()> {
        match r {
            Ok(Outcome::Empty) => self.empty += 1,
            Ok(Outcome::Ok { msgs, events }) => {
                self.ok += 1;
                self.msgs += msgs;
                self.events += events;
            }
            Ok(Outcome::Failed { msgs }) => {
                self.failed += 1;
                self.msgs += msgs;
            }
            // 上游解析器变了 —— 不是某个群的事，整轮退出，不做兼容层。
            // 提前返回会把 `set` 丢掉：已经在跑的任务打断不了，但进程本来就要退了。
            Err(e @ IngestError::Upstream(_)) => return Err(e.into()),
            // 其余都是该群的事：整体跳过、一行不写、**整轮继续**（承重不变量 3）。
            Err(e) => {
                self.failed += 1;
                tracing::error!(corp = %corp, room = %room, "读取失败，该群本轮跳过：{e}");
            }
        }
        Ok(())
    }
}

/// 任务本身不会 panic（每条失败路径都是 `Err`），所以 `JoinError` 只可能是 bug ——
/// 「构造已经保证、不可能为假」那一档，跟 `pull::join` 一个规矩。
fn join(j: Option<std::result::Result<RoomResult, tokio::task::JoinError>>) -> RoomResult {
    j.expect("set.len() > 0 时必有一个可 join")
        .expect("run_room 的每条失败路径都返回 Err，不 panic")
}

#[cfg(test)]
mod tests;
