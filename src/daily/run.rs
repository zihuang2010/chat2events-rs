//! 跑一轮：① 拉取 → ①② 摄取/会话 → ③④ 抽取/装配 → ⑤⑥ 打标/指标 → ⑦ 落库。
//!
//! **失败隔离粒度 = 群 × 本次窗口**（承重不变量 3）。整轮不因单群失败中止；
//! 结局和记账在 `tally.rs`。

use super::tally::{Outcome, Tally};
use crate::{
    Result,
    classify::{self, CURRENT_VERSION},
    config::Config,
    extract::{self, LiveModel},
    ingest::{self, IngestError},
    join,
    llm::Llm,
    metrics::{self, Attribution, Status},
    mirror, store,
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

    // **开轮第一行就是窗口。** 「跑完了但库里没数据」最常见的原因是窗口和数据错开
    // （样本停更、lookback 配小了），而那条信息此前只出现在收尾那行日志里 ——
    // 排查的人得先读完一整轮日志才看得到自己读的是哪两天。
    tracing::info!(
        run_date = %run_date,
        since = %w.since(),
        until = %w.until(),
        days = w.days().len(),
        lookback_days = config.ingest.lookback_days,
        deadline_secs = config.daily.round_deadline_secs,
        "开始跑批"
    );

    // ① 拉取 —— 把 OSS 上的月文件增量同步到本地 raw 区。
    // 索引表查不到是**整轮**失败（`?` 上抛）；单个群拉不下来只是这个群的事。
    let unsynced = mirror::sync(config, pool, &w, deadline).await?;

    // 保留期 —— **紧跟拉取，不放收尾**：收尾那儿有好几条提前 return，只要有群失败
    // 就轮不到清理，磁盘偏偏会在最该清的那些天继续涨。删除起点锚在窗口上
    // （见 `ingest::prune`），本轮要读的月份不可能被删。
    let pruned = ingest::prune(
        &config.ingest.raw_root,
        &w,
        config.ingest.raw_retention_months,
    );
    if pruned > 0 {
        tracing::info!(
            pruned,
            retention_months = config.ingest.raw_retention_months,
            "清理过保留期的月目录"
        );
    }

    let t0 = Instant::now();
    let mut rooms = ingest::list_rooms(&config.ingest.raw_root, &w);
    // **拉取失败的群本轮一行不写**（承重不变量 3）。这一步同时挡住 CDN 陈旧缓存 ——
    // 拿到旧副本 → 字节数不够 → 该群失败，不会拿残缺数据去出指标。
    rooms.retain(|r| !unsynced.contains(r));

    // 拉取失败的群：**只记 run_failure，`group` 表整行缺失**。和抽取失败不同 ——
    // 那时消息是全的、msg_count 可信；这时连消息都不全，写一个 0 出去就是
    // 「用 0 表示没算出来」，正是承重不变量 4 禁止的事。
    let mut t = Tally {
        unsynced: unsynced.len(),
        ..Tally::default()
    };
    for (corp, room) in &unsynced {
        tracing::error!(corp = %corp, room = %room, "跳过（拉取失败），只记 run_failure");
        // **记账失败不掀翻整轮**（承重不变量 3，和 `run_room` 里那条 store 失败同一条通道）：
        // 这个群本来就已经作废，少一行 `run_failure` 是少一条账，不是多一份坏数据。
        // 库整个连不上的话 `check_schema` 在开轮第一秒就炸过了 —— 走到这里的是瞬时抖动。
        // 整轮仍然会因为 `unsynced > 0` 非零码退出，不会绿灯过去。
        if let Err(e) = store::write_room(
            pool,
            run_date,
            corp,
            room,
            &w,
            None,
            &[],
            CURRENT_VERSION,
            Some("拉取失败，本轮不参与跑批"),
            &[],
            &[],
        )
        .await
        {
            tracing::error!(corp = %corp, room = %room, "连 run_failure 都没记上：{e}");
        }
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
                run_room(
                    &root,
                    &pool,
                    &model,
                    run_date,
                    &corp,
                    &room,
                    &w,
                    segment_msgs,
                )
                .await
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

    // 全部群窗口内都没有消息 —— **一行都没写，但这不是失败**（新部署的第一天、
    // 长假、上游停更都会这样）。不改退出码，但必须响一声：此前它和「跑得好好的」
    // 在日志上长得一模一样，而库里是空的。
    if t.empty > 0 && t.empty == rooms.len() {
        tracing::warn!(
            rooms = rooms.len(),
            since = %w.since(),
            until = %w.until(),
            "全部群在本轮窗口内都没有消息，一行都没写 —— \
             先确认上游是否停更，或窗口（lookback_days）是否和数据错开"
        );
    }

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
        return Err(format!(
            "{} 个群失败、{} 个群未同步，见 run_failure 表",
            t.failed, t.unsynced
        )
        .into());
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
/// 在飞的最多 `concurrency` 个，背压靠 `JoinSet` 自己，跟 `mirror` 一个写法，不用信号量。
///
/// ⚠️ **`room_concurrency` 今天量的是读取（本机核数），③ 接上之后约束变成端点 TPM，
/// 必须重新量**（ADR-0004 给了公式 `N ≈ TPM额度 / 14000`）。
///
/// `deadline` 是**整轮**的预算（`mirror` 已经花掉一部分）。到点之后不再开新的群，
/// 在飞的跑完 —— 那是事务边界，砍在半路会让一个群只写进去一半（承重不变量 2）。
pub(super) async fn run_rooms<F, Fut>(
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
pub(super) async fn run_room(
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
    let (root, corp2, room2, w2) = (
        raw_root.to_path_buf(),
        corp.to_string(),
        room.to_string(),
        w.clone(),
    );
    let conv = tokio::task::spawn_blocking(move || ingest::read_room(&root, &corp2, &room2, &w2))
        .await
        .expect("read_room 的每条失败路径都返回 Err，不 panic")?;

    // 有文件但窗口内一条消息都没有 —— 这个群本轮没发生任何事，**不写任何行**。
    // 走 debug 不走 info：1000 个群时这会是 1000 行，而它是排障信息不是运行状态
    // （运行状态由收尾那行的 `empty=` 计数承担）。
    if conv.msgs.is_empty() {
        tracing::debug!(corp = %corp, room = %room, "窗口内没有消息，不写任何行");
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
    let status = if events.is_some() {
        Status::Ok
    } else {
        Status::Failed
    };
    let evs = events.as_deref().unwrap_or(&[]);

    // ⑤ 打标 —— **在事务外**算好传给 ⑥ 和 ⑦：在 store 里逐个调，v1 缓存未命中就是
    // 持锁发 N 次 embedding 请求，而且会让 store 反向依赖 classify。
    let types: Vec<&str> = evs.iter().map(|e| classify::classify(&e.summary)).collect();

    // ⑥ 指标：消息级搭 ① 的车（不依赖抽取，失败的群照样有），事件级读刚抽出来的事实。
    let group = metrics::group_rows(corp, room, w, &conv.msg_counts, events.as_deref(), status);
    let agent = metrics::agent_rows(
        corp,
        room,
        evs,
        &types,
        CURRENT_VERSION,
        Attribution::default(),
    );

    // ⑦ 落库 —— 一个群一个事务（承重不变量 2）
    //
    // **失败重试一次。** 走到这里，这个群的模型调用已经跑完、token 已经烧掉 ——
    // 一次连接抖动（池子被别的群占满、库在重启）不该把这份成果整个扔掉。
    // 重试是安全的：失败的事务已经回滚，而 `write_room` 本来就是按分片删重写 + REPLACE，
    // 跑两遍和跑一遍等价。**不睡** —— 会失败的那几种情况（池子满、锁等待）本身就已经
    // 等满了各自的超时，再睡只是让整轮更长。
    //
    // ⚠️ **只在抽取成功时重试。** 抽取失败那条路写的是 `run_failure` —— 追加，不幂等，
    //    重试会在「提交成功但回包丢了」那个窗口里写出第二行。而它值不了这个价：
    //    那个群已经作废，少一行 `run_failure` 只是少一条账，和拉取失败那条通道一个待遇。
    let write = || {
        store::write_room(
            pool,
            run_date,
            corp,
            room,
            w,
            events.as_deref(),
            &types,
            CURRENT_VERSION,
            reason.as_deref(),
            &group,
            &agent,
        )
    };
    let mut wrote = write().await;
    if let Err(e) = &wrote
        && events.is_some()
    {
        tracing::warn!(corp = %corp, room = %room, "落库失败，重试一次：{e}");
        wrote = write().await;
    }
    wrote.map_err(|e| IngestError::Room(format!("落库失败：{e}")))?;

    Ok(match events {
        Some(evs) => Outcome::Ok {
            msgs,
            events: evs.len(),
        },
        None => Outcome::Failed { msgs },
    })
}
