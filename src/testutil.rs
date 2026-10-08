//! 测试共用 fixture，只在 `cfg(test)` 下编译。
//! 此前 `ingest` 的 `raw()` 和 `mirror` 的 `tmp()` 是同一个配方写两遍。

use serde_json::Value;

/// 本地 HTTP 模型端点，供分类与真实抽取 adapter 共用。
pub fn http_model(
    replies: Vec<(u16, Value)>,
    wait_for_all: bool,
) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        time::{Duration, Instant},
    };
    fn send(mut stream: TcpStream, (status, reply): (u16, Value)) {
        let body = reply.to_string();
        write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        let mut pending = Vec::new();
        for reply in replies {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("测试模型未收到预期请求：{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap().to_ascii_lowercase();
            assert!(header.starts_with("post /v1/chat/completions "));
            let len: usize = header
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut body = vec![0; len];
            stream.read_exact(&mut body).unwrap();
            requests.push(serde_json::from_slice(&body).unwrap());
            if wait_for_all {
                pending.push((stream, reply));
            } else {
                send(stream, reply);
            }
        }
        for (stream, reply) in pending {
            send(stream, reply);
        }
        requests
    });
    (base, handle)
}

pub fn completion(content: &str, finish_reason: &str) -> Value {
    serde_json::json!({"id":"test", "object":"chat.completion", "created":0, "model":"test-model", "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":finish_reason}]})
}

/// ③ 抽取那一队的 fixture。
pub fn test_llm(base: &str, model: &str) -> crate::llm::Llm {
    build_llm(base, model, false)
}

/// ⑤ 打标那一队的 fixture。**凡是要喂给 `Classifier::new` 的都用这个。**
///
/// 两队的 `max_tokens` 不是一个数（12000 对 6000），而 `classify.rs` 有一条测试正钉着
/// 「打标请求真的带着小预算出门」—— 拿抽取那份去打标，它会静默从「预算被压小了」
/// 翻转成「预算没被压小」：测试照样绿，保证没了。所以 fixture 必须分两个。
pub fn test_classify_llm(base: &str, model: &str) -> crate::llm::Llm {
    build_llm(base, model, true)
}

fn build_llm(base: &str, model: &str, classify: bool) -> crate::llm::Llm {
    let mut cfg: crate::config::Config = toml::from_str(include_str!("../config.toml")).unwrap();
    // 超时压到秒级：好几个用例靠「连 127.0.0.1:1 立刻失败」跑，生产那个 300s 会让它们挂满。
    cfg.llm.timeout_secs = 3;
    cfg.llm.connect_timeout_secs = 1;
    let m = if classify {
        &mut cfg.llm.classify
    } else {
        &mut cfg.llm.extract
    };
    m.base_url = base.into();
    m.model = model.into();
    let m = if classify {
        &cfg.llm.classify
    } else {
        &cfg.llm.extract
    };
    crate::llm::Llm::new(&cfg.llm, m, "test-only-key".into()).unwrap()
}
use std::{
    fs,
    path::{Path, PathBuf},
};

/// 每个用例一个独立的 raw 区，落在系统临时目录下（上一次的残留先删掉）。
pub fn fresh_root(prefix: &str, name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("c2e-{prefix}-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

/// 把若干行按生产布局写成一个月文件。路径问 `ingest::room_path` 要 —— 布局只拼一次。
pub fn write_month(root: &Path, month: &str, corp: &str, room: &str, rows: &[Value]) {
    let p = crate::stage::ingest::room_path(root, month, corp, room);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(
        &p,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
}

/// 业务本地时间 → 上游 `messageTime` 毫秒。本地时区是 `ingest::TZ`（Asia/Shanghai =
/// UTC+8）—— 「+8」这条换算只写在这一处；此前 3 个测试文件各手写一遍 `- hours(8)`。
pub fn upstream_ms(local: chrono::NaiveDateTime) -> i64 {
    (local - chrono::Duration::hours(8))
        .and_utc()
        .timestamp_millis()
}

/// 仅供显式启用的 MySQL 测试：每个用例创建自己的空库，不使用项目 secrets。
pub async fn mysql_pool(name: &str) -> sqlx::MySqlPool {
    use std::{
        str::FromStr,
        sync::atomic::{AtomicUsize, Ordering},
    };
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    let url = std::env::var("CHAT2EVENTS_TEST_DATABASE_URL")
        .expect("MySQL 测试需要显式设置 CHAT2EVENTS_TEST_DATABASE_URL，禁止使用业务库");
    let options = sqlx::mysql::MySqlConnectOptions::from_str(&url).expect("测试连接地址格式错误");
    assert!(
        options.get_database().is_some_and(|d| d.ends_with("_test")),
        "测试连接必须指向以 _test 结尾的专用库"
    );
    let admin = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let database = format!(
        "c2e_test_{}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        name
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE {database} CHARACTER SET utf8mb4"
    )))
    .execute(&admin)
    .await
    .unwrap();
    admin.close().await;
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(4)
        .connect_with(options.database(&database))
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
    pool
}

pub async fn drop_mysql_database(pool: sqlx::MySqlPool) {
    let (name,): (String,) = sqlx::query_as("SELECT DATABASE()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(name.starts_with(&format!("c2e_test_{}_", std::process::id())));
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE {name}")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// 脚本化应答的本地 HTTP 端点（Nacos 协议测试与名册测试共用）—— 照 [`http_model`] 的配方写，
/// 差异是**按路径分发且支持 GET**（那份只应答固定路径的 POST）。
///
/// 每条脚本是 `(路径, 状态码, 应答 JSON)`；来一个请求就取**最早一条路径相同**的
/// 应答并用掉，于是同一路径的多次调用可以给不同应答（token 过期重登就靠这个）。
/// 收到脚本里没有的路径直接 panic —— 路径拼错要当场看见，不是静默对不上。
///
/// 返回收到的 `(请求行, 正文)`，顺序即到达顺序。
pub fn scripted(
    mut script: Vec<(&'static str, u16, Value)>,
) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        while !script.is_empty() {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("假 Nacos 未收到预期请求：{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            // 大小写不能归一化：serviceName / accessToken 这些参数名是区分大小写的。
            let header = String::from_utf8(header).unwrap();
            let line = header.lines().next().unwrap().trim().to_owned();
            let target = line.split_whitespace().nth(1).unwrap().to_owned();
            let path = target.split('?').next().unwrap().to_owned();
            let len: usize = header
                .lines()
                .find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; len];
            stream.read_exact(&mut body).unwrap();
            let at = script
                .iter()
                .position(|(scripted, ..)| *scripted == path)
                .unwrap_or_else(|| panic!("假 Nacos 收到未脚本化的请求：{line}"));
            let (_, status, reply) = script.remove(at);
            let reply = reply.to_string();
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            requests.push((line, String::from_utf8(body).unwrap()));
        }
        requests
    });
    (base, handle)
}
