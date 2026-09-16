//! 外部名册 —— 商家名称与客服姓名的来源，本文件先铺通路的第一段：**Nacos 服务发现**。
//!
//! # 为什么只读工作台可以调外部服务
//!
//! `web/` 顶注那句「只从 MySQL 取数」当初要防的是**工作台绕过 MySQL 自己算指标**。
//! 这里取的是**展示别名**：名字不进任何指标、不进任何聚合键、不落库、不影响任何一个
//! 数字，取不到就回落显示 ID（前端早就写好了这条回落）。所以那条约束**收紧而不是
//! 推翻**：
//!
//! > 事实与指标只从 MySQL 取数；**展示别名可以取外部名册，且必须能回落**。
//!
//! ⚠️ 理由内联在这里就是全部 —— 本仓库的 `docs/adr/` 已删，不为它新开一份。
//!
//! # 只做发现，不做注册
//!
//! 不把工作台注册成 Nacos 实例、不发心跳。它的 HTTP 接口只给自己的前端用，前面是
//! 固定地址的反向代理，**没有第三方消费者**。没有消费者的注册是纯负债：多一个周期
//! 任务、多一种「进程活着但心跳线程死了」的故障形态，换不回任何东西。
//!
//! # 裸 HTTP 打 v1 OpenAPI，不引 SDK
//!
//! `nacos-sdk` 对本仓库净增 18 个 crate（tonic + prost 一系）、**只走 gRPC**、TLS 走
//! native-tls 与本项目通篇 rustls 的选型冲突，且它依赖的 reqwest 大版本与本项目语义化
//! 版本不兼容（会编出两份 reqwest、两套连接池，已实测确认）。而「只发现不注册」需要的
//! 端点只有两个，裸 HTTP 就够。放弃的是秒级变更推送、权重负载均衡、SDK 的本地容灾
//! 落盘和 endpoint 自动寻址 —— 前三项对「调两个内部服务」都无所谓，容灾落盘由
//! 「启动时拉不到就直接失败」替代。
//!
//! 目标端是 Nacos **2.x**，v1 OpenAPI 全部可用且无废弃标记；3.x 会断，届时只换
//! [`Nacos::url`] 里那一截路径。
//!
//! # 服务发现没有长轮询
//!
//! 长轮询是**配置中心**才有的；UDP 推送在 2.x 之后官方不再推荐、OpenAPI 文档里已无
//! 相关参数。所以只能轮询 `instance/list`，间隔取响应体里的 `cacheMillis` ——
//! 那是服务端自己声明的缓存时长，照它走就是官方口径，**不硬编码**。

use super::config::{RosterConfig, RosterSecrets};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

/// 令牌用到 `tokenTtl` 的十分之几就重登（默认 18000s ⇒ 提前半小时）。
///
/// Nacos v1 **没有刷新接口**，只能重新登录。取提前量是为了两头都不沾：
/// 每次请求都登录是白搭一次往返，到期才发现则要先吃一个 403 才知道。
const TOKEN_KEEP: u64 = 9;

/// 刷新失败后的重试间隔 —— 这一趟没拿到 `cacheMillis`，只能自己定一个。
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// `cacheMillis` 的下限。服务端返回 0 时不能照着睡 0 秒，那是对 Nacos 的 DoS。
const MIN_REFRESH: Duration = Duration::from_secs(1);

/// 服务名 → 健康实例的 `http://ip:port`。
type Hosts = RwLock<HashMap<String, Vec<String>>>;

/// 服务发现的结果，后台任务按 `cacheMillis` 周期刷新。
///
/// **只有一个实现，所以是 struct 不是 trait**（本仓库的端口判据：一个适配器 =
/// 假想接缝，两个 = 真接缝）。
pub struct Discovery {
    hosts: Arc<Hosts>,
}

impl Discovery {
    /// 登录 Nacos，解析两个服务名各自的健康实例，然后起后台刷新。
    ///
    /// **任一步失败即返回 `Err`，调用方直接退出进程** —— 配置错误要在进程起来的第一秒
    /// 暴露，不把错的服务名 / 命名空间 / 分组名 / 账号密码带上生产。启动期没有「保留
    /// 上一次」可言：手上一份可用数据都没有。
    pub async fn start(cfg: &RosterConfig, secrets: &RosterSecrets) -> crate::Result<Self> {
        let mut nacos = Nacos::new(cfg, secrets)?;
        let hosts: Arc<Hosts> = Arc::default();
        let services = vec![cfg.merchant_service.clone(), cfg.employee_service.clone()];
        let wait = nacos.refresh(&services, &hosts).await?;
        let task = Arc::clone(&hosts);
        tokio::spawn(async move {
            let mut wait = wait;
            loop {
                tokio::time::sleep(wait).await;
                match nacos.refresh(&services, &task).await {
                    Ok(next) => wait = next,
                    // 运行期刷新失败**保留上一次的实例列表**并 warn，不退出、不清空：
                    // 手上那份几秒前还是对的，远比「清空之后谁都查不到」有用。
                    Err(error) => {
                        tracing::warn!(%error, "Nacos 刷新失败，沿用上一次的实例列表");
                        wait = RETRY_AFTER;
                    }
                }
            }
        });
        Ok(Self { hosts })
    }

    /// 从该服务的健康实例里随机选一个。名册取数按调用逐次选，不做粘连。
    ///
    /// 纳秒当随机源，**不为「选一台机器」引入 `rand`**：两个内部服务、实例个位数，
    /// 要的只是别永远打同一台。Nacos 给的 `weight` 拿到了但不用 —— 加权要先有
    /// 「哪台该多分」的证据，今天没有。
    pub fn pick(&self, service: &str) -> Option<String> {
        let hosts = self.hosts.read().unwrap();
        let list = hosts.get(service).filter(|list| !list.is_empty())?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .subsec_nanos() as usize;
        list.get(nanos % list.len()).cloned()
    }
}

/// Nacos v1 OpenAPI 客户端 —— 登录拿令牌，按服务名查健康实例。
struct Nacos {
    http: reqwest::Client,
    /// 已验证且去掉末尾斜杠的服务端地址，[`Nacos::url`] 直接拼在它后面。
    base: String,
    username: String,
    password: String,
    namespace: String,
    group: String,
    token: String,
    /// 到这个时刻就该重新登录。构造时置为「现在」，于是第一次刷新必然先登录。
    relogin_at: Instant,
}

impl Nacos {
    fn new(cfg: &RosterConfig, secrets: &RosterSecrets) -> crate::Result<Self> {
        const INVALID: &str = "roster.nacos 不是合法的 Nacos 服务端地址：要 \
             http(s)://host:port，不带路径、查询串或凭证（`/nacos/v1/...` 由代码自己拼）";
        let url = reqwest::Url::parse(&cfg.nacos).map_err(|_| INVALID)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(INVALID.into());
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(cfg.timeout_secs))
                .build()?,
            base: cfg.nacos.trim_end_matches('/').to_owned(),
            username: secrets.username.clone(),
            password: secrets.password.clone(),
            namespace: cfg.namespace.clone(),
            group: cfg.group_name.clone(),
            token: String::new(),
            relogin_at: Instant::now(),
        })
    }

    /// v1 端点。`expect` 合法：`new` 已经确认 `base` 能解析且没有路径，
    /// 后面接一截固定 ASCII 路径不可能让它解析不了。
    fn url(&self, path: &str) -> reqwest::Url {
        reqwest::Url::parse(&format!("{}/nacos/v1/{path}", self.base))
            .expect("构造时已验证服务端地址")
    }

    /// 解析全部服务名并**整体替换**。任一失败即不动旧表，由调用方决定是崩还是沿用。
    /// 返回下一次刷新的间隔 = 各服务 `cacheMillis` 里最小的那个。
    async fn refresh(&mut self, services: &[String], hosts: &Hosts) -> crate::Result<Duration> {
        self.ensure_token().await?;
        let mut fresh = HashMap::new();
        // ⚠️ 起点是 `MAX` 不是 `RETRY_AFTER`：拿后者当起点等于给刷新间隔封了个 10 秒的
        // 顶，服务端说 60 秒也照旧十秒一轮 —— 那就不是「听服务端的」了。
        let mut wait = Duration::MAX;
        for service in services {
            let (list, cache) = self.instances(service).await?;
            wait = wait.min(cache);
            fresh.insert(service.clone(), list);
        }
        let mut current = hosts.write().unwrap();
        // 只在**落地之后**、且确实变了才打日志：刷新是每十秒一次的常态，
        // 每次都打就是把「两个服务各解析到几个健康实例」这条正向信号淹掉。
        for service in services {
            if current.get(service) != fresh.get(service) {
                let list = &fresh[service];
                tracing::info!(
                    service,
                    healthy = list.len(),
                    instances = list.join(" "),
                    "Nacos 解析到健康实例"
                );
            }
        }
        *current = fresh;
        Ok(wait.max(MIN_REFRESH))
    }

    async fn ensure_token(&mut self) -> crate::Result<()> {
        if Instant::now() < self.relogin_at {
            return Ok(());
        }
        let response = self
            .http
            .post(self.url("auth/login"))
            .form(&[
                ("username", self.username.as_str()),
                ("password", self.password.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("Nacos 登录请求失败（检查 roster.nacos 是否可达）：{e}"))?;
        let status = response.status();
        // ⚠️ 正文里有 accessToken，**一个字都不许进错误信息** —— 那会落进 journald。
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "Nacos 登录被拒（HTTP {status}）：检查 secrets.toml 的 \
                 [roster].username / password，或服务端是否开启鉴权"
            )
            .into());
        }
        let login: Login = serde_json::from_str(&body)
            .map_err(|e| format!("Nacos 登录响应不是预期的 accessToken/tokenTtl：{e}"))?;
        self.token = login.access_token;
        // 先除后乘、再 `checked_add`：`tokenTtl` 是上游给的数，乘出来的 `Duration`
        // 和加出来的 `Instant` 都会在溢出时 panic —— 而这里是后台任务，
        // panic 掉就是刷新从此静默停摆。荒唐的 ttl 退化成「立刻重登」，方向是安全的。
        let keep = Duration::from_secs(login.token_ttl / 10 * TOKEN_KEEP);
        self.relogin_at = Instant::now()
            .checked_add(keep)
            .unwrap_or_else(Instant::now);
        Ok(())
    }

    /// 只取健康实例。`healthyOnly=true` 已经让服务端筛过一遍，这里**再筛一次**：
    /// 上游是信任边界，它多给了什么不该由我们替它兜着。
    ///
    /// 取 `&mut self` 是为了在服务端提前作废令牌时把 `relogin_at` 拨回来，见下方注释。
    async fn instances(&mut self, service: &str) -> crate::Result<(Vec<String>, Duration)> {
        let mut url = self.url("ns/instance/list");
        url.query_pairs_mut()
            .append_pair("serviceName", service)
            .append_pair("groupName", &self.group)
            .append_pair("namespaceId", &self.namespace)
            .append_pair("healthyOnly", "true")
            // v1 的鉴权就长这样：令牌**作为查询参数**带上，没有请求头形式。
            .append_pair("accessToken", &self.token);
        let response = self.http.get(url).send().await.map_err(|e| {
            // ⚠️ **必须剥掉 URL**：`accessToken` 就在查询串里，而 reqwest 的错误
            // `Display` 无条件在末尾拼一句 `for url (...)` —— 不剥的话 Nacos 抖一下，
            // 完整令牌就随这条 warn 进了 journald。这正是 `ensure_token` 那条
            // 「一个字都不许进错误信息」要防的事，只是漏了传输错误这一路。
            format!("Nacos 查询服务 `{service}` 的实例失败：{}", e.without_url())
        })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // 401/403 = 服务端把令牌作废了（Nacos 重启、改密、清会话）。不拨回重登时刻的话，
            // 后台循环会每 10 秒失败一次直到 `tokenTtl` 的九成走完 —— 默认 18000s 就是
            // **最长 4.5 小时实例列表不再更新**，而 `pick()` 一直在发已经下线的地址。
            if matches!(status.as_u16(), 401 | 403) {
                self.relogin_at = Instant::now();
            }
            return Err(format!("Nacos 查询服务 `{service}` 返回 HTTP {status}").into());
        }
        let list: InstanceList = serde_json::from_str(&body)
            .map_err(|e| format!("Nacos 查询服务 `{service}` 的应答无法解析：{e}"))?;
        let hosts: Vec<String> = list
            .hosts
            .iter()
            .filter(|host| host.healthy)
            .map(|host| format!("http://{}:{}", host.ip, host.port))
            .collect();
        if hosts.is_empty() {
            return Err(format!(
                "Nacos 查不到服务 `{service}` 的健康实例：检查服务名、\
                 roster.namespace（`{}`）、roster.group_name（`{}`），\
                 或上游根本还没注册上来",
                self.namespace, self.group
            )
            .into());
        }
        Ok((hosts, Duration::from_millis(list.cache_millis)))
    }
}

#[derive(Deserialize)]
struct Login {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// 秒。默认 18000（5 小时）。
    #[serde(rename = "tokenTtl")]
    token_ttl: u64,
}

#[derive(Deserialize)]
struct InstanceList {
    #[serde(rename = "cacheMillis")]
    cache_millis: u64,
    hosts: Vec<Host>,
}

#[derive(Deserialize)]
struct Host {
    ip: String,
    port: u16,
    healthy: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const MERCHANT: &str = "merchant-service";
    const EMPLOYEE: &str = "employee-service";
    const LOGIN: &str = "/nacos/v1/auth/login";
    const LIST: &str = "/nacos/v1/ns/instance/list";

    /// 脚本化应答的本地 HTTP 端点 —— 照 `testutil::http_model` 的配方写，
    /// 差异是**按路径分发且支持 GET**（那份只应答固定路径的 POST）。
    ///
    /// 每条脚本是 `(路径, 状态码, 应答 JSON)`；来一个请求就取**最早一条路径相同**的
    /// 应答并用掉，于是同一路径的多次调用可以给不同应答（token 过期重登就靠这个）。
    /// 收到脚本里没有的路径直接 panic —— 路径拼错要当场看见，不是静默对不上。
    ///
    /// 返回收到的 `(请求行, 正文)`，顺序即到达顺序。
    fn scripted(
        mut script: Vec<(&'static str, u16, Value)>,
    ) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            time::Instant,
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

    fn login(ttl: u64, token: &str) -> (&'static str, u16, Value) {
        (LOGIN, 200, json!({"accessToken": token, "tokenTtl": ttl}))
    }

    fn hosts(hosts: Value) -> (&'static str, u16, Value) {
        (LIST, 200, json!({"cacheMillis": 10000, "hosts": hosts}))
    }

    /// 服务端把令牌作废了（Nacos 重启 / 改密 / 清会话）。
    fn denied() -> (&'static str, u16, Value) {
        (LIST, 403, json!({"status": 403}))
    }

    fn config(base: &str) -> RosterConfig {
        RosterConfig {
            nacos: base.into(),
            namespace: "public".into(),
            group_name: "DEFAULT_GROUP".into(),
            merchant_service: MERCHANT.into(),
            employee_service: EMPLOYEE.into(),
            ttl_secs: 300,
            timeout_secs: 3,
        }
    }

    fn secrets() -> RosterSecrets {
        RosterSecrets {
            username: "nacos".into(),
            password: "s3cret".into(),
        }
    }

    /// 运行期刷新失败**不清空**上一次的实例列表 —— 启动期拉不到是崩，运行期拉不到是
    /// 沿用。两者的差别就在这张表上，清空了页面会从「显示名字」退成「显示 ID」。
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_previous_instance_list() {
        let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
        // 脚本只够一轮；第二轮时假服务端已经退出，连接直接被拒。
        let (base, server) = scripted(vec![
            login(18000, "tok"),
            hosts(healthy.clone()),
            hosts(healthy),
        ]);
        let cfg = config(&base);
        let mut nacos = Nacos::new(&cfg, &secrets()).unwrap();
        let services = vec![MERCHANT.to_owned(), EMPLOYEE.to_owned()];
        let hosts = Hosts::default();
        nacos.refresh(&services, &hosts).await.unwrap();
        server.join().unwrap();

        let error = nacos
            .refresh(&services, &hosts)
            .await
            .unwrap_err()
            .to_string();
        // ⚠️ 令牌在查询串里，而这条错误会原样进 `warn!` 再进 journald。
        // reqwest 的 Display 默认要拼一句 `for url (...)` —— 剥掉了才看得到这条通过。
        assert!(!error.contains("tok"), "令牌泄漏进日志了：{error}");
        let kept = hosts.read().unwrap();
        assert_eq!(kept[MERCHANT], ["http://10.0.0.1:8080"]);
        assert_eq!(kept[EMPLOYEE], ["http://10.0.0.1:8080"]);
    }

    /// 服务端提前作废令牌（403）→ **下一轮立刻重新登录**，而不是干等到 `tokenTtl`
    /// 的九成走完。默认 ttl 18000s，等下去就是最长 4.5 小时实例列表不再更新，
    /// 其间 `pick()` 一直在发可能已经下线的地址。
    #[tokio::test]
    async fn a_token_revoked_by_the_server_forces_an_immediate_relogin() {
        let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
        let (base, server) = scripted(vec![
            login(18000, "tok-old"),
            hosts(healthy.clone()),
            denied(),
            login(18000, "tok-new"),
            hosts(healthy.clone()),
            hosts(healthy),
        ]);
        let cfg = config(&base);
        let mut nacos = Nacos::new(&cfg, &secrets()).unwrap();
        let services = vec![MERCHANT.to_owned(), EMPLOYEE.to_owned()];
        let hosts = Hosts::default();
        assert!(nacos.refresh(&services, &hosts).await.is_err());
        nacos.refresh(&services, &hosts).await.unwrap();

        let requests = server.join().unwrap();
        let logins = requests.iter().filter(|(l, _)| l.contains(LOGIN)).count();
        assert_eq!(logins, 2, "403 之后必须重新登录：{requests:?}");
        let last = requests.last().unwrap();
        assert!(last.0.contains("accessToken=tok-new"), "{last:?}");
    }

    /// 协议：登录请求的形状、令牌作为**查询参数**、只有健康实例被选中，
    /// 且两个服务共用一次登录（不是每次请求都登录）。
    #[tokio::test]
    async fn login_shape_token_as_query_parameter_and_only_healthy_instances() {
        let (base, server) = scripted(vec![
            login(18000, "tok-abc"),
            hosts(json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}])),
            hosts(json!([
                {"ip": "10.0.0.2", "port": 9090, "healthy": false},
                {"ip": "10.0.0.3", "port": 9090, "healthy": true},
            ])),
        ]);
        let discovery = Discovery::start(&config(&base), &secrets()).await.unwrap();
        assert_eq!(discovery.pick(MERCHANT).unwrap(), "http://10.0.0.1:8080");
        // 不健康那台必须一次都选不到 —— 随机选也不能选到它。
        assert_eq!(discovery.pick(EMPLOYEE).unwrap(), "http://10.0.0.3:9090");

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3, "两个服务只登录一次：{requests:?}");
        let (line, body) = &requests[0];
        assert_eq!(line, &format!("POST {LOGIN} HTTP/1.1"));
        assert_eq!(body, "username=nacos&password=s3cret");
        for (line, body) in &requests[1..] {
            assert!(line.starts_with(&format!("GET {LIST}?")), "{line}");
            assert!(line.contains("accessToken=tok-abc"), "{line}");
            assert!(line.contains("healthyOnly=true"), "{line}");
            assert!(line.contains("groupName=DEFAULT_GROUP"), "{line}");
            assert!(line.contains("namespaceId=public"), "{line}");
            assert!(body.is_empty(), "实例查询是 GET，不该有正文");
        }
        assert!(requests[1].0.contains(&format!("serviceName={MERCHANT}")));
        assert!(requests[2].0.contains(&format!("serviceName={EMPLOYEE}")));
    }

    /// 协议：令牌按 `tokenTtl` 算的提前量到了才重新登录，新令牌立刻用上。
    #[tokio::test]
    async fn an_expiring_token_is_renewed_before_the_next_lookup() {
        let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
        // 第一次登录的 ttl = 0 ⇒ 提前量也是 0 ⇒ 下一轮刷新必须重新登录。
        let (base, server) = scripted(vec![
            login(0, "tok-old"),
            hosts(healthy.clone()),
            hosts(healthy.clone()),
            login(18000, "tok-new"),
            hosts(healthy.clone()),
            hosts(healthy),
        ]);
        let cfg = config(&base);
        let mut nacos = Nacos::new(&cfg, &secrets()).unwrap();
        let services = vec![MERCHANT.to_owned(), EMPLOYEE.to_owned()];
        let hosts = Hosts::default();
        nacos.refresh(&services, &hosts).await.unwrap();
        nacos.refresh(&services, &hosts).await.unwrap();

        let requests = server.join().unwrap();
        let logins: Vec<_> = requests.iter().filter(|(l, _)| l.contains(LOGIN)).collect();
        assert_eq!(logins.len(), 2, "过期后要重新登录一次：{requests:?}");
        let tokens: Vec<_> = requests
            .iter()
            .filter(|(l, _)| l.contains(LIST))
            .map(|(l, _)| l.contains("accessToken=tok-new"))
            .collect();
        assert_eq!(
            tokens,
            [false, false, true, true],
            "重登之后必须立刻换用新令牌：{requests:?}"
        );
    }

    /// 启动期：解析不到实例就**退出**，错误信息要指得出是哪一项。
    #[tokio::test]
    async fn an_unknown_service_name_fails_startup_with_a_locatable_error() {
        let (base, server) = scripted(vec![login(18000, "tok"), hosts(json!([]))]);
        let error = Discovery::start(&config(&base), &secrets())
            .await
            .err()
            .expect("解析不到实例必须启动失败")
            .to_string();
        assert!(error.contains(MERCHANT), "{error}");
        assert!(error.contains("roster.namespace"), "{error}");
        assert!(error.contains("roster.group_name"), "{error}");
        server.join().unwrap();
    }

    /// 服务端地址写错 —— 连登录都发不出去，错误要点名是哪个配置键。
    #[test]
    fn a_malformed_endpoint_names_the_configuration_key() {
        for bad in ["nacos.internal:8848", "http://host:8848/nacos", ""] {
            let mut cfg = config("http://127.0.0.1:8848");
            cfg.nacos = bad.into();
            let error = Nacos::new(&cfg, &secrets()).err().unwrap().to_string();
            assert!(error.contains("roster.nacos"), "{bad}：{error}");
        }
    }
}
