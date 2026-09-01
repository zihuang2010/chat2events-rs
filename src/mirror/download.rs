//! 一个月文件怎么下 —— **ADR-0005 那三条承重规则就在这个文件里**：
//! 严格只读到 `ndjson_position` 为止 · 本地字节数必须等于它 · 本地更长就作废重拉。
//!
//! 瞬时失败共尝试 [`ATTEMPTS`] 次；**三道校验一次都不重**，那是旧副本，再要还是它。
//! [`MirrorError::Transient`] **不出这个文件** —— 次数用完就降级成群级失败。
//!
//! 路径布局不在这里：写文件问 [`crate::ingest::room_path`] 要路径，跟读的是同一个函数。

use super::{
    error::{MirrorError, Result},
    index::MonthFile,
};
use crate::ingest;
use std::{fs, io::Write, path::Path, time::Duration};

/// 单个月文件的下载超时。月末一个群 18 MB，两分钟绰绰有余。
/// ⚠️ 这是**每次尝试**的上限，不是这个文件的总耗时 —— 乘 [`ATTEMPTS`] 才是。
pub(super) const TIMEOUT: Duration = Duration::from_secs(120);

/// 只管 TCP+TLS 握手。单独设是为了让「OSS 连不上」十秒内失败，而不是每个文件都
/// 耗满上面那个按分钟计的整体超时 —— 1000 个群时这是「几分钟」和「几小时」之差。
/// 跟 `llm.rs` 同一个理由（见 `config.rs` 的 `connect_timeout_secs`）。
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 单个月文件的总尝试次数（含第一次）。1000 个群跑一轮就是 1000 次 GET，
/// CDN 抖动 0.5% 就是每天 5 个群从指标里静默消失。
const ATTEMPTS: u32 = 3;

/// 首次重试前等多久，之后翻倍（1s → 2s）。
/// ponytail: 不加抖动。在飞的只有 `mirror_concurrency` 个（8），凑不出惊群。
const BACKOFF: Duration = Duration::from_secs(1);

/// 一个月文件这一趟发生了什么。
pub(super) enum Outcome {
    /// 上游登记了但还没有数据
    Empty,
    /// 本地已经等于 `ndjson_position`，零字节下载
    Skip,
    /// 追加了这么多字节
    Pulled(u64),
}

/// 非 2xx 的响应算哪一类失败。
///
/// **此前根本没有这个检查** —— `send()` 不会因为 4xx/5xx 报错，于是一个 503 的
/// HTML 错误页会一路流进 [`write_and_verify`]，撞在字节数那道校验上，报出
/// 「多半是命中了陈旧缓存」。诊断指向完全错误的方向，还不会被重试。
fn http_status_error(status: reqwest::StatusCode, url: &str) -> MirrorError {
    let msg = format!("HTTP {status}：{url}");
    // 5xx 是对端的事，429/408 是它让我们等会儿再来 —— 都能靠重试救。
    // 其余 4xx（404 对象不存在、403 签名过期、416 上游 position 跑到了对象末尾之后）
    // 重试多少次都是同一个答案，直接判这个群本轮失败。
    if status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
    {
        MirrorError::Transient(msg)
    } else {
        MirrorError::Room(msg)
    }
}

/// 瞬时失败重试。**只有 [`MirrorError::Transient`] 会重来** —— 三道校验失败一次都不重，
/// 那是 CDN 给了旧副本，再要一次还是同一份（ADR-0005 结尾）。
///
/// 重来时 [`download_one`] 会重新读一次本地文件大小，所以「上一次写到哪」不需要在这里
/// 传递 —— 本地是字节级镜像，那个状态本来就存在磁盘上。
///
/// 单个文件最坏耗时 = 3 × [`TIMEOUT`] + 3s 退避 ≈ 6 分钟。**整轮的上界不在这里** ——
/// 由 `daily::run` 那个 deadline 兜（`round_deadline_secs`），到点就不再开新的下载。
pub(super) async fn download_with_retry(
    http: &reqwest::Client,
    base: &str,
    raw_root: &Path,
    f: &MonthFile,
) -> Result<Outcome> {
    let (mut attempt, mut delay) = (1u32, BACKOFF);
    loop {
        match download_one(http, base, raw_root, f).await {
            Err(MirrorError::Transient(m)) if attempt < ATTEMPTS => {
                // 静默重试等于不知道 CDN 在抖。这条 warn 是唯一的信号。
                tracing::warn!(
                    room = %f.room, month = %f.month, attempt,
                    "瞬时失败，{delay:?} 后重试：{m}"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                delay *= 2;
            }
            // 次数用完 → 降级成群级失败。**`Transient` 不出这个函数。**
            Err(MirrorError::Transient(m)) => {
                return Err(MirrorError::Room(format!("尝试 {ATTEMPTS} 次仍失败：{m}")));
            }
            r => return r,
        }
    }
}

async fn download_one(
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
    if !status.is_success() {
        return Err(http_status_error(status, &url));
    }
    // ⚠️ 整包进内存。峰值 = `mirror_concurrency` × 最大月文件（月末 8 × 18 MB ≈ 144 MB）——
    //    **那个旋钮同时是内存旋钮**，往上调之前先乘一遍。
    // ponytail: 不改成流式写盘。承重规则 1 要求「校验没过一个字节都不落地」，
    //           流式就得先写临时文件再改名，为 144 MB 换一套两阶段提交不划算。
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
    let mut fh = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(local)?;
    fh.write_all(body)?;
    fh.sync_all()?;

    if have == 0 {
        // 冷启动才数行：整文件数一遍，换一次独立于字节数的验证。
        // ponytail: 增量时只校验字节数（position 已是精确的端到端证明）。要每次都
        //           校验就得读回整个月文件，月末 1000 群 = 多读 18 GB。
        let n = fs::read(local)?.iter().filter(|b| **b == b'\n').count() as u64;
        if n != record_count {
            fs::remove_file(local)?;
            return Err(format!("落地 {n} 行，索引表说 {record_count} 行，已删除重来").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        ingest::room_path(&testutil::fresh_root("mirror", name), "202608", "C", "R")
    }

    const A: &[u8] = b"{\"a\":1}\n"; // 8 字节
    const B: &[u8] = b"{\"b\":2}\n"; // 8 字节

    /// 分类错一边的代价不对称：把 503 判成 `Room`，这个群白白丢一天；把 404 判成
    /// `Transient`，白等 3 秒再丢。所以这条边界值得钉死。
    #[test]
    fn transient_statuses_retry_and_the_rest_do_not() {
        use reqwest::StatusCode as S;
        for s in [
            S::INTERNAL_SERVER_ERROR,
            S::BAD_GATEWAY,
            S::SERVICE_UNAVAILABLE,
            S::GATEWAY_TIMEOUT,
            S::TOO_MANY_REQUESTS,
            S::REQUEST_TIMEOUT,
        ] {
            let e = http_status_error(s, "u");
            assert!(matches!(e, MirrorError::Transient(_)), "{s} 该重试：{e}");
        }
        // 416 = 上游 position 跑到了对象末尾之后，重试不会让 OSS 长出字节来
        for s in [
            S::NOT_FOUND,
            S::FORBIDDEN,
            S::RANGE_NOT_SATISFIABLE,
            S::BAD_REQUEST,
        ] {
            let e = http_status_error(s, "u");
            assert!(matches!(e, MirrorError::Room(_)), "{s} 不该重试：{e}");
        }
    }

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
        assert!(matches!(e, MirrorError::Room(_)), "{e}");
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
        assert!(matches!(e, MirrorError::Room(_)), "{e}");
        assert!(e.to_string().contains("行边界"), "{e}");
        assert!(!p.exists());
    }

    #[test]
    fn record_count_mismatch_deletes_the_file() {
        // 字节数对、行边界对，但索引表说该有 2 行 —— 独立于字节数的那道校验
        let p = tmp("count");
        let e = write_and_verify(&p, A, 0, 8, 2).unwrap_err();
        assert!(matches!(e, MirrorError::Room(_)), "{e}");
        assert!(e.to_string().contains("已删除重来"), "{e}");
        assert!(
            !p.exists(),
            "行数不对的文件必须删掉，否则下次 have 就是错的"
        );
    }
}
