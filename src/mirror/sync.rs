//! 一轮怎么调度 —— 返回本轮同步成功的群与月份，以及失败群。
//!
//! 背压靠 `JoinSet` 自己（跟 `daily::run_rooms` 一个写法，不用信号量）；
//! `deadline` 是**整轮**的预算，由 `daily::run` 算好传进来，不是这个函数自己的。

use super::{
    download::{Outcome, download_with_retry},
    error::Result,
    index::{MonthFile, list_month_files},
    oss::OssClient,
};
use crate::{
    config::{Config, OssSecrets},
    ingest, join,
    window::Window,
};
use sqlx::MySqlPool;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Instant,
};

/// 跑批只读本轮索引允许且同步成功的月份，不能从 raw 目录重新发现旧文件。
pub struct SyncResult {
    pub rooms: BTreeMap<(String, String), Vec<String>>,
    pub failed: BTreeSet<(String, String)>,
}

/// 拉取窗口覆盖的全部月文件。**失败不中断整轮**。
///
/// 成功名单保留每个群的月份：一个群跨月有两行，**任一行失败整个群就作废** ——
/// 承重不变量 3，少一个窗口就重写等于用残缺数据覆盖完整数据。
///
/// `deadline` 是**整轮**的预算（`daily::run` 算的，不是这个函数自己的）。到点之后
/// 不再启动新的下载，剩下的文件按「没拉成」处理 —— 本地那份很可能缺今天的字节，
/// 拿去跑批就是用残缺数据覆盖完整数据，正是不变量 3 要禁的事。
pub async fn sync(
    cfg: &Config,
    pool: &MySqlPool,
    secrets: &OssSecrets,
    w: &Window,
    deadline: Instant,
) -> Result<SyncResult> {
    let oss = Arc::new(OssClient::new(&cfg.ingest.oss, secrets)?);
    let files = list_month_files(pool, w).await?;
    tracing::info!(
        months = %ingest::months(w).join(","),
        files = files.len(),
        "索引表"
    );

    Ok(sync_files(
        files,
        oss,
        &cfg.ingest.raw_root,
        cfg.ingest.mirror_concurrency,
        deadline,
    )
    .await)
}

/// 索引读取和文件同步分开，离线测试可用真实本地文件及 HTTP 响应驱动同步全程。
async fn sync_files(
    files: Vec<MonthFile>,
    oss: Arc<OssClient>,
    raw_root: &Path,
    concurrency: usize,
    deadline: Instant,
) -> SyncResult {
    let mut set = tokio::task::JoinSet::new();
    let mut done: Vec<(MonthFile, Result<Outcome>)> = Vec::with_capacity(files.len());
    let mut failed_by_deadline = BTreeSet::new();
    let mut over_budget = 0usize;

    for f in files {
        // 空位可能要等到截止点之后，预算必须在等待之后再判。
        if set.len() >= concurrency {
            done.push(join(set.join_next().await));
        }
        // 整轮预算用完 —— 不再开新的，但已经在飞的让它跑完（那是事务边界）。
        if Instant::now() >= deadline {
            over_budget += 1;
            failed_by_deadline.insert((f.corp, f.room));
            continue;
        }
        let oss = oss.clone();
        let root = raw_root.to_path_buf();
        set.spawn(async move {
            let r = download_with_retry(&oss, &root, &f).await;
            (f, r)
        });
    }
    while let Some(j) = set.join_next().await {
        done.push(join(Some(j)));
    }

    let mut failed = failed_by_deadline;
    let mut rooms: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let (mut pulled, mut skipped, mut empty, mut bytes) = (0usize, 0usize, 0usize, 0u64);
    for (f, r) in &done {
        if matches!(r, Ok(Outcome::Pulled(_) | Outcome::Skip)) {
            rooms
                .entry((f.corp.clone(), f.room.clone()))
                .or_default()
                .push(f.month.clone());
        }
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
    // 任一月份失败，整群不能用另一月份的成功结果继续跑。
    rooms.retain(|room, _| !failed.contains(room));
    SyncResult { rooms, failed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    const BODY: &[u8] = b"{\"a\":1}\n";

    fn file(room: &str, month: &str) -> MonthFile {
        MonthFile {
            corp: "C".into(),
            room: room.into(),
            month: month.into(),
            object_key: String::new(),
            position: BODY.len() as u64,
            record_count: 1,
        }
    }

    fn cached(root: &Path, f: &MonthFile) {
        let path = ingest::room_path(root, &f.month, &f.corp, &f.room);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, BODY).unwrap();
    }

    #[tokio::test]
    async fn only_indexed_months_are_readable_and_one_failed_month_excludes_the_room() {
        let root = testutil::fresh_root("mirror-sync", "manifest");
        let current = file("good", "202609");
        let old = file("good", "202608");
        let excluded = file("excluded", "202609");
        let bad_good_month = file("bad", "202608");
        let bad_missing_month = file("bad", "202609");
        let mut empty = file("empty", "202609");
        for f in [&current, &old, &excluded, &bad_good_month, &empty] {
            cached(&root, f);
        }
        empty.position = 0;
        empty.record_count = 0;
        // 未列入索引的历史群和月份都仍在磁盘；缺文件的空 object_key 直接失败，不联网。
        let result = sync_files(
            vec![current, bad_good_month, bad_missing_month, empty],
            Arc::new(OssClient::for_test("http://127.0.0.1:1")),
            &root,
            2,
            Instant::now() + Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            result.rooms,
            BTreeMap::from([(("C".into(), "good".into()), vec!["202609".into()])])
        );
        assert_eq!(result.failed, BTreeSet::from([("C".into(), "bad".into())]));
        assert!(ingest::room_path(&root, &old.month, &old.corp, &old.room).exists());
        assert!(ingest::room_path(&root, &excluded.month, &excluded.corp, &excluded.room).exists());
    }

    #[tokio::test]
    async fn waiting_for_download_capacity_rechecks_the_deadline() {
        let root = testutil::fresh_root("mirror-sync", "deadline");
        let mut first = file("first", "202609");
        first.object_key = "first.ndjson".into();
        let second = file("second", "202609");
        cached(&root, &second);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let oss = Arc::new(OssClient::for_test(&format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        let deadline = Instant::now() + Duration::from_millis(300);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            std::thread::sleep(
                (deadline + Duration::from_millis(20)).saturating_duration_since(Instant::now()),
            );
            write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nContent-Length: 8\r\nConnection: close\r\n\r\n").unwrap();
            stream.write_all(BODY).unwrap();
        });
        let result = sync_files(vec![first, second], oss, &root, 1, deadline).await;
        assert!(result.rooms.contains_key(&("C".into(), "first".into())));
        assert!(!result.rooms.contains_key(&("C".into(), "second".into())));
        assert_eq!(
            result.failed,
            BTreeSet::from([("C".into(), "second".into())])
        );
        server.join().unwrap();
    }
}
