//! 跑一轮：抽取群独立保存事件，再经有界 channel 交给打标队列。
//!
//! **失败隔离粒度 = 群 × 本次窗口**（承重不变量 3）。整轮不因单群失败中止；
//! 结局和记账在 `tally.rs`。

use super::{
    classify::{ClassifyTask, classify_room, run_classification},
    tally::{Outcome, Tally},
};
use crate::{
    Result,
    classify::{CURRENT_VERSION, Classifier},
    config::{Config, OssSecrets},
    extract::{self, Event, LiveModel},
    ingest::{self, IngestError},
    join,
    llm::Llm,
    metrics::{self, Status},
    mirror, store,
    window::Window,
};
use chrono::{Datelike, NaiveDate};
use sqlx::MySqlPool;
use std::{
    collections::BTreeMap,
    future::Future,
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc};

/// 跑一轮 —— 日常跑批。窗口是 `[T-(N+1), T-2]`，`T` = 今天。
///
/// 补跑历史走 [`run_span`]：**不要**拿这个函数循环喂过去的日期，那样窗口两两重叠，
/// 同一天会被抽两遍，白烧一倍 token。
pub async fn run(
    config: &Config,
    extract_llm: &Llm,
    classify_llm: &Llm,
    pool: &MySqlPool,
    oss: &OssSecrets,
) -> Result<()> {
    let run_date = chrono::Local::now().date_naive();
    let w = Window::new(run_date, config.ingest.lookback_days);
    run_span(config, extract_llm, classify_llm, pool, oss, run_date, w).await
}

/// 跑一轮，窗口由调用方给。
///
/// ⚠️ **宽窗口会写穿冻结区（承重不变量 1）。** `store::stray_days` 校验的是
/// 「事件落在**传进来的**窗口内」—— 窗口一宽，那道守卫就跟着一起放宽了。
/// 这是补跑该有的样子（人工授权的整体删重写），但调用方有义务把这件事喊出来，
/// 别让补跑在日志上跟日常跑批长得一模一样。见 `examples/backfill.rs`。
///
/// ⚠️ **两个 `Llm` 是两个不同的模型**（`[llm.extract]` / `[llm.classify]`），传反了
/// 编译过、测试也过 —— 唯一能第一秒看见的地方是 `main` 那两行启动日志和
/// `Classifier::new` 里那句「大模型分类策略就绪」打出来的 `model`。
pub async fn run_span(
    config: &Config,
    extract_llm: &Llm,
    classify_llm: &Llm,
    pool: &MySqlPool,
    oss: &OssSecrets,
    run_date: NaiveDate,
    w: Window,
) -> Result<()> {
    // DDL 漂移要在第一秒暴露。踩过一次：改了 schema.sql 但库没迁移，
    // **抽取跑完 23 分钟才在落库那步炸掉**。跑批是无人值守的。
    store::check_schema(pool).await?;

    // ⑤ 的词表和打标缓存 —— **整轮构造一次**，跟 `Llm` / 连接池一个待遇。
    // 读不到词表 / 缓存目录写不了 = 整轮死（启动期资源，本来就该整轮死），
    // 不是某个群的事：那样每个群都会各失败一次，账记一千遍原因是同一个。
    // 只有显式 v0 允许空词表，正式版本缺词表在构造时失败。
    // ⚠️ **挪出 runtime 线程**：`Classifier::new` 里的 `Cache::open` 是同步的，要把整个
    // 打标缓存逐行读进内存（`classify.cache_dir` 下那个 ndjson 只增不减）。跟
    // `run_room` 里那句 DuckDB 读取同一条理由 —— 阻塞调用不留在 runtime 线程上。
    let types = store::read_taxonomy(pool, CURRENT_VERSION).await?;
    let (llm2, cache_dir) = (classify_llm.clone(), config.classify.cache_dir.clone());
    let classifier = tokio::task::spawn_blocking(move || {
        Classifier::new(CURRENT_VERSION, types, llm2, &cache_dir)
    })
    .await
    .expect("Classifier::new 的每条失败路径都返回 Err，不 panic")?;
    tracing::info!(
        taxonomy_version = CURRENT_VERSION,
        types = classifier.type_count(),
        "词表就绪{}",
        if classifier.type_count() == 0 {
            "（空 = v0，全部 __untyped__，不发打标请求）"
        } else {
            ""
        }
    );

    // **整轮**预算，各阶段共用同一份 —— 每个阶段各给一份就不叫整轮预算了。
    let deadline = Instant::now() + Duration::from_secs(config.daily.round_deadline_secs);

    // **开轮第一行就是窗口。** 「跑完了但库里没数据」最常见的原因是窗口和数据错开
    // （样本停更、lookback 配小了），而那条信息此前只出现在收尾那行日志里 ——
    // 排查的人得先读完一整轮日志才看得到自己读的是哪两天。
    //
    // 不打 `lookback_days`：日常跑批里它恒等于 `days`（那才是它的直接后果），
    // 补跑时窗口根本不由它定 —— 打出来只会是一个和实际窗口对不上的数。
    tracing::info!(
        run_date = %run_date,
        since = %w.since(),
        until = %w.until(),
        days = w.days().len(),
        deadline_secs = config.daily.round_deadline_secs,
        "开始跑批"
    );

    // ① 拉取 —— 把 OSS 上的月文件增量同步到本地 raw 区。
    // 索引表查不到是**整轮**失败（`?` 上抛）；单个群拉不下来只是这个群的事。
    let synced = mirror::sync(config, pool, oss, &w, deadline).await?;
    let unsynced = synced.failed;
    let room_months = synced.rooms;

    // 保留期 —— **紧跟拉取，不放收尾**：收尾那儿有好几条提前 return，只要有群失败
    // 就轮不到清理，磁盘偏偏会在最该清的那些天继续涨。删除起点锚在窗口上
    // （见 `ingest::prune`），本轮要读的月份不可能被删。
    //
    // ⚠️ **同步阻塞调用，故意没有 `spawn_blocking`。** `prune` 是 `fs::remove_dir_all`，
    // 一次删掉整个月目录（1000 群约 18 GB），在慢盘上是秒级到分钟级的阻塞。
    // 放在这里安全的**唯一理由是位置**：拉取已经收工、`run_rooms` 还没开始，
    // 此刻 runtime 上没有任何在飞的任务会被它饿到。
    // **把这一句挪进任何循环、或挪到群任务开始之后，就必须同时改成 `spawn_blocking`**
    // ——那时它卡住的是别的群的模型调用。挪线程挡不住「挪进循环」这个错误
    // （挪进去之后每轮删一次照样是错的），所以这条约束写在这里而不是靠改写法来防。
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
    // 群与月份都来自本轮同步结果；本地历史文件存在不代表仍获索引允许。
    let mut rooms: Vec<_> = room_months.keys().cloned().collect();
    rotate_daily(&mut rooms, run_date);

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
        if let Err(e) =
            record_failure(pool, run_date, corp, room, &w, "拉取失败，本轮不参与跑批").await
        {
            tracing::error!(corp = %corp, room = %room, "连 run_failure 都没记上：{e}");
        }
    }

    let model = std::sync::Arc::new(LiveModel::new(extract_llm.clone()));
    let root = config.ingest.raw_root.clone();
    let segment_msgs = config.extract.segment_msgs;
    let concurrency = config.classify.concurrency;
    let slots = Semaphore::new(concurrency);
    // ⚠️ **通道深度挂 `room_concurrency`，不是 `classify.concurrency`。**
    // 抽取侧比打标侧宽（10 对 6），深度跟着窄的那边走会让抽完的群堵在投递上 ——
    // 而它堵着的时候仍占着一个抽取名额，实际抽取并发就低于 10。
    // 背压仍然存在（通道满了照样等），只是不再由更窄的那一队来定。
    let (tx, rx) = mpsc::channel(config.ingest.room_concurrency);
    let extraction = async {
        let result = run_rooms(
            &rooms,
            config.ingest.room_concurrency,
            deadline,
            &mut t,
            |corp, room| {
                let months = room_months[&(corp.clone(), room.clone())].clone();
                let (root, pool, model, w, tx) = (
                    root.clone(),
                    pool.clone(),
                    model.clone(),
                    w.clone(),
                    tx.clone(),
                );
                async move {
                    let outcome = run_room(
                        &root,
                        &pool,
                        &model,
                        run_date,
                        &corp,
                        &room,
                        &w,
                        &months,
                        segment_msgs,
                    )
                    .await?;
                    if let Outcome::Ok { events, .. } = &outcome {
                        tx.send(ClassifyTask {
                            corpid: corp,
                            roomid: room,
                            event_count: *events,
                        })
                        .await
                        .map_err(|_| {
                            IngestError::Room("打标队列已关闭，事件已保存但任务未发送".into())
                        })?;
                    }
                    Ok(outcome)
                }
            },
        )
        .await;
        drop(tx);
        result
    };
    let classification = run_classification(rx, concurrency, |task| {
        classify_room(pool, &classifier, &slots, concurrency, run_date, &w, task)
    });
    // 即使抽取遇到整轮错误，也排空已经交接的打标任务。
    let (extracted, classified) = tokio::join!(extraction, classification);
    extracted?;

    // 「没轮到」和「拉取失败」走**同一条通道**。少这一行账，库里对这批群就是整行
    // 缺失 —— 几个月后没有任何东西解释得了报表上的洞，因为 `Tally` 只活在这一轮的内存里。
    //
    // 逐个群**不打日志**：它们的原因逐字相同，收尾那行的 `over_budget=` 已经说清了数量，
    // 而可诊断的那一份现在在库里。这一点和上面 `unsynced` 那个循环不同 —— 拉取失败
    // 逐群各有各的原因，值得逐条打。
    for (corp, room) in &t.skipped {
        // 记账失败不掀翻整轮，跟 `unsynced` 那条通道一个待遇：这个群本轮本来就没跑，
        // 少一行账是少一条账，不是多一份坏数据。整轮仍会因为它非零码退出。
        if let Err(e) =
            record_failure(pool, run_date, corp, room, &w, "整轮预算用完，本轮没轮到").await
        {
            tracing::error!(corp = %corp, room = %room, "连 run_failure 都没记上：{e}");
        }
    }

    // ⑧ 产能账 —— **本轮实测，不是估算**。
    //
    // 「这一轮跑得完吗」此前在跑批里没有任何一个数回答得了：`over_budget` 只说
    // 「有多少群没轮到」，说不出「差多远」，而唯一能推它的输入（索引表的
    // `ndjson_record_count`）是**整月**记录数、窗口只有两天，推出来高估一个数量级。
    // 所以不在开轮预测，改成收工用实测外推 —— 分子是真的烧掉的模型秒。
    //
    // ⚠️ **分母只能用 `extracted`（真的跑完的群），不能用 `rooms.len()`**：没轮到的、
    // 拉取失败的、空窗口的都没发过请求，掺进去会把每群成本摊薄。
    // ⚠️ **不要拿 `secs` 当分母**：`t0` 起算在拉取和清理之后，量的是抽取+打标；
    // 而 `deadline` 那个预算是覆盖拉取的。外推只用 `model_secs`，与 `t0` 无关。
    //
    // 只外推抽取阶段；分类吞吐与排空时间必须另外观察，不能当作整轮完成时间。
    let usage = model.usage();
    let projected_extract_hours = (t.ok > 0).then(|| {
        usage.secs / t.ok as f64 * rooms.len() as f64
            / config.ingest.room_concurrency as f64
            / 3600.0
    });
    tracing::info!(
        rooms = rooms.len(),
        extracted = t.ok,
        classified = classified.ok,
        classify_failed = classified.failed,
        empty = t.empty,
        failed = t.failed,
        unsynced = t.unsynced,
        over_budget = t.skipped.len(),
        msgs = t.msgs,
        events = t.events,
        segments = usage.calls,
        model_secs = usage.secs,
        projected_extract_hours,
        deadline_hours = config.daily.round_deadline_secs as f64 / 3600.0,
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
    if !t.skipped.is_empty() {
        return Err(format!(
            "整轮预算 {}s 用完，{} 个群没轮到（各记一行 run_failure）。没跑完的群下一轮\
             重来 —— lookback_days ≥ 2 时漏掉的那天还在窗口里，且队首每天挪一格，\
             不会总是同一批群。",
            config.daily.round_deadline_secs,
            t.skipped.len()
        )
        .into());
    }
    if t.failed > 0 || t.unsynced > 0 || classified.failed > 0 {
        return Err(format!(
            "{} 个群抽取或保存失败、{} 个群打标失败、{} 个群未同步，见 run_failure 表；记账失败见日志",
            t.failed, classified.failed, t.unsynced
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
/// 抽取在群之间并行；段之间串行。打标由另一个队列限制并发。
/// 在飞的最多 `concurrency` 个，背压靠 `JoinSet` 自己，跟 `mirror` 一个写法，不用信号量。
///
/// `deadline` 是**整轮**的预算（`mirror` 已经花掉一部分）。到点之后不再开新的群，
/// 在飞的跑完 —— 那是事务边界，砍在半路会让一个群只写进去一半（承重不变量 2）。
/// 没轮到的群进 [`Tally::skipped`]，**调用方有义务给它们补 `run_failure`** ——
/// 本函数不认识 `store`，所以它只能把名单交出去，不能自己写。
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
        if set.len() >= concurrency {
            t.record(join(set.join_next().await))?;
        }
        if Instant::now() >= deadline {
            // 走 `record` 而不是直接 push：四种结局共用同一个 match，
            // 承重不变量 3 的处置点仍然只有一处。
            t.record((corp, room, Ok(Outcome::Skipped)))?;
            continue;
        }
        let fut = f(corp.clone(), room.clone());
        set.spawn(async move { (corp, room, fut.await) });
    }
    while let Some(j) = set.join_next().await {
        t.record(join(Some(j)))?;
    }
    Ok(())
}

/// 队首每天往后挪 [`STRIDE`] 格。
///
/// **`BTreeMap::keys()` 给的是 (corp, room) 字典序，而预算到点是从队尾开始砍的** ——
/// 顺序固定就意味着字母序末尾那批群天天被砍。那不是「偶尔漏一天」，是**永久没有数据**。
/// 转一格不改变损失总量（每轮照样丢那么多群），只把损失从「固定砸在同一批群头上」摊成
/// 「轮流」。⚠️ **它不提速** —— 提速是 `room_concurrency` 和端点吞吐那条线的事。
///
/// 分出来只为让那条性质**离线可测**：`run_span` 要真 MySQL 才跑得动，而偏移写错
/// 在生产上是**静默的** —— 那批群继续天天被砍，日志上一个字都不会变。
pub(super) fn rotate_daily(rooms: &mut [(String, String)], run_date: NaiveDate) {
    if !rooms.is_empty() {
        let n = rooms.len();
        // 先取模再乘：`num_days_from_ce()` 到 2026 年约 74 万，乘一个和 n 同量级的
        // 步长仍在 usize 内，但先取模让它跟群数无关地稳住。
        let day = run_date.num_days_from_ce() as usize % n;
        rooms.rotate_left(day * stride(n) % n);
    }
}

/// 队首每天往后挪多少格。**绝不是 1，也不能是一个写死的常量。**
///
/// 挪一格意味着「跑得完的那一段」每天只滑动一位 —— 一个群一旦掉出那一段，就要等
/// 队伍绕完**整整一圈**才回来，空档 = `群数 - 跑得完的数` 天，**而且是连续的**。
/// 实测（n=1000、每轮跑得完 400、跑 1200 天）：**某个群连续 600 天一行数据都没有**。
/// 「轮流」这个词在挪一格时是骗人的：它轮的是*哪一批*被砍，不是*什么时候*被砍。
///
/// ⚠️ **写死一个大质数不管用** —— 起作用的是 `STRIDE % n`，不是 STRIDE 本身。
/// 踩过：取 9973 时 `9973 % 1000 = 973 ≡ -27`，窗口每天只挪 27 位，空档仍有 **23 天**。
/// 常量必然对某些 n 退化，而 n 是数据决定的（本轮同步成功的群数），不由我们挑。
///
/// 所以步长**从 n 算出来**：取黄金分割 `n × 0.618`，再往上找到第一个与 n 互质的数。
/// 黄金分割是三距离定理的最优点 —— `d × s mod n` 这串点对**任意宽度**的窗口都近似
/// 等分布，而窗口宽度（每轮跑得完几个群）恰恰是我们**事先不知道**的那个量。
/// 互质保证 n 天内跑遍全部位置，不会只在少数几个位置之间跳。
///
/// 实测最长空档（n 从 37 到 5000，覆盖率 20%~80%）：**≤ 7 天**，多数是 2 天。
/// 覆盖率跌破 20% 之后空档还会涨 —— 但那时缺的是产能，不是轮转策略，
/// 换任何排队顺序都救不了。
///
/// **不改变损失总量**：每轮照样丢那么多群，只是把连续 600 天摊成 2 天，
/// 而 `lookback_days ≥ 2` 时隔一天没跑到还能靠下一轮的窗口重叠补回来。
fn stride(n: usize) -> usize {
    let mut s = ((n as f64 * 0.618_033_988_75) as usize).max(1);
    while gcd(s, n) != 1 {
        s += 1;
    }
    s
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// 一个群的读取、抽取和事实保存。成功后由调用方发送打标任务。
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
    months: &[String],
    segment_msgs: usize,
) -> std::result::Result<Outcome, IngestError> {
    // ①② 读：DuckDB 是同步阻塞的，**必须挪出 runtime 线程** —— 留在上面跑的话，
    // 每读一个群，N 个在飞的模型调用全被卡在同一个线程上（硬规则点名的那条）。
    let (root, corp2, room2, w2, months) = (
        raw_root.to_path_buf(),
        corp.to_string(),
        room.to_string(),
        w.clone(),
        months.to_vec(),
    );
    let conv = tokio::task::spawn_blocking(move || {
        ingest::read_synced_room(&root, &corp2, &room2, &w2, &months)
    })
    .await
    .expect("read_room 的每条失败路径都返回 Err，不 panic");
    let conv = match conv {
        Ok(conv) => conv,
        Err(e @ IngestError::Upstream(_)) => return Err(e),
        Err(e) => {
            let reason = format!("读取失败：{e}");
            if let Err(write_error) = record_failure(pool, run_date, corp, room, w, &reason).await {
                return Err(IngestError::Room(format!(
                    "{reason}；记录 run_failure 失败：{write_error}"
                )));
            }
            return Err(e);
        }
    };

    // 有文件但窗口内一条消息都没有 —— 这个群本轮没发生任何事，**不写任何行**。
    // 走 debug 不走 info：1000 个群时这会是 1000 行，而它是排障信息不是运行状态
    // （运行状态由收尾那行的 `empty=` 计数承担）。
    if conv.msgs.is_empty() {
        tracing::debug!(corp = %corp, room = %room, "窗口内没有消息，不写任何行");
        return Ok(Outcome::Empty);
    }
    // 拆开后在抽取装配结束即释放明文；分类等待期间只保留摘要和消息级计数。
    let ingest::Conversation { msgs, msg_counts } = conv;
    let n_msgs = msgs.len();

    // 空事件是成功；抽取失败仍保留上一轮事实。
    let extracted = extract::extract(&msgs, model, segment_msgs).await;
    // 消息按时间升序，同一员工保留窗口内最新的非空账号；只保留账号映射即可释放正文。
    let accounts: std::collections::BTreeMap<String, String> = msgs
        .iter()
        .filter_map(|m| {
            m.official_user_id
                .as_ref()
                .map(|account| (m.sender_id.clone(), account.clone()))
        })
        .collect();
    drop(msgs);
    let (extracted, reason) = match extracted {
        Ok(evs) => (Some(evs), None),
        Err(e) => {
            tracing::error!(corp = %corp, room = %room, "抽取失败，该群本轮 failed：{e}");
            (None, Some(e.to_string()))
        }
    };

    let status = if extracted.is_some() {
        Status::Ok
    } else {
        Status::Failed
    };
    let events: Option<&[Event]> = extracted.as_deref();

    // ⑥ 指标：消息级搭 ① 的车（不依赖抽取，失败的群照样有），事件级读刚抽出来的事实。
    let group = metrics::group_rows(corp, room, w, &msg_counts, events, status);

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
            store::Shard::new(corp, room, w),
            events,
            reason.as_deref(),
            &group,
            &accounts,
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

    Ok(match extracted {
        Some(evs) => Outcome::Ok {
            msgs: n_msgs,
            events: evs.len(),
        },
        None => Outcome::Failed { msgs: n_msgs },
    })
}

/// 消息尚未完整读取时，只写失败记录，不修改事实与指标。
async fn record_failure(
    pool: &MySqlPool,
    run_date: NaiveDate,
    corp: &str,
    room: &str,
    w: &Window,
    reason: &str,
) -> Result<()> {
    store::write_room(
        pool,
        run_date,
        store::Shard::new(corp, room, w),
        None,
        Some(reason),
        &[],
        &BTreeMap::new(),
    )
    .await
}
