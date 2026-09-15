//! 一轮怎么调度 —— 返回本轮同步成功的群与月份，以及失败群。
//!
//! 背压靠 `JoinSet` 自己（跟 `daily::run_rooms` 一个写法，不用信号量）；
//! `deadline` 是**整轮**的预算，由 `daily::run` 算好传进来，不是这个函数自己的。

use super::{
    download::{Outcome, download_with_retry},
    error::{MirrorError, Result},
    index::{MonthFile, list_month_files},
    oss::OssClient,
};
use crate::{
    config::{Config, OssSecrets},
    join,
    stage::ingest,
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
///
/// `only` 非空 = **只跑这几个群**（`officialRoomId`，也就是文件名，承重不变量 8），
/// 人工重跑单群用；空切片 = 本轮索引里的全部群，日常跑批走这条。
///
/// **过滤落在这里，不落在 `daily::run_span` 的群列表上。** 那儿过滤时月文件已经
/// 拉完了 —— 为跑 1 个群把上千个群的文件全过一遍（本地已有也要一次 HEAD）。
/// 放在索引之后、下载之前，才是「只拉这个群」而不是「拉完再挑」。
/// 连带好处：`SyncResult.failed` 自然也只含目标群，`run_span` 不会给不相干的群
/// 记 `run_failure`、把退出码搞成非零。
pub async fn sync(
    cfg: &Config,
    pool: &MySqlPool,
    secrets: &OssSecrets,
    w: &Window,
    deadline: Instant,
    only: &[String],
) -> Result<SyncResult> {
    let oss = Arc::new(OssClient::new(&cfg.ingest.oss, secrets)?);
    let files = pick(list_month_files(pool, w).await?, only)?;
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

/// 按 `only` 挑群（空 = 全部放行）。
///
/// **分出来只为让「拼错的 roomid 不静默跑 0 个群」这条性质离线可测** —— [`sync`]
/// 自己要真 MySQL（`list_month_files`）才跑得动，跟 [`sync_files`] 同一个理由。
fn pick(mut files: Vec<MonthFile>, only: &[String]) -> Result<Vec<MonthFile>> {
    if only.is_empty() {
        return Ok(files);
    }
    let listed = files.len();
    let keep: BTreeSet<&str> = only.iter().map(String::as_str).collect();
    // 挑中的群，**跨月的每一行都留下**：`sync_files` 靠「任一月份失败整群作废」保证
    // 完整性（不变量 3），这里少留一个月等于用残缺数据覆盖完整数据。
    files.retain(|f| keep.contains(f.room.as_str()));
    // **群 ID 是人手敲进命令行的**，打错一个字符就会拉 0 个文件、跑 0 个群，
    // 然后绿灯退出 —— 运维看到的是「跑完了」，实际什么都没重跑。
    // 真实的信任边界，显式炸掉；`Round` 不是 `Room`：这是参数错，不是某个群的事。
    if files.is_empty() {
        return Err(MirrorError::Round(format!(
            "指定的 {} 个群在本轮索引里一个都没命中（窗口内共 {listed} 个月文件）\
             —— 确认 roomid 拼写和日期窗口",
            only.len()
        )));
    }
    tracing::info!(rooms = only.len(), files = files.len(), "只跑指定的群");
    Ok(files)
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
            let r = download_with_retry(&oss, &root, &f, deadline).await;
            (f, r)
        });
    }
    while let Some(j) = set.join_next().await {
        done.push(join(Some(j)));
    }

    let mut failed = failed_by_deadline;
    let mut rooms: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let (mut pulled, mut skipped, mut empty, mut bytes) = (0usize, 0usize, 0usize, 0u64);
    let mut partial = 0usize;
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
            // ⚠️ **没追平 = 失败**，走和拉取失败同一条通道。上面那个 `matches!` 不认
            // `Partial`，所以这个月不会进 `rooms`，这个群本轮不抽 —— 那正是要的：
            // 抽取跑在截断的月文件上会算出偏少的事件、并写下 `ok` 和事实凭据，
            // 那天就成了「已知成功群日」进分母，而少的那批永远没人知道。
            // 已经落盘的字节推进了本地 `have`，下一轮从那里接着拉。
            Ok(Outcome::Partial(n)) => {
                partial += 1;
                bytes += n;
                failed.insert((f.corp.clone(), f.room.clone()));
                tracing::error!(
                    room = %f.room, month = %f.month, bytes = n,
                    "整轮预算用完时还没追平索引位置，该群本轮不参与跑批；下一轮从已落盘处续"
                );
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
        partial,
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

    /// 本文件的形状：对象名留空（意外进入请求路径就立即失败，测试不访问网络），
    /// 字节数跟着 `BODY`。其余字段走 `mirror/tests.rs` 的共享 fixture。
    fn file(room: &str, month: &str) -> MonthFile {
        MonthFile {
            object_key: String::new(),
            position: BODY.len() as u64,
            ..crate::stage::mirror::tests::file(room, month)
        }
    }

    fn cached(root: &Path, f: &MonthFile) {
        let path = ingest::room_path(root, &f.month, &f.corp, &f.room);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, BODY).unwrap();
    }

    /// 挑群：跨月的行要全留，拼错的 roomid 要**显式炸**而不是静默跑 0 个群。
    ///
    /// 后半条是这个参数唯一危险的地方 —— 运维拿它重跑失败的群，参数打错一个字符
    /// 而进程绿灯退出的话，看到的是「补跑完成」，实际一个群都没重跑。
    #[test]
    fn picking_rooms_keeps_every_month_and_a_typo_fails_loudly() {
        let files = vec![
            file("wanted", "202608"),
            file("wanted", "202609"),
            file("other", "202609"),
        ];
        assert_eq!(
            pick(files.clone(), &[]).unwrap().len(),
            3,
            "不挑就该原样放行"
        );

        let got = pick(files.clone(), &["wanted".into()]).unwrap();
        assert_eq!(
            got.iter().map(|f| f.month.as_str()).collect::<Vec<_>>(),
            ["202608", "202609"],
            "挑中的群跨月两行都要留下"
        );

        let e = pick(files, &["wnated".into()]).unwrap_err();
        assert!(
            matches!(e, MirrorError::Round(_)),
            "roomid 拼错该整轮失败，不是静默跑 0 个群：{e}"
        );
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
