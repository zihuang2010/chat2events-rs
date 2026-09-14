//! 结果缓存 —— 内容寻址 SQLite，保留首次答案。**承重件，不是优化**（见模块头）。
//!
//! 确定性由这里保证，不由算法保证：`temperature = 0` 不保证同输入同输出，而非冻结区
//! 每天重写 `[T-3, T-2]`，同一批 event 会被反复打标。没有跨运行的持久答案，
//! 报表就会**抖动而非修正** —— 正是承重不变量 1 要防的那件事。
//!
//! 四件事压在 `open / get / commit` 三个函数底下：文件独占锁（另一个进程不能拿旧内存
//! 快照继续追加）· 旧 NDJSON 一次性导入 · 有界页缓存的按键读盘 · 写失败后拒绝继续追加。

use super::{
    model::validate,
    types::{Assignment, Labels},
};
use crate::BoxError;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read},
    path::Path,
};

/// 缓存键是 `sha256(summary)`，**不是 event_id**。
///
/// 用 event_id 会跟分片删重写直接冲突：重跑某个群某天，event 全删重建、id 全变，
/// 落盘的那批答案立刻变成没人认领的孤儿，而新 event 又没有标签。
///
/// 存 hash 不存原文的第二个理由是 PII：`summary` 里有客户姓名和地址
/// （脱敏明确不掩它们），而缓存**只增不减**——原文一旦进去就是永久的。
///
pub(super) fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

pub(super) fn hex(k: &[u8; 32]) -> String {
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

pub(super) struct Cache {
    db: rusqlite::Connection,
    file: File,
    known: BTreeSet<String>,
    write_failed: bool,
}

impl Drop for Cache {
    fn drop(&mut self) {
        // 显式解锁：并行测试 fork 的短暂描述符继承也不能延长本对象的锁生命周期。
        if let Err(error) = self.file.unlock() {
            tracing::warn!("释放分类缓存锁失败：{error}");
        }
    }
}

impl Cache {
    pub(super) fn open(path: &Path, known: &BTreeSet<String>) -> Result<Self, BoxError> {
        let started = std::time::Instant::now();
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        // 锁覆盖整个缓存生命周期；另一个进程不能拿旧内存快照继续追加答案。
        //
        // ⚠️ **拿不到锁最可能的原因是「上一轮还没跑完」**，而那一轮是被打标排空拖住的
        // （分类端点退化时一批最坏 `request_timeout_secs`）。这一路的后果最狠：
        // 本轮在碰任何群之前就 `Err` → **整轮零事实、零指标、零 `run_failure`**，
        // 库里对这一天一个字都没有。所以这条消息要**带上锁文件路径**，让人一眼看出
        // 是谁占着；`run_failure` 记不了（那张表是「群 × 运行」，整轮级失败没有群，
        // 塞个哨兵 roomid 进去会被 `read_filters` 和覆盖度当成真群看见）。
        // 整轮非零退出由调用方兜，cron 的告警接的是退出码。
        file.try_lock().map_err(|e| {
            tracing::error!(
                lock = %path.display(),
                "拿不到分类缓存的独占锁 —— 多半是上一轮还没跑完（打标排空被拖住）。\
                 本轮将在碰任何群之前失败，库里不会有这一天的任何记录"
            );
            format!(
                "无法独占分类缓存 {}，请确认没有其他跑批或重打标占用：{e}",
                path.display()
            )
        })?;
        let database = path.with_extension("sqlite");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        drop(options.open(&database)?);
        let mut db = rusqlite::Connection::open(&database)?;
        // 页缓存按 2 MiB 控制，禁用 mmap；历史答案留在磁盘，事务提交后才对调用方可见。
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-2048; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 {
            return Err("分类缓存格式版本高于当前程序，拒绝覆盖".into());
        }
        if version == 0 {
            // 旧 NDJSON 只导入一次，格式标记与答案同事务；中断后回滚，重试不会改变首个答案。
            let tx = db.transaction()?;
            tx.execute_batch(
                "CREATE TABLE answers (hash BLOB PRIMARY KEY, labels TEXT NOT NULL) WITHOUT ROWID;",
            )?;
            let mut reader = BufReader::new(&file);
            let mut line = Vec::new();
            let (mut bad, mut imported) = (0usize, 0usize);
            loop {
                line.clear();
                let n = (&mut reader)
                    .take(1024 * 1024)
                    .read_until(b'\n', &mut line)?;
                if n == 0 {
                    break;
                }
                if n == 1024 * 1024 {
                    return Err("旧分类缓存单行超过 1 MiB，拒绝无界加载".into());
                }
                // 不修改旧文件，保留升级前备份；未完成尾行不是已提交答案。
                if !line.ends_with(b"\n") {
                    bad += 1;
                    break;
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let entry = serde_json::from_slice::<Entry>(&line).ok().and_then(|e| {
                    let key = unhex(&e.h)?;
                    let labels = validate(
                        vec![Assignment {
                            index: 1,
                            type_ids: e.t,
                        }],
                        1,
                        known,
                    )
                    .ok()?
                    .pop()?;
                    Some((key, labels))
                });
                if let Some((key, labels)) = entry {
                    imported += tx.execute(
                        "INSERT OR IGNORE INTO answers (hash, labels) VALUES (?1, ?2)",
                        rusqlite::params![key.as_slice(), serde_json::to_string(labels.all())?],
                    )?;
                } else {
                    bad += 1;
                }
            }
            tx.execute_batch("PRAGMA user_version=1;")?;
            tx.commit()?;
            tracing::info!(
                imported,
                bad,
                "旧分类缓存导入完成；原文件保留，坏行与未完成尾行未导入"
            );
        }
        tracing::info!(
            ms = started.elapsed().as_millis(),
            path = %database.display(),
            "分类缓存已打开（按键读盘）"
        );
        Ok(Self {
            db,
            file,
            known: known.clone(),
            write_failed: false,
        })
    }

    pub(super) fn get(&self, key: &[u8; 32]) -> Result<Option<Labels>, BoxError> {
        let raw: Option<String> = self
            .db
            .query_row(
                "SELECT labels FROM answers WHERE hash=?1",
                [key.as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|raw| {
            validate(
                vec![Assignment {
                    index: 1,
                    type_ids: serde_json::from_str(&raw)?,
                }],
                1,
                &self.known,
            )
            .map_err(|_| BoxError::from("持久分类答案损坏或与词表不符，拒绝重新请求以免答案漂移"))?
            .pop()
            .ok_or_else(|| "持久分类答案为空".into())
        })
        .transpose()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.db
            .query_row("SELECT COUNT(*) FROM answers", [], |r| r.get::<_, i64>(0))
            .unwrap() as usize
    }

    /// 同一摘要采用第一个已提交的答案；每个模型批次只同步一次磁盘。
    pub(super) fn commit(
        &mut self,
        entries: Vec<([u8; 32], Labels)>,
    ) -> Result<Vec<Labels>, BoxError> {
        if self.write_failed {
            return Err("分类缓存此前写入失败，停止追加；请排查磁盘并重启本轮".into());
        }
        // 同批重复与跨批竞争都由唯一键决定；事务失败不返回未持久化答案。
        self.write_failed = true;
        let tx = self.db.transaction()?;
        let mut accepted = Vec::with_capacity(entries.len());
        for (key, labels) in entries {
            tx.execute(
                "INSERT OR IGNORE INTO answers (hash, labels) VALUES (?1, ?2)",
                rusqlite::params![key.as_slice(), serde_json::to_string(labels.all())?],
            )?;
            let raw: String = tx.query_row(
                "SELECT labels FROM answers WHERE hash=?1",
                [key.as_slice()],
                |r| r.get(0),
            )?;
            let got = validate(
                vec![Assignment {
                    index: 1,
                    type_ids: serde_json::from_str(&raw)?,
                }],
                1,
                &self.known,
            )
            .map_err(|_| "持久分类答案损坏，拒绝发布")?
            .pop()
            .expect("一个已校验答案");
            accepted.push(got);
        }
        tx.commit()?;
        self.write_failed = false;
        Ok(accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        model::BATCH,
        tests::{known, lab},
    };
    use super::*;

    #[test]
    fn cache_lock_rejects_a_second_process_and_releases_on_drop() {
        const ENV: &str = "C2E_CLASSIFY_LOCK_PROBE";
        if let Some(path) = std::env::var_os(ENV) {
            assert!(Cache::open(Path::new(&path), &known()).is_err());
            return;
        }
        let dir = crate::testutil::fresh_root("classify", "lock");
        let path = dir.join("v1.ndjson");
        let cache = Cache::open(&path, &known()).unwrap();
        assert!(Cache::open(&path, &known()).is_err());
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "classify::cache::tests::cache_lock_rejects_a_second_process_and_releases_on_drop",
            ])
            .env(ENV, &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        drop(cache);
        assert!(Cache::open(&path, &known()).is_ok());
    }

    #[test]
    fn cache_write_failure_does_not_publish_answers_or_continue_appending() {
        let dir = crate::testutil::fresh_root("classify", "write-failure");
        let path = dir.join("v1.ndjson");
        let mut cache = Cache::open(&path, &known()).unwrap();
        cache.db.execute_batch("PRAGMA query_only=ON;").unwrap(); // 只读连接确定性制造事务写入错误。
        assert!(cache.commit(vec![(digest("a"), lab(&["a"]))]).is_err());
        assert_eq!(cache.len(), 0);
        assert!(
            cache
                .commit(vec![(digest("b"), lab(&["b"]))])
                .unwrap_err()
                .to_string()
                .contains("此前写入失败")
        );
    }

    #[test]
    fn legacy_answers_migrate_once_and_keep_the_first_answer() {
        let dir = crate::testutil::fresh_root("classify", "migration");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.ndjson");
        let key = digest("旧答案");
        let original = format!(
            "{}\n坏行\n{}\n半截",
            serde_json::to_string(&Entry {
                h: hex(&key),
                t: vec!["a".into()]
            })
            .unwrap(),
            serde_json::to_string(&Entry {
                h: hex(&key),
                t: vec!["b".into()]
            })
            .unwrap()
        );
        std::fs::write(&path, &original).unwrap();
        let mut cache = Cache::open(&path, &known()).unwrap();
        assert_eq!(cache.get(&key).unwrap(), Some(lab(&["a"])));
        assert_eq!(
            cache.commit(vec![(key, lab(&["b"]))]).unwrap(),
            [lab(&["a"])]
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "升级保留原缓存备份"
        );
        drop(cache);
        // 导入标记已提交，重启不再扫描旧文件，也不会重新解释已持久化答案。
        std::fs::write(&path, "已经归档").unwrap();
        assert_eq!(
            Cache::open(&path, &known()).unwrap().get(&key).unwrap(),
            Some(lab(&["a"]))
        );
    }

    #[test]
    fn disk_cache_uses_indexed_lookup_with_a_bounded_page_cache() {
        let count: usize = std::env::var("CHAT2EVENTS_CACHE_TEST_ENTRIES")
            .ok()
            .map(|v| v.parse().unwrap())
            .unwrap_or(2000);
        let dir = crate::testutil::fresh_root("classify", "capacity");
        let path = dir.join("v1.ndjson");
        let mut cache = Cache::open(&path, &known()).unwrap();
        for first in (0..count).step_by(BATCH) {
            cache
                .commit(
                    (first..(first + BATCH).min(count))
                        .map(|i| (digest(&i.to_string()), lab(&["a"])))
                        .collect(),
                )
                .unwrap();
        }
        drop(cache);
        let start = std::time::Instant::now();
        let cache = Cache::open(&path, &known()).unwrap();
        assert_eq!(cache.len(), count);
        for i in [0, count / 2, count.saturating_sub(1)] {
            assert_eq!(
                cache.get(&digest(&i.to_string())).unwrap(),
                Some(lab(&["a"]))
            );
        }
        let plan: String = cache
            .db
            .query_row(
                "EXPLAIN QUERY PLAN SELECT labels FROM answers WHERE hash=?1",
                [digest("0").as_slice()],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("SEARCH") && plan.contains("PRIMARY KEY"),
            "{plan}"
        );
        assert_eq!(
            cache
                .db
                .query_row("PRAGMA cache_size", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            -2048
        );
        eprintln!(
            "cache entries={count}, reopen_and_lookup_ms={}",
            start.elapsed().as_millis()
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
        let mut c = Cache::open(&p, &known()).unwrap();
        c.commit(vec![(k1, lab(&["a", "b"]))]).unwrap();
        drop(c);

        std::fs::write(
            &p,
            format!("{}{{半截\n", std::fs::read_to_string(&p).unwrap()),
        )
        .unwrap();

        let c = Cache::open(&p, &known()).unwrap();
        // 缓存存的是**全集**不是主类 —— 只存主类的话副类每次都要重问，
        // 「同 summary 同答案」就只保住了一半。
        assert_eq!(c.get(&k1).unwrap(), Some(lab(&["a", "b"])));
        assert_eq!(c.len(), 1, "坏行不该变成一条答案");
    }

    #[test]
    fn hex_round_trips() {
        let k = digest("x");
        assert_eq!(unhex(&hex(&k)), Some(k));
        assert_eq!(unhex("nothex"), None);
    }

    #[test]
    fn a_second_answer_cannot_replace_the_first_cached_answer() {
        let dir = crate::testutil::fresh_root("classify", "first-answer");
        let mut c = Cache::open(&dir.join("v1.ndjson"), &known()).unwrap();
        let key = digest("同一摘要");
        c.commit(vec![(key, lab(&["a"]))]).unwrap();
        c.commit(vec![(key, lab(&["b"]))]).unwrap();
        assert_eq!(c.get(&key).unwrap(), Some(lab(&["a"])));
    }

    #[test]
    fn an_unterminated_cache_tail_does_not_swallow_the_next_answer() {
        let dir = crate::testutil::fresh_root("classify", "tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.ndjson");
        std::fs::write(&path, b"{\"h\":\"unfinished").unwrap();
        let key = digest("新摘要");
        let mut c = Cache::open(&path, &known()).unwrap();
        c.commit(vec![(key, lab(&["a"]))]).unwrap();
        drop(c);
        let reopened = Cache::open(&path, &known()).unwrap();
        assert_eq!(reopened.get(&key).unwrap(), Some(lab(&["a"])));
    }
}
