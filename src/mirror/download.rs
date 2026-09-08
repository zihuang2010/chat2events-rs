//! 一个月文件怎么下 —— **三条承重规则就在这个文件里**：
//! 严格只读到 `ndjson_position` 为止 · 本地字节数必须等于它 · 本地更长就作废重拉。
//!
//! 瞬时失败共尝试 [`ATTEMPTS`] 次；**三道校验一次都不重**，对象与索引不一致需排查上游。
//! [`MirrorError::Transient`] **不出这个文件** —— 次数用完就降级成群级失败。
//!
//! 路径布局不在这里：写文件问 [`crate::ingest::room_path`] 要路径，跟读的是同一个函数。

use super::{
    error::{MirrorError, Result},
    index::MonthFile,
    oss::OssClient,
};
use crate::ingest;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::Duration,
};

/// 单个月文件的下载超时。月末一个群 18 MB，两分钟绰绰有余。
/// ⚠️ 这是**每次尝试**的上限，不是这个文件的总耗时 —— 乘 [`ATTEMPTS`] 才是。
pub(super) const TIMEOUT: Duration = Duration::from_secs(120);

/// 只管 TCP+TLS 握手。单独设是为了让「OSS 连不上」十秒内失败，而不是每个文件都
/// 耗满上面那个按分钟计的整体超时 —— 1000 个群时这是「几分钟」和「几小时」之差。
/// 跟 `llm.rs` 同一个理由（见 `config.rs` 的 `connect_timeout_secs`）。
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 单个月文件的总尝试次数（含第一次）。1000 个群跑一轮就是 1000 次 GET，
/// 下载抖动 0.5% 就是每天 5 个群从指标里静默消失。
const ATTEMPTS: u32 = 3;

/// 首次重试前等多久，之后翻倍（1s → 2s）。
/// ponytail: 不加抖动。在飞的只有 `mirror_concurrency` 个（8），凑不出惊群。
const BACKOFF: Duration = Duration::from_secs(1);

/// 每个在飞下载最多保留 64 MiB，8 路的正文上限为 512 MiB；超限显式失败，不分配整包。
/// 更大的合法月文件应改为固定缓冲临时落盘，不能仅抬高并发绕过容量验收。
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// 一个月文件这一趟发生了什么。
pub(super) enum Outcome {
    /// 索引确认长度为零，本地旧数据已清理
    Empty,
    /// 本地已经等于 `ndjson_position`，零字节下载
    Skip,
    /// 追加了这么多字节
    Pulled(u64),
}

/// 非 206 的响应算哪一类失败。
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
/// 对象与索引不一致不当成网络抖动。
///
/// 重来时 [`download_one`] 会重新读一次本地文件大小，所以「上一次写到哪」不需要在这里
/// 传递 —— 本地是字节级镜像，那个状态本来就存在磁盘上。
///
/// 单个文件最坏耗时 = 3 × [`TIMEOUT`] + 3s 退避 ≈ 6 分钟。**整轮的上界不在这里** ——
/// 由 `daily::run` 那个 deadline 兜（`round_deadline_secs`），到点就不再开新的下载。
pub(super) async fn download_with_retry(
    oss: &OssClient,
    raw_root: &Path,
    f: &MonthFile,
) -> Result<Outcome> {
    let (mut attempt, mut delay) = (1u32, BACKOFF);
    loop {
        match download_one(oss, raw_root, f).await {
            Err(MirrorError::Transient(m)) if attempt < ATTEMPTS => {
                // 静默重试等于不知道下载在抖。这条 warn 是唯一的信号。
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

async fn download_one(oss: &OssClient, raw_root: &Path, f: &MonthFile) -> Result<Outcome> {
    let local = ingest::room_path(raw_root, &f.month, &f.corp, &f.room);
    let (path, position) = (local.clone(), f.position);
    let have = tokio::task::spawn_blocking(move || resumable_len(&path, position))
        .await
        .expect("本地副本检查通过 Result 返回")?;
    if f.position == 0 {
        return Ok(Outcome::Empty);
    }
    if have == f.position {
        return Ok(Outcome::Skip);
    }
    if f.position - have > MAX_DOWNLOAD_BYTES {
        return Err(MirrorError::Room(
            "下载增量超过 64 MiB 内存预算，请改用流式落盘处理大文件".into(),
        ));
    }

    // 冷启动和续传共用签名 GET，严格止于索引已确认的字节位置。
    let request = oss.request(&f.object_key, have, f.position - 1).await?;
    let url = request.url().to_string();
    let mut resp = oss.http.execute(request).await?;
    let status = resp.status();
    // 直连 OSS 必须兑现 Range；拒绝 200 整包，避免重新引入旧 CDN 的切片兼容。
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(http_status_error(status, &url));
    }
    // 字节数相等不能证明偏移正确，必须先验证返回区间才能追加。
    // 对象可能已继续增长，total 允许大于索引快照，但起止位置必须完全一致。
    let expected_range = format!("bytes {have}-{}", f.position - 1);
    let valid_range = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok())
        .and_then(|range| range.split_once('/'))
        .is_some_and(|(range, total)| {
            range == expected_range && total.parse::<u64>().is_ok_and(|n| n >= f.position)
        });
    if !valid_range {
        return Err(format!("OSS Content-Range 与请求范围不一致，期望 {expected_range}").into());
    }
    let expected = (f.position - have) as usize;
    // 在扩展缓冲前检查真实接收量，Content-Range 正确也不能信任响应正文长度。
    let mut body = Vec::new();
    body.try_reserve_exact(expected)
        .map_err(|_| MirrorError::Room("下载缓冲分配失败".into()))?;
    while let Some(chunk) = resp.chunk().await? {
        if chunk.len() > expected - body.len() {
            return Err(MirrorError::Room(
                "OSS 响应正文超过索引确认的字节范围".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }

    // 校验与 fsync 在线程池里完成，避免卡住同一 runtime 线程上的其他下载。
    let (position, record_count) = (f.position, f.record_count);
    tokio::task::spawn_blocking(move || {
        write_and_verify(&local, &body, have, position, record_count)
    })
    .await
    .expect("文件校验与写入错误通过 Result 返回")?;
    Ok(Outcome::Pulled(f.position - have))
}

/// 文件长度只有在完整行末尾才是续传位置；崩溃留下的半行必须作废重拉。
fn resumable_len(local: &Path, position: u64) -> Result<u64> {
    let mut file = match fs::File::open(local) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let have = file.metadata()?.len();
    let mut valid = have <= position;
    if valid && have > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut tail = [0];
        file.read_exact(&mut tail)?;
        valid = tail[0] == b'\n';
    }
    if valid {
        return Ok(have);
    }
    drop(file);
    tracing::warn!(path = %local.display(), have, position, "本地副本过长或末行不完整，作废重拉");
    fs::remove_file(local)?;
    Ok(0)
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
            "OSS 返回了 {} 字节，期望 {want}（本地 {have} → 上游 {position}）。\
             对象与索引位置不一致",
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

    // raw 区是**未脱敏的客户正文**（实测 1850 条里 193 条带手机号、88 条带门牌号级
    // 住址、101 处真实姓名），保留两个月约 36 GB。跟 `secrets.toml` 一个待遇：
    // **只有属主能读**，不指望部署时的 umask —— 默认给出的是 0755 / 0644，
    // 也就是整个跑批机上任何账号都能翻客户资料。
    // `mirror` 是 raw 区唯一写入方，所以模式位设在这一处就覆盖全部。
    // ⚠️ `mode()` 只在**创建**时生效。已经落地的旧文件保持原权限，
    //    升级到这一版时要在目标机上手工 `chmod` 一次，见 `docs/deploy.md`。
    if let Some(dir) = local.parent() {
        let mut b = fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        b.mode(0o700);
        b.create(dir)?;
    }
    // 追加 + fsync；调用方在阻塞线程上执行整段校验和写入。
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut fh = opts.open(local)?;
    if let Err(error) = fh.write_all(body).and_then(|()| fh.sync_all()) {
        drop(fh);
        fs::remove_file(local).map_err(|cleanup| {
            MirrorError::Room(format!("写入失败：{error}；清理不可信副本失败：{cleanup}"))
        })?;
        return Err(error.into());
    }

    if have == 0 {
        // 冷启动才数行，换一次独立于字节数的验证。**数手里这份 `body`**：`have == 0`
        // ⇒ 落地的文件内容恒等于它，`fs::read` 把刚 fsync 完的 18 MB 原样读回来只是
        // 第二份 18 MB 分配（`mirror_concurrency = 8` 的冷启动峰值凭空翻倍）。
        // ponytail: 增量时只校验字节数（position 已是精确的端到端证明）。要每次都
        //           校验就得读回整个月文件，月末 1000 群 = 多读 18 GB。
        let n = body.iter().filter(|b| **b == b'\n').count() as u64;
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

    #[tokio::test]
    async fn oversized_body_is_rejected_before_waiting_for_the_rest() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let oss = OssClient::for_test(&format!("http://{}", listener.local_addr().unwrap()));
        let (release, wait) = std::sync::mpsc::channel();
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
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nContent-Length: 1000000\r\n\r\n012345678").unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        let root = testutil::fresh_root("mirror", "oversized-body");
        let file = MonthFile {
            corp: "C".into(),
            room: "R".into(),
            month: "202608".into(),
            object_key: "202608/C/R.ndjson".into(),
            position: 8,
            record_count: 1,
        };
        let result =
            tokio::time::timeout(Duration::from_secs(1), download_one(&oss, &root, &file)).await;
        let _ = release.send(());
        server.join().unwrap();
        let error = result
            .expect("必须在收到第九字节时拒绝，不等待剩余正文")
            .err()
            .unwrap();
        assert!(error.to_string().contains("超过索引"));
        assert!(!ingest::room_path(&root, "202608", "C", "R").exists());
        let large = MonthFile {
            position: MAX_DOWNLOAD_BYTES + 1,
            ..file
        };
        assert!(
            download_one(&oss, &root, &large)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("64 MiB")
        );
    }

    #[tokio::test]
    async fn an_interrupted_local_append_is_replaced_by_a_complete_download() {
        use std::{io::Read, net::TcpListener};

        for have in [3, 16] {
            let root = testutil::fresh_root("mirror", &format!("interrupted-{have}"));
            let file = MonthFile {
                corp: "C".into(),
                room: "R".into(),
                month: "202608".into(),
                object_key: "202608/C/R.ndjson".into(),
                position: 16,
                record_count: 2,
            };
            let local = ingest::room_path(&root, &file.month, &file.corp, &file.room);
            fs::create_dir_all(local.parent().unwrap()).unwrap();
            // 同时覆盖未写完整和长度恰好相等但末尾损坏，不能直接 Skip。
            fs::write(&local, vec![b'x'; have]).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let oss = OssClient::for_test(&format!("http://{}", listener.local_addr().unwrap()));
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("未收到恢复下载：{e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                let valid = request.contains("\r\nrange: bytes=0-15\r\n");
                write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-15/16\r\nContent-Length: 16\r\nConnection: close\r\n\r\n").unwrap();
                stream.write_all(&[A, B].concat()).unwrap();
                valid
            });
            let result = download_with_retry(&oss, &root, &file).await;
            assert!(result.is_ok(), "恢复失败：{:?}", result.err());
            assert!(server.join().unwrap(), "损坏副本必须从零重拉");
            assert_eq!(fs::read(&local).unwrap(), [A, B].concat());
        }
    }

    /// 归零后的旧副本必须消失，否则下游扫描本地目录时仍会读到它。
    #[tokio::test]
    async fn zero_position_removes_stale_file_before_returning_empty() {
        let root = testutil::fresh_root("mirror", "zero-position");
        let file = MonthFile {
            corp: "C".into(),
            room: "R".into(),
            month: "202608".into(),
            // 空对象名让意外进入请求路径时立即失败，测试不访问网络。
            object_key: String::new(),
            position: 0,
            record_count: 0,
        };
        let local = ingest::room_path(&root, &file.month, &file.corp, &file.room);
        write_and_verify(&local, A, 0, 8, 1).unwrap();
        let oss = OssClient::for_test("http://127.0.0.1:1");

        assert!(matches!(
            download_with_retry(&oss, &root, &file).await.unwrap(),
            Outcome::Empty
        ));
        assert!(!local.exists(), "索引已归零，本地仍残留旧消息");
        // 再跑一次时本地已不存在，仍应正常返回 Empty。
        assert!(matches!(
            download_with_retry(&oss, &root, &file).await.unwrap(),
            Outcome::Empty
        ));
    }

    /// 用目录占据文件路径，确定性地制造删除失败，避免依赖运行账号的权限。
    #[tokio::test]
    async fn zero_position_cleanup_failure_is_a_room_failure() {
        let root = testutil::fresh_root("mirror", "zero-position-cleanup-error");
        let file = MonthFile {
            corp: "C".into(),
            room: "R".into(),
            month: "202608".into(),
            object_key: String::new(),
            position: 0,
            record_count: 0,
        };
        let local = ingest::room_path(&root, &file.month, &file.corp, &file.room);
        fs::create_dir_all(&local).unwrap();
        fs::write(local.join("occupied"), A).unwrap();
        let oss = OssClient::for_test("http://127.0.0.1:1");

        let result = download_with_retry(&oss, &root, &file).await;
        assert!(matches!(result, Err(MirrorError::Room(_))));
        assert!(local.exists(), "删除失败不能伪装成 Empty 或递归删除目录");
    }

    #[tokio::test]
    async fn signed_download_retries_resumes_and_rejects_invalid_responses() {
        use std::{io::Read, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (status, body, range, content_range) in [
                ("503 Service Unavailable", &b""[..], "bytes=0-7", ""),
                // 对象可以已继续追加；只下载索引确认的位置，不要求 total == position。
                ("206 Partial Content", A, "bytes=0-7", "bytes 0-7/24"),
                ("206 Partial Content", B, "bytes=8-15", "bytes 8-15/24"),
                ("403 Forbidden", &b""[..], "bytes=0-7", ""),
                (
                    "200 OK",
                    &b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n"[..],
                    "bytes=16-23",
                    "",
                ),
                ("206 Partial Content", A, "bytes=16-23", "bytes 0-7/24"),
                ("206 Partial Content", A, "bytes=16-23", ""),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with("get /202608/c/r.ndjson http/1.1\r\n"));
                assert!(
                    request.contains(
                        "\r\nauthorization: oss4-hmac-sha256 credential=test-access-key/"
                    )
                );
                assert!(request.contains(&format!("\r\nrange: {range}\r\n")));
                assert!(!request.contains("test-secret-key"));
                write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Range: {content_range}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let oss = OssClient::for_test(&base);
        let root = testutil::fresh_root("mirror", "signed-download");
        let mut file = MonthFile {
            corp: "C".into(),
            room: "R".into(),
            month: "202608".into(),
            object_key: "202608/C/R.ndjson".into(),
            position: 8,
            record_count: 1,
        };
        assert!(matches!(
            download_with_retry(&oss, &root, &file).await.unwrap(),
            Outcome::Pulled(8)
        ));
        file.position = 16;
        file.record_count = 2;
        assert!(matches!(
            download_with_retry(&oss, &root, &file).await.unwrap(),
            Outcome::Pulled(8)
        ));
        let local = ingest::room_path(&root, &file.month, &file.corp, &file.room);
        assert_eq!(fs::read(&local).unwrap(), [A, B].concat());
        assert!(matches!(
            download_with_retry(&oss, &root, &file).await.unwrap(),
            Outcome::Skip
        ));
        file.position = 8;
        let forbidden_root = testutil::fresh_root("mirror", "forbidden");
        let error = download_with_retry(&oss, &forbidden_root, &file)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, MirrorError::Room(_)));
        assert!(error.to_string().contains("403 Forbidden"));
        assert!(!ingest::room_path(&forbidden_root, &file.month, &file.corp, &file.room).exists());
        file.position = 24;
        for expected_error in ["200 OK", "Content-Range", "Content-Range"] {
            let error = download_with_retry(&oss, &root, &file).await.err().unwrap();
            assert!(matches!(error, MirrorError::Room(_)));
            assert!(error.to_string().contains(expected_error), "{error}");
            assert_eq!(fs::read(&local).unwrap(), [A, B].concat());
        }
        // 上述错误响应保留了本地内容；随后验证索引归零会清理这份旧副本。
        file.position = 0;
        assert!(matches!(
            download_with_retry(&oss, &root, &file).await.unwrap(),
            Outcome::Empty
        ));
        assert!(!local.exists());
        server.join().unwrap();
    }

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

    /// raw 区放的是未脱敏的客户正文，权限跟 `secrets.toml` 同级。
    /// 靠默认 umask 的话跑批机上任何账号都能翻 —— 这条钉住的是「不靠 umask」。
    #[cfg(unix)]
    #[test]
    fn raw_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp("mode");
        write_and_verify(&p, A, 0, 8, 1).unwrap();
        let mode = |q: &Path| fs::metadata(q).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600, "月文件权限过宽");
        assert_eq!(mode(p.parent().unwrap()), 0o700, "月目录权限过宽");
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
        // 承重规则 2：对象字节数必须与索引位置一致
        let p = tmp("short");
        let e = write_and_verify(&p, A, 0, 99, 1).unwrap_err();
        assert!(matches!(e, MirrorError::Room(_)), "{e}");
        assert!(e.to_string().contains("对象与索引位置不一致"), "{e}");
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
