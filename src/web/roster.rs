//! 外部名册 —— 商家名称与客服姓名的来源。两件事一个文件：
//! **Nacos 服务发现**（[`Discovery`]）＋ **按需批量查询的名册缓存**（[`Roster`]）。
//!
//! 不拆成「Nacos 客户端」和「名册」两个文件：本仓库的端口判据是
//! **一个适配器 = 假想接缝，两个 = 真接缝**，拆开就是为一个不存在的第二实现
//! 付两个文件的钱。
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
//!
//! # 名册：按需批量 + 负缓存 + 整体过期
//!
//! **不做启动预热、不拉全量** —— 两个服务都提供批量按 ID 查询，而 ID 集合天然被
//! 查询窗口收敛（筛选器本来就只列窗口内出现过的群和客服）。请求到达时算出**缺失**的
//! 那批，一次补齐。
//!
//! 值是 `Option<String>`：**`None` = 查过，上游明确说查无此人**。不记住的话，同一批
//! 不存在的 ID 每个请求都要重查一遍。⚠️ 与「调不通」严格分开 —— 后者不进负缓存，
//! 否则一次上游抖动会把所有人钉在「显示 ID」上整整一个 TTL。
//!
//! TTL 到期**整体清空**，不做逐条过期：名册规模很小（客服实测二十余人、商家数百），
//! 逐条过期要多存 N 个时间戳再逐个检查，换不来任何东西。

use super::config::{RosterConfig, RosterSecrets};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
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

/// 展示别名的进程内名册 —— **按需批量查询 + 负缓存 + 整体过期 + 代数**。
///
/// **只有一个实现，所以是 struct 不是 trait。** 它是本次唯一的接缝，支持两种测试模式：
/// 预填充（[`Roster::canned`]，完全不碰网络）与协议（指向本地假 HTTP 服务端）。
///
/// ⚠️ **名字是展示，不是事实，也不是维度**：不进指标、不进聚合键、不落库。
/// 取不到一律回落显示 ID —— 那条回落前端早就写好了。
pub struct Roster {
    upstream: Upstream,
    ttl: Duration,
    /// 进程起来的时刻 —— [`Roster::generation`] 的原点。
    born: Instant,
    /// **一把锁串行化填充**：并发请求不为同一批 ID 重复发起调用。锁跨 `await`
    /// 持有，所以必须是 `tokio` 的那把。成功时后到的请求醒来已经不缺了，
    /// 于是只等**一次**往返；失败那一路由 [`FAILURE_COOLDOWN`] 兜住。
    names: tokio::sync::Mutex<Names>,
    /// 实际向上游发起过几次批量查询。负缓存生效 = 同一批查无此人的 ID 不再 +1。
    lookups: AtomicUsize,
}

/// 名册的两个域。**不是 trait，是枚举**：两个域的缓存语义完全相同
/// （按需批量 · 负缓存 · 整体过期 · 冷却），只有「怎么发这次请求、怎么读这个应答」不同。
///
/// ⚠️ 共用那套语义是**有意的**，不是顺手抽象：「失败绝不进负缓存」「冷却按域分开」
/// 这两条都是承重的，抄成两份就有两个地方会漂。真正不同的那点小差异用一个 `match`
/// 分掉，比给「一个域」发明一个 trait 便宜得多。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Domain {
    /// 账号域 `account-app`：`officialUserId` → 客服姓名。
    Employee = 0,
    /// 商家域 `merchant-app`：`merchant_id` → 店铺名称。
    Merchant = 1,
}

impl Domain {
    /// 一次请求最多几个 ID。**两个域的上限来源完全不同**，见各自的常量。
    fn batch(self) -> usize {
        match self {
            Self::Employee => EMPLOYEE_BATCH,
            Self::Merchant => MERCHANT_BATCH,
        }
    }

    /// 日志与错误文案里的名字。
    fn label(self) -> &'static str {
        match self {
            Self::Employee => "员工名册",
            Self::Merchant => "商家名册",
        }
    }
}

struct Names {
    /// 按 [`Domain`] 索引：ID → 名字。**`None` = 上游明确说查无此人 / 此商家**（负缓存）。
    tables: [HashMap<String, Option<String>>; 2],
    /// 按 [`Domain`] 索引的冷却截止时刻 —— 见 [`FAILURE_COOLDOWN`]。
    /// **按域分开**：账号域挂了不该连累商家域，它们是两个服务。
    cooldown: [Instant; 2],
    /// 这两张表是在第几个 TTL 纪元里建起来的，与 [`Roster::generation`] 同源。
    epoch: u64,
}

/// 账号域（`account-app`）的「按企业主体批量查企微员工详情」v2。
///
/// ⚠️ **必须用 v2，不能用 v1**（`/rpc/v1/.../getWechatEmpInfoMapByUserIds`）：
/// v1 不收 `corpId`，服务端内部兜底成默认主体 —— 多主体场景下会**静默漏数据**，
/// 表现成「这些人查无此人」，而那正是会被负缓存记住的那一档。
const EMPLOYEE_PATH: &str = "/rpc/v2/work/wechat/emp/getWechatEmpInfoMapByCorpIdAndUserIds";

/// 一次请求最多几个 `userId` —— 服务端 `@Size(max = 100)`，超了整批被校验拦下，
/// **分批是调用方的责任**。
const EMPLOYEE_BATCH: usize = 100;

/// 商家域（`merchant-app`）的「按 id 批量查店铺名称」。
///
/// ⚠️ **同族还有两个接口，名字相近但语义不同，别换**：
/// `getMerchantNameAndStatusMap` **不过滤已删除商家**（底层 SQL 只有 `id in (...)`），
/// `getMerchantBaseInfosMap` 会把四类手机号一起拉回来（为取一个店名新开一条 PII 路径）。
/// 选这一个正是因为它**最轻**（`Map<Long, String>`）且删除过滤**可控**。
const MERCHANT_PATH: &str = "/rpc/merchant/getMerchantNameByIds";

/// `isDeleted=0` = 只要未删除的商家。
///
/// ⚠️ **这是当前的工作假设，不是业务确认过的定义**：它把「已注销」当成
/// `is_deleted` 软删除。`is_deleted` 和 `status`（1正常 2拉黑 3冻结 4保护）是**两个维度**，
/// 而这个接口只回名字、**不带 `status`**。业务若最后说「已注销」指拉黑 / 冻结，
/// 这里换个值是不够的，要改走 `getMerchantNameAndStatusMap` 读 `status`。
const MERCHANT_NOT_DELETED: &str = "0";

/// 一次请求最多几个 `merchant_id`。
///
/// ⚠️ **这个上限是我们自己定的，服务端没有条数兜底**（不像账号域那边超了会明确报错）——
/// 超长只表现为网关 414 或**截断**，那是会静默算错的错法。
/// 换算：商家 id 是**数据库自增**不是雪花（`Merchant.java` 的 `IdType.AUTO`），7-8 位量级，
/// 逗号分隔每个约 9 字节 ⇒ 500 个 ≈ 4.5 KB，加路径仍在常见 8 KB 网关限制内。
/// **它防的不是正常量级**（商家 ID 被查询窗口收敛，几百而已），是窗口异常放大的那一天。
const MERCHANT_BATCH: usize = 500;

/// 统一包装 `Result` 的成功码。**是 1，不是 0 也不是 200**（`ResultEnum.SUCCESS`）。
/// 两个域同一套包装，所以这一条也共用。
///
/// ⚠️ **必须同时要求 `code == 1` 和 `data` 非空**，两道一起才闭环：
/// 上游「失败时 `data` 是 null 还是 `{}`」没有权威答案（`Result` 来自外部依赖
/// `com.jdd.integration:jdd-common-resultvo`，源码不在手上）。而空 map 是**合法的成功
/// 响应**（查不到的 ID 不放进 map），所以万一失败时 `data` 也是 `{}`，只看 `data`
/// 就会把一批人当成「查无此人」负缓存住 —— 那是失败不该有的待遇。`code` 这一道先拦住它。
const RESULT_OK: i64 = 1;

/// 上游调不通之后冷却多久再试。
///
/// ⚠️ **它挡的是连接池，不是上游。** 失败**故意不记进负缓存**（那会把所有人钉住
/// 一整个 TTL），于是缺失集合一直非空 —— 没有冷却的话，堵在那把锁后面的每个请求
/// 醒来都会自己再发一次，串行地一人一个 `timeout_secs`。而 `serve::filters` 是
/// **握着一条只读连接和一个打开的快照事务**在等，`web.concurrency` 个请求排下来
/// 足够把 `mysql.max_connections` 耗光，把毫不相干的接口一起拖死。
///
/// 冷却期内直接回落（不发请求、不打日志），于是一次上游抖动最多花掉**一个**往返。
/// 取 10 秒：比 `timeout_secs` 的秒级量纲大一档，又远小于名册 TTL。
const FAILURE_COOLDOWN: Duration = Duration::from_secs(10);

enum Upstream {
    /// 生产：Nacos 找到实例，再打那个域的批量查询。
    Service {
        discovery: Discovery,
        /// 按 [`Domain`] 索引的服务名。
        services: [String; 2],
        http: reqwest::Client,
    },
    /// 预填充模式的测试替身 —— **上游的假答案，不是缓存的初始内容**。
    /// 于是「按需填充 → 映射增长」这条路在测试里照样完整走一遍，
    /// 表里没有的 ID 就是「查无此人」，跟真上游一个语义。
    #[cfg(test)]
    Canned([HashMap<String, String>; 2]),
}

impl Roster {
    /// 起 Nacos 发现，再拿它建名册。发现失败即返回 `Err` —— 见 [`Discovery::start`]。
    pub async fn start(cfg: &RosterConfig, secrets: &RosterSecrets) -> crate::Result<Arc<Self>> {
        let discovery = Discovery::start(cfg, secrets).await?;
        Ok(Arc::new(Self::with_upstream(
            Upstream::Service {
                discovery,
                services: [cfg.employee_service.clone(), cfg.merchant_service.clone()],
                // ⚠️ 超时是**每一批**的，不是整次补齐的：账号域 100 个一批，
                // 补 250 个人就是 3 批、最坏 3 × timeout_secs。名册规模本来就小
                // （实测客服二十余人 = 1 批），真长到要分很多批时得在这里加总预算。
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .build()?,
            },
            Duration::from_secs(cfg.ttl_secs),
        )))
    }

    fn with_upstream(upstream: Upstream, ttl: Duration) -> Self {
        Self {
            upstream,
            ttl,
            born: Instant::now(),
            names: tokio::sync::Mutex::new(Names {
                tables: Default::default(),
                cooldown: [Instant::now(); 2],
                epoch: 0,
            }),
            lookups: AtomicUsize::new(0),
        }
    }

    /// 把一批 `officialUserId` 解析成客服姓名。
    pub async fn employees(
        &self,
        corp: &str,
        wanted: &BTreeSet<String>,
    ) -> HashMap<String, String> {
        self.resolve(corp, Domain::Employee, wanted).await
    }

    /// 把一批 `merchant_id` 解析成店铺名称。
    ///
    /// ID 是**字符串不是整数**，全程不解析成 `u64` —— `merchant_id` 在本仓库是
    /// `BIGINT UNSIGNED`，而取数那边本来就 `CAST(... AS CHAR)` 出来（免得前端丢精度）。
    /// 原样带出去、原样对回来，中间少一次可能溢出的转换。
    pub async fn merchants(&self, wanted: &BTreeSet<String>) -> HashMap<String, String> {
        // 商家那个接口不收 corpId（它按 id 直查），传空串即可。
        self.resolve("", Domain::Merchant, wanted).await
    }

    /// 两个域共用的那套：整体过期 → 算缺失 → 冷却判断 → 批量补齐 → 取答案。
    ///
    /// **返回值里只有解析出名字的那些** —— 「查无此人」和「调不通」都不在里面，
    /// 调用方一律回落显示 ID。两者的区别只落在日志上（见下面的 `warn`），
    /// 不落在返回值上：页面对它们的处置是同一个。
    async fn resolve(
        &self,
        corp: &str,
        domain: Domain,
        wanted: &BTreeSet<String>,
    ) -> HashMap<String, String> {
        let epoch = self.generation();
        let mut names = self.names.lock().await;
        // TTL 到期**整体清空**（两张表一起）。判据是「纪元变了」而不是「距上次建表
        // 过了多久」—— 两者在这里必须是同一个数，否则代数和这两张表会各走各的。
        if names.epoch != epoch {
            names.tables = Default::default();
            names.epoch = epoch;
        }
        let slot = domain as usize;
        let missing: Vec<String> = wanted
            .iter()
            .filter(|id| !names.tables[slot].contains_key(*id))
            .cloned()
            .collect();
        // 冷却期内直接回落：不发请求、不打日志（那条 warn 刚打过）。见 `FAILURE_COOLDOWN`。
        if !missing.is_empty() && Instant::now() >= names.cooldown[slot] {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            match self.upstream.lookup(corp, domain, &missing).await {
                // 上游答了：找到的记名字，没答的记 `None` —— 那是**数据常态**，不告警。
                Ok(answered) => names.tables[slot].extend(answered),
                // Nacos 不可用 / 服务调不通 —— **可修的运维故障，要 `warn`**，
                // 且**绝不进负缓存**：一次抖动不该把所有人钉住一整个 TTL。
                Err(error) => {
                    names.cooldown[slot] = Instant::now() + FAILURE_COOLDOWN;
                    tracing::warn!(
                        %error,
                        ids = missing.len(),
                        "{}查询失败，本次回落显示 ID",
                        domain.label()
                    );
                }
            }
        }
        wanted
            .iter()
            .filter_map(|id| Some((id.clone(), names.tables[slot].get(id)?.clone()?)))
            .collect()
    }

    /// 拼进响应缓存数据戳的那个数 —— 它一变，旧响应全部作废。
    ///
    /// **= 进程起来之后走过了几个 TTL，一个纯时间函数。** 这一点是承重的：
    ///
    /// ⚠️ 代数曾经是个「在补齐时 +1」的计数器，那是**循环依赖**。
    /// 代数进的是响应缓存的数据戳，而缓存**命中时 handler 根本不会跑**
    /// （`cache::cached` 在 `next.run` 之前就返回了）—— 于是同一组筛选一直命中旧响应
    /// → 补齐永远没机会跑 → 代数永远不动 → 缓存永远不失效。名册于是
    /// 再也刷新不了，只能等夜里跑批改了库里的戳。「改名后几分钟内页面跟着变」直接落空。
    ///
    /// 纯时间函数没有这个问题：谁都不用调用它，时间自己会走。
    /// 它同时天然满足两条硬要求 —— 只在 TTL 整体过期时递增，
    /// 映射因按需填充而增长时不动。
    pub fn generation(&self) -> u64 {
        (self.born.elapsed().as_millis() / self.ttl.as_millis().max(1)) as u64
    }

    /// 预填充模式：给两个域各一张上游的假答案表，表里没有的 ID 即「查无此人」。
    #[cfg(test)]
    pub(super) fn canned(employees: &[(&str, &str)], ttl: Duration) -> Arc<Self> {
        Self::canned_both(employees, &[], ttl)
    }

    #[cfg(test)]
    pub(super) fn canned_both(
        employees: &[(&str, &str)],
        merchants: &[(&str, &str)],
        ttl: Duration,
    ) -> Arc<Self> {
        let table = |rows: &[(&str, &str)]| {
            rows.iter()
                .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
                .collect()
        };
        Arc::new(Self::with_upstream(
            Upstream::Canned([table(employees), table(merchants)]),
            ttl,
        ))
    }

    /// 至今向上游发起过几次批量查询。
    #[cfg(test)]
    pub(super) fn lookups(&self) -> usize {
        self.lookups.load(Ordering::Relaxed)
    }
}

impl Upstream {
    /// 一次批量查询。返回的每个 ID 都有答案：`Some(名字)` 或 **`None` = 上游说查无此人**。
    /// 调不通一律走 `Err`，由调用方决定不进负缓存。
    async fn lookup(
        &self,
        corp: &str,
        domain: Domain,
        ids: &[String],
    ) -> crate::Result<HashMap<String, Option<String>>> {
        match self {
            Self::Service {
                discovery,
                services,
                http,
            } => {
                let service = &services[domain as usize];
                let label = domain.label();
                let mut found = HashMap::new();
                for batch in ids.chunks(domain.batch()) {
                    // 每一批各选一次实例：实例列表按 `cacheMillis` 在后台刷新，
                    // 用最新那份没坏处，也顺手把负载摊开。
                    let instance = discovery
                        .pick(service)
                        .ok_or_else(|| format!("Nacos 尚无服务 `{service}` 的健康实例"))?;
                    let request = match domain {
                        Domain::Employee => http
                            .post(format!("{instance}{EMPLOYEE_PATH}"))
                            .header(reqwest::header::CONTENT_TYPE, "application/json")
                            .body(serde_json::to_string(
                                &serde_json::json!({"corpId": corp, "userIdSet": batch}),
                            )?),
                        Domain::Merchant => {
                            let mut url =
                                reqwest::Url::parse(&format!("{instance}{MERCHANT_PATH}"))?;
                            // 逗号分隔那一种写法（服务端 `@RequestParam List<Long>`
                            // 两种都收）。**不手拼查询串** —— 编码交给 `Url`。
                            url.query_pairs_mut()
                                .append_pair("ids", &batch.join(","))
                                .append_pair("isDeleted", MERCHANT_NOT_DELETED);
                            http.get(url)
                        }
                    };
                    let response = response_of(request, label, service).await?;
                    let (status, body) = response;
                    if !status.is_success() {
                        return Err(format!(
                            "{label} `{service}` 返回 HTTP {status}（{instance}）"
                        )
                        .into());
                    }
                    let answer: Envelope = serde_json::from_str(&body)
                        .map_err(|e| format!("{label} `{service}` 的应答无法解析：{e}"))?;
                    // ⚠️ **`code != 1` 或 `data` 缺席都当「调不通」**，绝不当成
                    // 「这一批全部查无此人」—— 后者会把他们负缓存住一整个 TTL，
                    // 而那是**失败**不该有的待遇。空 map 才是合法的「全都没查到」
                    // （服务端对查不到的 ID 是不放进 map，不是报错）。判据见 [`RESULT_OK`]。
                    let data = answer
                        .data
                        .filter(|_| answer.code == Some(RESULT_OK))
                        .ok_or_else(|| {
                            format!(
                                "{label} `{service}` 应答异常：code={:?}（成功是 {RESULT_OK}）\
                                 message={:?}",
                                answer.code, answer.message
                            )
                        })?;
                    for id in batch {
                        // map 里缺 key = 不存在或已删除。空名字跟没有名字一样没用，
                        // 一并当查不到，回落显示 ID。
                        let name = data
                            .get(id)
                            .and_then(|value| match domain {
                                // 账号域回的是对象，取 `name`；商家域直接就是店名。
                                Domain::Employee => value.get("name").and_then(Value::as_str),
                                Domain::Merchant => value.as_str(),
                            })
                            .map(str::to_owned)
                            .filter(|name| !name.trim().is_empty());
                        found.insert(id.clone(), name);
                    }
                }
                Ok(found)
            }
            #[cfg(test)]
            Self::Canned(tables) => Ok(ids
                .iter()
                .map(|id| (id.clone(), tables[domain as usize].get(id).cloned()))
                .collect()),
        }
    }
}

/// 发一次请求，回 `(状态码, 正文)`。两个域共用 —— 差异都在请求构造那一步。
async fn response_of(
    request: reqwest::RequestBuilder,
    label: &str,
    service: &str,
) -> crate::Result<(reqwest::StatusCode, String)> {
    let response = request
        .send()
        .await
        .map_err(|e| format!("{label} `{service}` 调用失败：{e}"))?;
    let status = response.status();
    Ok((status, response.text().await.unwrap_or_default()))
}

/// 两个域共用的 `Result<Map<K, V>>` 外层包装。
///
/// 成功判据是 **`code == 1` 且 `data` 非空**，理由见 [`RESULT_OK`]。
/// `data` 的值用 `Value` 装着，因为两个域的 `V` 不同：账号域是
/// `WorkWechatEmpCorpInfoRespDTO`（对象，取 `name`），商家域直接是店名字符串。
///
/// ⚠️ **只取名字那一个字段。** 账号域那个 DTO 有二十来个字段
/// （`position` / `mainDepartment` / `deptIds` / `city` …），
/// **端口上每多一个死字段，就是向未来每一个适配器收一次税**
/// （`CONTEXT.md` 的领域契约同一条规矩）。
#[derive(Deserialize)]
struct Envelope {
    code: Option<i64>,
    message: Option<String>,
    /// key = 那个域的 ID。**查不到的 ID 根本不出现**。
    data: Option<HashMap<String, Value>>,
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
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const CORP: &str = "ww0123456789abcdef";
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

    fn ids(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    const LONG: Duration = Duration::from_secs(300);

    /// 预填充：上游认识的人拿到姓名，不认识的记成**负缓存**（`None`），
    /// 两者都不再重查。
    #[tokio::test]
    async fn a_name_is_resolved_once_and_a_miss_is_remembered() {
        let roster = Roster::canned(&[("zhang.san", "张三")], LONG);
        let first = roster.employees(CORP, &ids(&["zhang.san", "ghost"])).await;
        assert_eq!(first.get("zhang.san").unwrap(), "张三");
        assert!(!first.contains_key("ghost"), "查无此人不该出现在结果里");
        assert_eq!(roster.lookups(), 1);

        // 第二次：`ghost` 已经在负缓存里，**不许再发起一次查询**。
        let again = roster.employees(CORP, &ids(&["zhang.san", "ghost"])).await;
        assert_eq!(again.get("zhang.san").unwrap(), "张三");
        assert!(!again.contains_key("ghost"));
        assert_eq!(
            roster.lookups(),
            1,
            "负缓存没生效，同一批不存在的 ID 又查了一遍"
        );
    }

    /// 代数：**只在 TTL 整体过期时递增**；按需填充让映射增长时不动。
    ///
    /// 后半条是承重的 —— 映射每个请求都在长，代数跟着长的话，
    /// 响应缓存的数据戳每个请求都变，缓存永不命中。
    #[tokio::test]
    async fn the_generation_moves_only_when_the_whole_table_expires() {
        let roster = Roster::canned(&[("a", "甲"), ("b", "乙")], LONG);
        assert_eq!(roster.generation(), 0);
        roster.employees(CORP, &ids(&["a"])).await;
        assert_eq!(roster.generation(), 0);
        // 映射增长（多了 b）—— 代数**不得**变。
        roster.employees(CORP, &ids(&["a", "b"])).await;
        assert_eq!(roster.generation(), 0, "按需填充让映射增长时代数不得递增");
        assert_eq!(
            roster.lookups(),
            2,
            "第二次确实补了新 ID，否则上一条断言是空的"
        );
    }

    /// 代数**不靠谁来调用**，时间到了自己就走 —— 承重，理由见 [`Roster::generation`]
    /// 的注释（响应缓存命中时 handler 根本不会跑）。
    ///
    /// 真睡一小会儿，不引 `tokio` 的 `test-util`：睡眠只会超时不会提前，所以
    /// 「睡过 TTL 之后代数变了」不会偶发失败；前半段是两次内存查表，微秒级，
    /// 离 200ms 的 TTL 远得很。
    #[tokio::test]
    async fn the_generation_advances_on_its_own_without_any_lookup() {
        let ttl = Duration::from_millis(200);
        let roster = Roster::canned(&[("a", "甲")], ttl);
        roster.employees(CORP, &ids(&["a"])).await;
        assert_eq!((roster.generation(), roster.lookups()), (0, 1));

        // 一次调用都没有，光是时间过去就该翻代数 —— 于是响应缓存的戳跟着变，
        // handler 才有机会重新跑一遍。
        tokio::time::sleep(ttl + Duration::from_millis(50)).await;
        assert_eq!(roster.generation(), 1, "代数不会自己走，名册就再也刷新不了");
        assert_eq!(roster.lookups(), 1, "代数推进不该自己去查上游");

        // 纪元变了 ⇒ 下一次进门整表清空重查。
        roster.employees(CORP, &ids(&["a"])).await;
        assert_eq!(roster.lookups(), 2);
    }

    /// 上游调不通之后进**冷却**：堵在锁后面的那些请求直接回落，不是一人一个超时。
    ///
    /// 没有冷却的话，失败不进负缓存 ⇒ 缺失集合一直非空 ⇒ 每个醒来的请求都自己再发
    /// 一次，而它们**各握着一条只读连接**在排队。
    #[tokio::test]
    async fn a_failing_upstream_cools_down_instead_of_retrying_per_request() {
        let roster = unreachable_roster();
        for _ in 0..5 {
            assert!(
                roster
                    .employees(CORP, &ids(&["zhang.san"]))
                    .await
                    .is_empty()
            );
        }
        assert_eq!(roster.lookups(), 1, "冷却期内不该再打上游");
    }

    /// 上游调不通（Nacos 没有健康实例）**绝不进负缓存** —— 一次抖动不该把所有人
    /// 钉在「显示 ID」上整整一个 TTL。
    #[tokio::test]
    async fn an_unreachable_upstream_does_not_poison_the_negative_cache() {
        let roster = unreachable_roster();
        assert!(
            roster
                .employees(CORP, &ids(&["zhang.san"]))
                .await
                .is_empty()
        );
        assert_eq!(roster.lookups(), 1);
        // 冷却过去之后还会再试 —— 这证明它**没有**被记成「查无此人」。
        // （把冷却截止拨回当下，不为这一条真睡十秒。）
        roster.names.lock().await.cooldown = [Instant::now(); 2];
        assert!(
            roster
                .employees(CORP, &ids(&["zhang.san"]))
                .await
                .is_empty()
        );
        assert_eq!(roster.lookups(), 2, "调不通被当成查无此人记进负缓存了");
    }

    /// 协议：账号域 v2 批量查询的请求形状、`userId → name` 的回填、
    /// **map 里缺 key = 查无此人**（进负缓存），以及**分批**（服务端 `@Size(max=100)`）。
    #[tokio::test]
    async fn the_employee_batch_query_matches_the_account_app_contract() {
        // 101 个 ID ⇒ 必须切成 100 + 1 两批，否则服务端整批拒收。
        let wanted: Vec<String> = (0..101).map(|n| format!("user{n:03}")).collect();
        let (base, server) = scripted(vec![
            (
                EMPLOYEE_PATH,
                200,
                json!({"code": 1, "message": "ok", "data": {
                    "user000": {"name": "张三", "position": "客服"},
                    // 空名字跟没名字一样没用 —— 当查不到，回落显示账号。
                    "user001": {"name": "  "},
                }}),
            ),
            (EMPLOYEE_PATH, 200, json!({"code": 1, "data": {}})),
        ]);
        let hosts: Arc<Hosts> = Arc::default();
        hosts
            .write()
            .unwrap()
            .insert("account-app".into(), vec![base]);
        let roster = Arc::new(Roster::with_upstream(service_upstream(hosts), LONG));

        let names = roster
            .employees(CORP, &wanted.iter().cloned().collect())
            .await;
        assert_eq!(names.get("user000").unwrap(), "张三");
        assert!(!names.contains_key("user001"), "空名字该当查不到");
        assert_eq!(names.len(), 1);

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2, "101 个 ID 必须切成两批：{requests:?}");
        assert_eq!(
            requests[0].0,
            format!("POST {EMPLOYEE_PATH} HTTP/1.1"),
            "路径必须是 v2（v1 不收 corpId，会静默兜底成默认主体）"
        );
        let first: Value = serde_json::from_str(&requests[0].1).unwrap();
        assert_eq!(first["corpId"], CORP);
        assert_eq!(first["userIdSet"].as_array().unwrap().len(), EMPLOYEE_BATCH);
        let second: Value = serde_json::from_str(&requests[1].1).unwrap();
        assert_eq!(second["userIdSet"], json!(["user100"]));

        // 回填过的 ID 一个都不再查 —— 查到的和「查无此人」的都算数。
        roster
            .employees(CORP, &wanted.iter().cloned().collect())
            .await;
        assert_eq!(roster.lookups(), 1, "第二轮不该再打上游");
    }

    /// `code != 1` 或 `data` 缺席 = **调不通**，不是「这一批全部查无此人」。
    ///
    /// 这两者混淆的代价是不对称的：当成查无此人就会把他们负缓存住一整个 TTL，
    /// 而那是失败不该有的待遇。空 map **配 `code == 1`** 才是合法的「全都没查到」。
    ///
    /// 第二种形态（`code != 1` 却带着 `{}`）单独钉住：上游失败时 `data` 到底是
    /// `null` 还是 `{}` 没有权威答案（`Result` 来自外部依赖，源码不在手上），
    /// 所以两道判据缺一不可 —— 只看 `data` 的话，这一条会静默把人记进负缓存。
    #[tokio::test]
    async fn a_response_that_is_not_code_one_is_a_failure_not_a_batch_of_misses() {
        for bad in [
            json!({"code": 500, "message": "内部错误", "data": null}),
            json!({"code": 500, "message": "内部错误", "data": {}}),
        ] {
            let (base, server) = scripted(vec![(EMPLOYEE_PATH, 200, bad.clone())]);
            let hosts: Arc<Hosts> = Arc::default();
            hosts
                .write()
                .unwrap()
                .insert("account-app".into(), vec![base]);
            let roster = Arc::new(Roster::with_upstream(service_upstream(hosts), LONG));

            assert!(roster.employees(CORP, &ids(&["a"])).await.is_empty());
            server.join().unwrap();
            // 没进负缓存：冷却一过就会再试（拨回冷却，不真睡十秒）。
            roster.names.lock().await.cooldown = [Instant::now(); 2];
            assert!(roster.employees(CORP, &ids(&["a"])).await.is_empty());
            assert_eq!(roster.lookups(), 2, "{bad} 被当成查无此人记进负缓存了");
        }
    }

    /// 协议：商家域 `getMerchantNameByIds` 的请求形状（GET + `ids` + `isDeleted=0`）、
    /// `merchantId → nick_name` 的回填、**缺 key = 查无此商家**（进负缓存），以及分批。
    ///
    /// ⚠️ 商家 ID 全程是**字符串**，不解析成整数 —— `merchant_id` 是 `BIGINT UNSIGNED`，
    /// 取数那边本来就 `CAST(... AS CHAR)` 出来（免得前端丢精度）。这一条用一个
    /// 超出 `i64` 的值钉住：换成 `u64`/`i64` 中转就会溢出或丢精度。
    #[tokio::test]
    async fn the_merchant_name_query_matches_the_merchant_app_contract() {
        const HUGE: &str = "18446744073709551615";
        // 501 个 ⇒ 切成 500 + 1 两批（上限是我们自己定的，服务端没有条数兜底）。
        let mut wanted: Vec<String> = (0..500).map(|n| format!("{}", 10_000_000 + n)).collect();
        wanted.push(HUGE.into());
        let (base, server) = scripted(vec![
            (
                MERCHANT_PATH,
                200,
                json!({"code": 1, "data": {"10000000": "甲商家", "10000001": "   "}}),
            ),
            (
                MERCHANT_PATH,
                200,
                json!({"code": 1, "data": {HUGE: "大号商家"}}),
            ),
        ]);
        let hosts: Arc<Hosts> = Arc::default();
        hosts
            .write()
            .unwrap()
            .insert("merchant-app".into(), vec![base]);
        let roster = Arc::new(Roster::with_upstream(service_upstream(hosts), LONG));

        let names = roster.merchants(&wanted.iter().cloned().collect()).await;
        assert_eq!(names.get("10000000").unwrap(), "甲商家");
        assert!(!names.contains_key("10000001"), "空店名该当查不到");
        assert_eq!(
            names.get(HUGE).unwrap(),
            "大号商家",
            "超 i64 的 ID 必须原样对回来"
        );
        assert_eq!(names.len(), 2);

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2, "501 个 ID 必须切成两批：{requests:?}");
        let (line, body) = &requests[0];
        assert!(line.starts_with(&format!("GET {MERCHANT_PATH}?")), "{line}");
        assert!(body.is_empty(), "这是 GET，不该有正文");
        // 逗号是 `Url` 编码过的（%2C），服务端解码后按逗号切 —— 断言时解回来比较。
        let query = line
            .split('?')
            .nth(1)
            .unwrap()
            .trim_end_matches(" HTTP/1.1");
        let ids = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("ids="))
            .unwrap()
            .replace("%2C", ",");
        assert_eq!(ids.split(',').count(), MERCHANT_BATCH);
        assert!(ids.starts_with("10000000,10000001,"), "{ids}");
        // ⚠️ 这一位就是「已注销商家显不显示」那个业务假设的落点。
        assert!(query.contains("isDeleted=0"), "{query}");
        assert!(requests[1].0.contains(HUGE), "{:?}", requests[1].0);

        // 回填过的 ID 一个都不再查 —— 查到的和「查无此商家」的都算数。
        // （`lookups` 数的是「补齐了几轮」，不是几个批次：上面那一轮两批算一次。）
        assert_eq!(roster.lookups(), 1);
        roster.merchants(&wanted.iter().cloned().collect()).await;
        assert_eq!(roster.lookups(), 1, "第二轮不该再打上游");
    }

    /// 两个域的**冷却互不连累** —— 它们是两个服务，账号域挂了不该让商家名也停摆。
    #[tokio::test]
    async fn one_domain_failing_does_not_cool_down_the_other() {
        // 只注册商家域的实例：账号域 `pick` 必然拿不到，走「调不通」。
        let (base, server) = scripted(vec![(
            MERCHANT_PATH,
            200,
            json!({"code": 1, "data": {"1": "甲商家"}}),
        )]);
        let hosts: Arc<Hosts> = Arc::default();
        hosts
            .write()
            .unwrap()
            .insert("merchant-app".into(), vec![base]);
        let roster = Arc::new(Roster::with_upstream(service_upstream(hosts), LONG));

        assert!(
            roster
                .employees(CORP, &ids(&["zhang.san"]))
                .await
                .is_empty()
        );
        // 账号域刚进冷却，商家域必须照常查得到。
        let names = roster.merchants(&ids(&["1"])).await;
        assert_eq!(names.get("1").unwrap(), "甲商家");
        server.join().unwrap();
    }

    /// Nacos 一个健康实例都没有的名册 —— 每次查询都走「调不通」那一路。
    fn unreachable_roster() -> Arc<Roster> {
        Arc::new(Roster::with_upstream(
            service_upstream(Arc::default()),
            LONG,
        ))
    }

    /// 指向给定实例表的生产形态上游。传 `Arc::default()` 就是「一个健康实例都没有」。
    fn service_upstream(hosts: Arc<Hosts>) -> Upstream {
        Upstream::Service {
            discovery: Discovery { hosts },
            services: ["account-app".into(), "merchant-app".into()],
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
        }
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

    /// 协议：服务端没开鉴权时 `[roster].username` 留空 ⇒ **一次登录都不发**，
    /// 实例查询里也没有 `accessToken` 参数。脚本里没有 `LOGIN` 这一条，
    /// 真发了登录请求假 Nacos 会当场 panic。
    #[tokio::test]
    async fn an_unauthenticated_nacos_skips_login_and_the_token_parameter() {
        let healthy = json!([{"ip": "10.0.0.1", "port": 8080, "healthy": true}]);
        let (base, server) = scripted(vec![hosts(healthy.clone()), hosts(healthy)]);
        let anonymous = RosterSecrets {
            username: String::new(),
            password: String::new(),
        };
        Discovery::start(&config(&base), &anonymous).await.unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2, "只该有两次实例查询：{requests:?}");
        for (line, _) in &requests {
            assert!(line.starts_with(&format!("GET {LIST}?")), "{line}");
            assert!(!line.contains("accessToken"), "{line}");
        }
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
