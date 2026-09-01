//! ① 摄取 · 拉取 —— 索引表驱动的增量同步（**ADR-0005**）。
//!
//! ```text
//! MySQL b_wecom_group_message_month_file          ← 谁、在哪、到第几字节
//!           │ ndjson_object_key
//!           ▼
//! GET <download_base_url>/<object_key>            Range: bytes=<本地已有>-
//!           │ 只取到 ndjson_position 为止
//!           ▼
//! <raw_root>/<yyyyMM>/<corpId>/<roomId>.ndjson    追加，OSS 的字节级镜像
//! ```
//!
//! **为什么不是 `ossutil sync`**：OSS 上是按月的 AppendObject，每天往里追加。
//! sync 看到文件变了就整个重下 —— 月末一个群 18 MB、1000 个群 18 GB，而当天真正
//! 新增的只有 600 MB。索引表把「新的字节从第几位开始」直接告诉了我们。
//!
//! **三条承重规则**：
//!
//! 1. **严格只读到 `ndjson_position` 为止，一个字节都不多读。** 超过它的字节可能是
//!    一次在飞的 AppendObject，末尾会是半行 JSON。这条从结构上消灭了「截断坏行」
//!    那一整类失败，比事后检测干净得多。
//! 2. **本地字节数必须等于 `ndjson_position`。** 端到端的完整性证明，同时是 CDN
//!    陈旧缓存的唯一防线 —— 那条路径配了 30 天缓存，实测 `Cache-Control: no-cache`
//!    和随机 query 参数都绕不掉（ADR-0005 结尾）。
//! 3. **本地比上游还长 → 本地作废重拉。** 追加写不该出现这种事（上游删档重建才会），
//!    但不处理就会每天校验失败、这个群永远跑不了。
//!
//! 失败按**群**隔离（承重不变量 3），与抽取失败同一条路径；**索引表本身查不到是
//! 整轮失败** —— 那不是某个群的事。两种处置在类型上分开（[`PullError`]），
//! 跟 `ingest` 的 [`crate::ingest::IngestError`] 一个规矩。
//!
//! **路径布局不在这里** —— 写文件问 [`ingest::room_path`] 要路径，跟读的是同一个
//! 函数。上游字段名只出现在 [`MonthFile`] 和下面那条 `index_sql!` 里，跟 `ingest.rs`
//! 一个规矩。

use crate::{config::Config, ingest, window::Window};
use sqlx::{MySqlPool, Row};
use std::{collections::BTreeSet, fmt, fs, io::Write, path::Path, time::Duration};

/// 两种失败的**处置方式不同**，所以必须在类型上分开（跟 `IngestError` 同一规矩）。
/// 曾经这条分界靠「错误出现的位置」表达（走 `?` = 整轮死、进 done 向量 = 群级），
/// 约定完全隐式 —— 新增失败路径没有任何东西提醒你选对通道。
#[derive(Debug)]
pub enum PullError {
    /// 索引表查不到 / HTTP 客户端起不来 —— 不是某个群的事，整轮失败。
    Round(String),
    /// 某群某月拉取或校验失败 —— 该群本轮不参与跑批（承重不变量 3）。
    Room(String),
}

impl fmt::Display for PullError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Round(m) | Self::Room(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for PullError {}

/// 索引表连不上/查不动/行的形状不对 —— 上游侧的整轮问题。
impl From<sqlx::Error> for PullError {
    fn from(e: sqlx::Error) -> Self {
        Self::Round(format!("索引表：{e}"))
    }
}

/// 下载中的 HTTP 错误发生在单个月文件上 —— 群级。
impl From<reqwest::Error> for PullError {
    fn from(e: reqwest::Error) -> Self {
        Self::Room(format!("下载：{e}"))
    }
}

/// 本地读写错误发生在单个月文件上 —— 群级。
impl From<std::io::Error> for PullError {
    fn from(e: std::io::Error) -> Self {
        Self::Room(format!("本地文件：{e}"))
    }
}

/// 校验类失败的文案（字节数 / 行边界 / 记录数）—— 全部是群级。
impl From<String> for PullError {
    fn from(m: String) -> Self {
        Self::Room(m)
    }
}

type Result<T> = std::result::Result<T, PullError>;

/// 单个月文件的下载超时。月末一个群 18 MB，两分钟绰绰有余。
const TIMEOUT: Duration = Duration::from_secs(120);

/// 索引表的一行 = 一个群一个月的月文件。
#[derive(Debug, Clone)]
struct MonthFile {
    corp: String,
    room: String,
    month: String,
    object_key: String,
    /// 下一批 AppendObject 预期字节位置 = 已确认的文件末尾
    position: u64,
    /// 已确认追加记录数 = 免费的、独立于字节数的完整性校验
    record_count: u64,
}

/// 索引表列名 —— `index_sql!` 的 SELECT 列表和取值点引用**同一组常量**，
/// 打错一个字母是编译错误（跟 `ingest.rs` 的 COL_* 同一个理由）。
const COL_CORP_ID: &str = "corp_id";
const COL_ROOM_ID: &str = "official_room_id";
const COL_FILE_MONTH: &str = "file_month";
const COL_OBJECT_KEY: &str = "ndjson_object_key";
const COL_POSITION: &str = "ndjson_position";
const COL_RECORD_COUNT: &str = "ndjson_record_count";

/// 上游字段名只允许出现在这里。
///
/// **为什么是 `macro_rules!` 而不是 `const`**：跟 `ingest.rs` 的 `select_sql!`
/// 同一个理由，见那边的注释 —— `format!` 在编译期校验占位符。
macro_rules! index_sql {
    () => {
        r#"
SELECT {corp_id}, {room_id}, {file_month}, {object_key},
       {position}, {record_count}
FROM b_wecom_group_message_month_file
WHERE is_deleted = 0 AND {file_month} IN ({months})
ORDER BY {corp_id}, {room_id}, {file_month}
"#
    };
}

/// 窗口覆盖的月份里登记的全部月文件。跨月时同一个群会有两行。
///
/// 月份是我们自己 `%Y%m` 格式化出来的，仍然走 `?` 绑定 —— 没有理由为「反正拼不坏」
/// 破一次例。`is_deleted` 是上游的软删除/冻结标记，1 = 不该再读。
async fn list_month_files(pool: &MySqlPool, w: &Window) -> Result<Vec<MonthFile>> {
    let months = ingest::months(w);
    let sql = format!(
        index_sql!(),
        months = vec!["?"; months.len()].join(", "),
        corp_id = COL_CORP_ID,
        room_id = COL_ROOM_ID,
        file_month = COL_FILE_MONTH,
        object_key = COL_OBJECT_KEY,
        position = COL_POSITION,
        record_count = COL_RECORD_COUNT,
    );

    // sqlx 0.9 拦住一切非 `&'static str` 的 SQL，要显式声明审计过。
    // **这里唯一动态的东西是占位符本身**（`?, ?`），条数由 `months.len()` 决定，
    // 月份的值走下面的 `bind` —— 没有任何外部数据参与拼接。
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for m in &months {
        q = q.bind(m);
    }
    q.fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            Ok(MonthFile {
                corp: r.try_get(COL_CORP_ID)?,
                room: r.try_get(COL_ROOM_ID)?,
                month: r.try_get(COL_FILE_MONTH)?,
                object_key: r.try_get(COL_OBJECT_KEY)?,
                position: r.try_get(COL_POSITION)?,
                record_count: r.try_get(COL_RECORD_COUNT)?,
            })
        })
        .collect()
}

/// 一个月文件这一趟发生了什么。
enum Outcome {
    /// 上游登记了但还没有数据
    Empty,
    /// 本地已经等于 `ndjson_position`，零字节下载
    Skip,
    /// 追加了这么多字节
    Pulled(u64),
}

async fn pull_one(
    http: &reqwest::Client,
    base: &str,
    raw_root: &Path,
    f: &MonthFile,
) -> Result<Outcome> {
    let local = ingest::room_path(raw_root, &f.month, &f.corp, &f.room);
    // 「已经拉到第几字节」= 文件大小。本地是字节级镜像，所以不需要任何额外状态存储。
    let mut have = fs::metadata(&local).map(|m| m.len()).unwrap_or(0);

    if f.position == 0 {
        return Ok(Outcome::Empty);
    }
    if have == f.position {
        return Ok(Outcome::Skip);
    }
    if have > f.position {
        // 承重规则 3。上游删档重建才会这样，但不处理这个群就永远卡死。
        tracing::warn!(
            room = %f.room,
            have,
            upstream = f.position,
            "本地比上游长，作废重拉"
        );
        fs::remove_file(&local)?;
        have = 0;
    }

    let url = format!("{}/{}", base.trim_end_matches('/'), f.object_key);
    // `have == 0` 时 `bytes=0-` 等价于全量下载 —— 冷启动和增量走同一条路径，
    // 不写两条分支。
    let resp = http
        .get(&url)
        .header(reqwest::header::RANGE, format!("bytes={have}-"))
        .send()
        .await?;
    let status = resp.status();
    let mut body = resp.bytes().await?;

    // 有些 CDN 会忽略 Range 直接返 200 整个对象 —— 那就自己切掉已有的部分。
    if status == reqwest::StatusCode::OK && have > 0 {
        if (body.len() as u64) < have {
            return Err(format!(
                "忽略 Range 返回了 200，但整个对象只有 {} 字节，比本地已有的 {have} 还短",
                body.len()
            )
            .into());
        }
        body = body.slice(have as usize..);
    }

    write_and_verify(&local, &body, have, f.position, f.record_count)?;
    Ok(Outcome::Pulled(f.position - have))
}

/// 三道校验，然后追加落盘。**承重规则 1、2 就在这里** —— 不修补、不截断、不落一半。
///
/// 分出来是为了让测试不打网络就覆盖到它：字节数、行边界、记录数三道校验和
/// 「冷启动 / 追加」两条路径，都是能静默写坏数据的地方。
fn write_and_verify(
    local: &Path,
    body: &[u8],
    have: u64,
    position: u64,
    record_count: u64,
) -> Result<()> {
    let want = position - have;
    if body.len() as u64 != want {
        return Err(format!(
            "CDN 只给了 {} 字节，期望 {want}（本地 {have} → 上游 {position}）。\
             多半是命中了陈旧缓存，见 ADR-0005 结尾",
            body.len()
        )
        .into());
    }
    // 只读到 position 为止，所以这一段必然以完整行开始和结束。不成立说明上游的
    // position 不在行边界上 —— 那是比少几个字节严重得多的事。
    if !body.starts_with(b"{") || !body.ends_with(b"\n") {
        return Err(format!(
            "取回的字节不是完整的 NDJSON 行（首 {:?} 末 {:?}），\
             上游 ndjson_position 可能不在行边界上",
            body.first(),
            body.last()
        )
        .into());
    }

    if let Some(dir) = local.parent() {
        fs::create_dir_all(dir)?;
    }
    // 追加 + fsync。**这里不 spawn_blocking**：拉取是跑批的第一步，跑完才进抽取，
    // 此刻 runtime 上没有在飞的模型调用会被卡住 —— 跟硬规则点名的 `read_room`
    // （DuckDB，与 N 个模型调用同时在飞）不是一回事。
    let mut fh = fs::OpenOptions::new().create(true).append(true).open(local)?;
    fh.write_all(body)?;
    fh.sync_all()?;

    if have == 0 {
        // 冷启动才数行：整文件数一遍，换一次独立于字节数的验证。
        // ponytail: 增量时只校验字节数（position 已是精确的端到端证明）。要每次都
        //           校验就得读回整个月文件，月末 1000 群 = 多读 18 GB。
        let n = fs::read(local)?.iter().filter(|b| **b == b'\n').count() as u64;
        if n != record_count {
            fs::remove_file(local)?;
            return Err(
                format!("落地 {n} 行，索引表说 {record_count} 行，已删除重来").into(),
            );
        }
    }
    Ok(())
}

/// 拉取窗口覆盖的全部月文件。**失败不中断整轮**，返回本轮不该参与跑批的群。
///
/// 返回 `(corp, room)`：一个群跨月有两行，**任一行失败整个群就作废** ——
/// 承重不变量 3，少一个窗口就重写等于用残缺数据覆盖完整数据。
pub async fn pull(
    cfg: &Config,
    pool: &MySqlPool,
    w: &Window,
) -> Result<BTreeSet<(String, String)>> {
    let files = list_month_files(pool, w).await?;
    tracing::info!(
        months = %ingest::months(w).join(","),
        files = files.len(),
        "索引表"
    );

    let http = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| PullError::Round(format!("HTTP 客户端构建失败：{e}")))?;
    let mut set = tokio::task::JoinSet::new();
    let mut done: Vec<(MonthFile, Result<Outcome>)> = Vec::with_capacity(files.len());

    for f in files {
        // 背压：在飞的最多 pull_concurrency 个。JoinSet 自己就够了，不用再加信号量。
        if set.len() >= cfg.ingest.pull_concurrency {
            done.push(join(set.join_next().await));
        }
        let http = http.clone();
        let base = cfg.ingest.download_base_url.clone();
        let root = cfg.ingest.raw_root.clone();
        set.spawn(async move {
            let r = pull_one(&http, &base, &root, &f).await;
            (f, r)
        });
    }
    while let Some(j) = set.join_next().await {
        done.push(join(Some(j)));
    }

    let mut failed = BTreeSet::new();
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
    tracing::info!(
        pulled,
        skipped,
        empty,
        failed = failed.len(),
        bytes,
        "拉取完成"
    );
    Ok(failed)
}

/// 任务本身不会 panic（每条失败路径都是 `Err`），所以 `JoinError` 只可能是 bug ——
/// 这是「构造已经保证、不可能为假」那一档，按硬规则用 `expect`。
fn join(
    j: Option<std::result::Result<(MonthFile, Result<Outcome>), tokio::task::JoinError>>,
) -> (MonthFile, Result<Outcome>) {
    j.expect("set.len() > 0 时必有一个可 join")
        .expect("拉取任务的每条失败路径都返回 Err，不 panic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        ingest::room_path(&testutil::fresh_root("pull", name), "202608", "C", "R")
    }

    const A: &[u8] = b"{\"a\":1}\n"; // 8 字节
    const B: &[u8] = b"{\"b\":2}\n"; // 8 字节

    #[test]
    fn cold_start_writes_every_byte() {
        let p = tmp("cold");
        write_and_verify(&p, A, 0, 8, 1).unwrap();
        assert_eq!(fs::read(&p).unwrap(), A);
    }

    #[test]
    fn append_extends_the_existing_file() {
        let p = tmp("append");
        write_and_verify(&p, A, 0, 8, 1).unwrap();
        // 增量：本地已有 8 字节、上游到 16，只补第二行。record_count 故意给个荒唐值 ——
        // have != 0 时不该再数行（数一遍要读回整个月文件）。
        write_and_verify(&p, B, 8, 16, 999).unwrap();
        assert_eq!(fs::read(&p).unwrap(), [A, B].concat());
    }

    #[test]
    fn short_body_is_a_room_failure_and_writes_nothing() {
        // 承重规则 2：这就是 CDN 陈旧缓存唯一的防线
        let p = tmp("short");
        let e = write_and_verify(&p, A, 0, 99, 1).unwrap_err();
        assert!(matches!(e, PullError::Room(_)), "{e}");
        assert!(e.to_string().contains("陈旧缓存"), "{e}");
        assert!(!p.exists(), "校验没过就一个字节都不该落地");
    }

    #[test]
    fn body_not_on_a_line_boundary_is_rejected() {
        // 上游 position 不在行边界上 —— 比少几个字节严重得多，不能让它变成
        // 下游一次 JSON 解析失败
        let p = tmp("boundary");
        let half = b"{\"a\":1}"; // 没有结尾换行
        let e = write_and_verify(&p, half, 0, half.len() as u64, 1).unwrap_err();
        assert!(matches!(e, PullError::Room(_)), "{e}");
        assert!(e.to_string().contains("行边界"), "{e}");
        assert!(!p.exists());
    }

    #[test]
    fn record_count_mismatch_deletes_the_file() {
        // 字节数对、行边界对，但索引表说该有 2 行 —— 独立于字节数的那道校验
        let p = tmp("count");
        let e = write_and_verify(&p, A, 0, 8, 2).unwrap_err();
        assert!(matches!(e, PullError::Room(_)), "{e}");
        assert!(e.to_string().contains("已删除重来"), "{e}");
        assert!(!p.exists(), "行数不对的文件必须删掉，否则下次 have 就是错的");
    }
}
