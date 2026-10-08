//! Nacos 服务发现 —— 内核层。只读工作台的名册（[`crate::web::roster`]）靠它找到
//! 内部服务；它只管「服务名 → 健康实例地址」，不认识名册，也不认识任何业务接口。
//!
//! 放在 crate 根而不是 `web/` 下：服务发现不是只读旁路独有的东西，进程层同样会用，
//! 让进程层 `use crate::web::...` 就是它依赖了只读旁路，违反 `lib.rs` 顶注
//! 「四类东西写在路径上」的判据。被两类用到的东西归内核。
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

/// Nacos 的连接参数 —— 全部必填，代码里没有默认值。
///
/// 住在工作台配置的 `[roster]` 节里（工作台把它 `flatten` 进自己的 `RosterConfig`，
/// 配置文件格式不变）。
#[derive(Deserialize)]
pub struct NacosConfig {
    /// Nacos 服务端地址，`scheme://host:port`，**不带 `/nacos` 路径**。
    pub nacos: String,
    /// 命名空间 ID 与分组名。**用的就是 Nacos 默认值时也必须显式写出来** ——
    /// 落进代码当默认值的话，配错了不会在启动时喊，只表现成「解析不到实例」。
    pub namespace: String,
    pub group_name: String,
    /// 商家域与员工域的服务名。写错即启动失败，错误信息带上服务名。
    pub merchant_service: String,
    pub employee_service: String,
    /// Nacos 与两个业务服务共用的 HTTP 超时。
    pub timeout_secs: u64,
}

/// Nacos 的账号密码。跟数据库只读账号同一个 `secrets.toml`，同一份 0600 检查。
///
/// 两个键**必填但可以是空串**：`username = ""` 表示服务端没开鉴权，此时
/// [`Discovery`] 既不登录也不带 `accessToken`。必填是有意的 ——
/// 「忘了写」和「确实不需要」得长得不一样，前者必须启动即崩。
#[derive(Deserialize)]
pub struct NacosSecrets {
    pub username: String,
    pub password: String,
}

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
    pub async fn start(cfg: &NacosConfig, secrets: &NacosSecrets) -> crate::Result<Self> {
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

    /// 测试用：直接给定「服务名 → 实例地址」，不连 Nacos、不起后台任务。
    #[cfg(test)]
    pub(crate) fn fixed(instances: &[(&str, &str)]) -> Self {
        let hosts = instances
            .iter()
            .map(|(service, host)| ((*service).to_owned(), vec![(*host).to_owned()]))
            .collect();
        Self {
            hosts: Arc::new(RwLock::new(hosts)),
        }
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
    fn new(cfg: &NacosConfig, secrets: &NacosSecrets) -> crate::Result<Self> {
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
        // 服务端没开鉴权时 `secrets.toml` 的 `[roster].username` 留空 ⇒ 不登录，
        // 实例查询也不带 `accessToken`。空账号照样去 POST `auth/login` 换回来的只是
        // 一个 403（Nacos 对空用户名一律 `user not found`），那会让工作台起不来。
        if self.username.is_empty() {
            return Ok(());
        }
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
            .append_pair("healthyOnly", "true");
        // v1 的鉴权就长这样：令牌**作为查询参数**带上，没有请求头形式。
        // 未开鉴权时没有令牌，这个参数整个不出现。
        if !self.token.is_empty() {
            url.query_pairs_mut()
                .append_pair("accessToken", &self.token);
        }
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
mod tests;
