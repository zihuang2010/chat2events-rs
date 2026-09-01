//! 一轮怎么调度 —— **失败不中断整轮**，返回本轮不该参与跑批的群。
//!
//! 背压靠 `JoinSet` 自己（跟 `daily::run_rooms` 一个写法，不用信号量）；
//! `deadline` 是**整轮**的预算，由 `daily::run` 算好传进来，不是这个函数自己的。

use super::{
    download::{CONNECT_TIMEOUT, Outcome, TIMEOUT, download_with_retry},
    error::{MirrorError, Result},
    index::{MonthFile, list_month_files},
};
use crate::{config::Config, ingest, join, window::Window};
use sqlx::MySqlPool;
use std::{collections::BTreeSet, time::Instant};

/// 拉取窗口覆盖的全部月文件。**失败不中断整轮**，返回本轮不该参与跑批的群。
///
/// 返回 `(corp, room)`：一个群跨月有两行，**任一行失败整个群就作废** ——
/// 承重不变量 3，少一个窗口就重写等于用残缺数据覆盖完整数据。
///
/// `deadline` 是**整轮**的预算（`daily::run` 算的，不是这个函数自己的）。到点之后
/// 不再启动新的下载，剩下的文件按「没拉成」处理 —— 本地那份很可能缺今天的字节，
/// 拿去跑批就是用残缺数据覆盖完整数据，正是不变量 3 要禁的事。
pub async fn sync(
    cfg: &Config,
    pool: &MySqlPool,
    w: &Window,
    deadline: Instant,
) -> Result<BTreeSet<(String, String)>> {
    let files = list_month_files(pool, w).await?;
    tracing::info!(
        months = %ingest::months(w).join(","),
        files = files.len(),
        "索引表"
    );

    let http = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| MirrorError::Round(format!("HTTP 客户端构建失败：{e}")))?;
    let mut set = tokio::task::JoinSet::new();
    let mut done: Vec<(MonthFile, Result<Outcome>)> = Vec::with_capacity(files.len());
    let mut failed_by_deadline = BTreeSet::new();
    let mut over_budget = 0usize;

    for f in files {
        // 整轮预算用完 —— 不再开新的，但已经在飞的让它跑完（那是事务边界）。
        if Instant::now() >= deadline {
            over_budget += 1;
            failed_by_deadline.insert((f.corp, f.room));
            continue;
        }
        // 背压：在飞的最多 mirror_concurrency 个。JoinSet 自己就够了，不用再加信号量。
        if set.len() >= cfg.ingest.mirror_concurrency {
            done.push(join(set.join_next().await));
        }
        let http = http.clone();
        let base = cfg.ingest.download_base_url.clone();
        let root = cfg.ingest.raw_root.clone();
        set.spawn(async move {
            let r = download_with_retry(&http, &base, &root, &f).await;
            (f, r)
        });
    }
    while let Some(j) = set.join_next().await {
        done.push(join(Some(j)));
    }

    let mut failed = failed_by_deadline;
    let (mut pulled, mut skipped, mut empty, mut bytes) = (0usize, 0usize, 0usize, 0u64);
    for (f, r) in &done {
        match r {
            Ok(Outcome::Pulled(n)) => {
                pulled += 1;
                bytes += n;
                tracing::debug!(room = %f.room, bytes = n, "已追加");
            }
            Ok(Outcome::Skip) => skipped += 1,
            Ok(Outcome::Empty) => empty += 1,
            Err(e) => {
                failed.insert((f.corp.clone(), f.room.clone()));
                tracing::error!(
                    room = %f.room,
                    month = %f.month,
                    "拉取失败，该群本轮不参与跑批：{e}"
                );
            }
        }
    }
    if over_budget > 0 {
        tracing::error!(
            over_budget,
            "整轮预算用完，这些月文件本轮没拉，对应的群作废"
        );
    }
    tracing::info!(
        pulled,
        skipped,
        empty,
        over_budget,
        failed = failed.len(),
        bytes,
        "拉取完成"
    );
    Ok(failed)
}
