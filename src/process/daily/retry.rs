//! 人工重跑 —— 按 `run_failure` 找出还没修好的失败群，自动挑群、自动定窗口。
//!
//! 「对指定群跑抽取＋分类」这件事本身由 [`run_span`] 早就做得了
//! （`src/bin/backfill.rs` 的第 4 个参数起就是群列表）。**这里补的只有
//! 「该重跑哪些群、哪个窗口」** —— 此前那一段是人工查 SQL 抄出来的。
//!
//! **两支的窗口策略不同，这是整个文件的支点**：
//!
//! | 支 | 窗口是什么 | 给宽了会怎样 | 所以 |
//! |---|---|---|---|
//! | 抽取（[`run_span`]） | **删重写范围** | 重抽已成功的天、写穿冻结区（不变量 1） | 严格按失败行自带的窗口分组，一组一趟 |
//! | 打标（[`recover`]） | **筛选范围** | 什么都不会发生（不命中就不做） | 取并集，一趟跑完 |
//!
//! 判据本身在 `store::unrepaired_extract_failures` 的文档注释里。

use super::{recover::recover, run::run_span};
use crate::{
    Result,
    config::{Config, OssSecrets},
    llm::Llm,
    stage::store,
    window::Window,
};
use chrono::{Local, NaiveDate};
use sqlx::MySqlPool;

/// `since` 夹的是 **`run_date`（跑批日）**，不是数据日 —— 运维视角是「重跑最近
/// 一周失败的」，而且 `run_failure` 上恰好有 `idx_run(run_date)`。
///
/// `dry_run` 只打印计划，**一个模型请求都不发**。这不是可有可无的方便功能：
/// 这个入口会烧 token，而且窗口一旦早于日常窗口就写穿冻结区 —— 跑之前看一眼
/// 规模和窗口是最小的安全阀。
// 七个参数跟 `run_span` 一个待遇：只有一个调用点（`src/bin/retry.rs`），
// 且全是「整趟就这一份」的资源，打包成 struct 换不来任何东西。
#[allow(clippy::too_many_arguments)]
pub async fn retry(
    config: &Config,
    extract_llm: &Llm,
    classify_llm: &Llm,
    pool: &MySqlPool,
    oss: &OssSecrets,
    since: NaiveDate,
    dry_run: bool,
) -> Result<()> {
    // DDL 漂移在第一秒暴露，跟 `run_span` / `recover` 一个待遇。
    store::check_schema(pool).await?;

    let (rows, legacy) = store::unrepaired_extract_failures(pool, since).await?;
    if legacy > 0 {
        // **不能静默吞掉**：否则「retry 查不到东西」会被读成「都好了」，
        // 而库里还躺着一批永远修不到的群。
        tracing::warn!(
            legacy,
            "跳过 {legacy} 条没有窗口的历史失败行（`window_since IS NULL`，加这两列之前写的）\
             —— 它们按「影响全历史」保守处理，自动重跑等于把整个历史重抽一遍。\
             真要修得人工挑群跑 backfill，见 docs/deploy.md"
        );
    }

    // 查询已按 `(window_since, window_until, roomid)` 排序，顺序扫一遍就分完组。
    let mut groups: Vec<((NaiveDate, NaiveDate), Vec<String>)> = Vec::new();
    for (room, s, u) in rows {
        match groups.last_mut() {
            Some((w, rooms)) if *w == (s, u) => rooms.push(room),
            _ => groups.push(((s, u), vec![room])),
        }
    }
    let total_rooms: usize = groups.iter().map(|(_, rooms)| rooms.len()).sum();

    // **冻结区告警**（承重不变量 1 的可见性要求）—— 凡是能写事实列的入口都必须有，
    // 文案与 `src/bin/backfill.rs` 同源：不能让重跑在日志上跟日常跑批长得一模一样。
    let run_date = Local::now().date_naive();
    let frozen_before = Window::new(run_date, config.ingest.lookback_days).since();
    let frozen: Vec<_> = groups
        .iter()
        .filter(|((s, _), _)| *s < frozen_before)
        .collect();
    // 组按 `window_since` 升序，所以第一组就是最早的那个窗口。
    if let Some(((earliest, _), _)) = frozen.first() {
        tracing::warn!(
            groups = frozen.len(),
            rooms = frozen.iter().map(|(_, r)| r.len()).sum::<usize>(),
            %earliest,
            %frozen_before,
            "有 {} 组窗口覆盖冻结区：{} 之前的事实列将被整体删重写。\
             这是人工授权的重来，不是日常跑批 —— 确认这是你要的。",
            frozen.len(),
            frozen_before
        );
    }

    for ((s, u), rooms) in &groups {
        tracing::info!(
            since = %s,
            until = %u,
            rooms = rooms.len(),
            // 抽头几个就够挑一个群先用 backfill 单跑一遍验证；全列出来在上千个群时是一行天书。
            sample = %rooms.iter().take(5).cloned().collect::<Vec<_>>().join(","),
            "待重跑"
        );
    }
    if dry_run {
        let span = store::classify_failure_span(pool, since).await?;
        tracing::info!(
            groups = groups.len(),
            rooms = total_rooms,
            legacy_skipped = legacy,
            classify_since = ?span.map(|(s, _)| s),
            classify_until = ?span.map(|(_, u)| u),
            "空跑：以上是计划，一个模型请求都没发。去掉 --dry-run 才真跑"
        );
        return Ok(());
    }

    // **逐组串行，绝不能 `join!`。** `run_span` 内部每趟各建一个 `Classifier`，
    // 而分类缓存是**单写者互斥**的（`classify::Cache::open` 的文件锁，
    // drop 时才释放）—— 并发跑第二组会被自己的第一组挡在锁外面，报成
    // 「无法独占分类缓存」。串行同时也让 `room_concurrency` 那个内存上界仍然成立。
    let (mut ok, mut failed) = (0usize, 0usize);
    for ((s, u), rooms) in &groups {
        // 单组失败不中止其余组（承重不变量 3 的形状）：失败的群会重新写一行
        // `run_failure`，下一趟 retry 照样捞得到。
        match run_span(
            config,
            extract_llm,
            classify_llm,
            pool,
            oss,
            run_date,
            Window::span(*s, *u),
            rooms,
        )
        .await
        {
            Ok(()) => ok += 1,
            Err(error) => {
                failed += 1;
                tracing::error!(since = %s, until = %u, "这一组重跑失败，继续下一组：{error}");
            }
        }
    }

    // **重新查一次打标失败的窗口** —— 上面那一轮重跑自己也可能打标失败，
    // 顺手把它们一起捞进来，不用等下一趟。
    let classified = match store::classify_failure_span(pool, since).await? {
        Some((s, u)) => {
            tracing::info!(since = %s, until = %u, "补齐打标失败的群日");
            Some(recover(config, classify_llm, pool, s, u).await)
        }
        None => None,
    };

    tracing::info!(
        groups = groups.len(),
        rooms = total_rooms,
        ok,
        failed,
        legacy_skipped = legacy,
        classify_ok = classified.as_ref().is_some_and(|r| r.is_ok()),
        "重跑结束"
    );
    if let Some(Err(error)) = classified {
        tracing::error!("补标失败：{error}");
        failed += 1;
    }
    if failed > 0 {
        return Err(format!("{failed} 组重跑或补标失败，见上面的日志与 run_failure 表").into());
    }
    Ok(())
}
