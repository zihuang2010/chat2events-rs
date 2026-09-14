//! 配置加载。
//!
//! 分两个文件，密钥单独隔离：
//!   config.toml   调参与端点，进 git，谁都能读
//!   secrets.toml  密钥，0600，不进 git
//!
//! 读不到、字段缺、类型不对，一律直接崩 —— 配置错误要在进程起来的第一秒暴露，
//! 而不是跑到落库那步才炸。
//!
//! **所有键必填，代码里没有默认值。** 曾经 7 个键有 serde 默认值，且与 config.toml
//! 里的显式值逐字相同 —— 默认分支是死代码，只剩「toml 丢了字段照样起来」这一个
//! 效果，而那正是上面那条规矩要禁止的事。

use async_openai::types::chat::ReasoningEffort;
use serde::{Deserialize, de::DeserializeOwned};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Deserialize)]
pub struct Config {
    pub daily: DailyConfig,
    pub ingest: IngestConfig,
    pub extract: ExtractConfig,
    pub classify: ClassifyConfig,
    pub llm: LlmSection,
    pub mysql: MysqlConfig,
    pub log: LogConfig,
}

#[derive(Deserialize)]
pub struct LogConfig {
    /// error / warn / info / debug / trace，也接受 EnvFilter 的分模块写法。
    /// ⚠️ 调到 debug 不会让 async-openai 更啰嗦，只对 reqwest/hyper 有效果 ——
    /// 端点侧的埋点情况见 `llm.rs` 里 create 调用处的注释。
    /// 环境变量 RUST_LOG 若存在会覆盖它 —— 临时排障不用改配置文件。
    pub level: String,
}

/// 起日志 —— **全部入口共用这一处**（都经 `boot::Boot::load`）。
///
/// 日志一律走 stderr。**stdout 全程不写一个字节** —— 抽取结果由 ⑦ 落 MySQL，
/// 跑批没有「把结果打出来」这条路径。写 stderr 是为了让 `2> run.log` 能单独收日志，
/// 且重定向到文件/journald 时不掺 ANSI 颜色码。
///
/// ⚠️ **时间戳必须是本地时间**：`tracing_subscriber` 默认的 `SystemTime` 打的是 UTC，
/// 在 UTC+8 上每一行都比墙钟少 8 小时 —— 排障时拿日志时间去对 `ps` / MySQL 的
/// `NOW()` 会整整差一个时区。`ChronoLocal::rfc_3339()` 带 `+08:00` 后缀，
/// 换台机器也不用猜它是哪个时区的。
/// （库里时间列的时区问题是另一回事，见 `examples/tzcheck.rs`。）
pub fn init_logging(cfg: &LogConfig) {
    use std::io::IsTerminal;
    use tracing_subscriber::{EnvFilter, fmt};

    // RUST_LOG 存在就听它的（临时排障不用改文件），否则走 config.toml
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.level));

    fmt()
        .with_env_filter(filter)
        .with_timer(fmt::time::ChronoLocal::rfc_3339())
        .with_writer(std::io::stderr)
        // 重定向到文件或 journald 时别写 ANSI 颜色码，那是噪声
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// 跑批那一轮本身的参数 —— 不属于任何单个阶段，所以不塞进 `[ingest]`。
#[derive(Deserialize)]
pub struct DailyConfig {
    /// **整轮**的墙钟预算，从 `daily::run` 进门开始算，① 和 ①② 共用同一份。
    ///
    /// 到点之后**不再启动**新的下载 / 新的群，在飞的跑完就收工，进程以非零码退出。
    /// 不是硬砍：砍在半路会让一个群只写进去一半（承重不变量 2 要求两个分片同一个
    /// 事务），而"不再开新的"天然落在事务边界上。
    ///
    /// ⚠️ **这是失控护栏，不是调优旋钮。** 正常一轮是分钟级；它存在只为了让
    /// 「OSS 半死不活、每个文件都读到一半断」那种情况在几小时内收场，
    /// 而不是把 cron 挂到第二天。没跑完的群下一轮会重新拉 —— 只要
    /// `lookback_days ≥ 2`，漏掉的那天下一轮还在窗口里。
    pub round_deadline_secs: u64,
}

/// ① 摄取 —— 镜像（`mirror/`）和读取（`ingest/`）共用这一段。
#[derive(Deserialize)]
pub struct IngestConfig {
    /// 本地 raw 区 = OSS 的字节级镜像，「已拉到第几字节」= 文件大小。
    /// 目录布局与理由见 `ingest/layout.rs`（布局的唯一权威）。
    pub raw_root: PathBuf,

    /// 回看窗口 N：跳过当天和昨天，读 `[T-(N+1), T-2]`，这 N 天就是非冻结区。
    /// ⚠️ N=2 时「周五提问、周一回复」永远拼不起来 —— 没有任何一次运行会同时读到
    /// 周五和周一，该事件会永久算作未回复，而损失系统性落在**周五值班的人**头上。
    /// 判据是「跨 2 天以上才闭合」的占比，且**要按周几分别看**，> 5% 立刻调 4。
    pub lookback_days: u32,

    /// OSS 服务端点（不含 bucket）、bucket 和签名地域；直连 OSS，不经过 CDN。
    pub oss: OssConfig,

    /// 本地 raw 区保留几个月，更老的月目录整个删掉（[`crate::stage::ingest::prune`]）。
    ///
    /// 保留起点锚在**窗口**上（保留「窗口最早月往前推 N-1 个月」起的全部月目录），
    /// 所以 `lookback_days` 配多大都不会把本轮要读的月份删掉。
    ///
    /// ⚠️ **它只约束跑批的输入，不再是 webUI 下钻的可见范围** —— 原文渲染快照在
    /// 抽取时就落进了 `b_merchant_group_event.source_messages`，工作台一个文件都不读。
    /// 留 2 个月是为了「抽取失败重跑」和 `backfill` 补跑还读得到原文，
    /// 降到 1 只留窗口月，那两件事就做不了了。
    ///
    /// ⚠️ **删除粒度是整个月目录**（一个群一个月一个文件，删不了半个），所以 M 月的
    /// 原文在 M+2 月 4 号那轮跑批消失 —— 月末的消息只活 33 天，月初的活 63 天。
    ///
    /// N=2 时磁盘上界 ≈ 2 个月（1000 群约 36 GB）。此前**没有任何清理**，
    /// 一年约 216 GB 且永不回落。
    pub raw_retention_months: u32,

    /// 同时在飞的月文件下载数。跟 `segment_msgs` / `room_concurrency`
    /// 一个规矩：这类值必须由部署环境明确给出，代码里没有默认值。
    pub mirror_concurrency: usize,

    /// 同时读取、抽取和保存的群数；群内段仍串行。打标使用独立并发额度。
    pub room_concurrency: usize,
}

/// ③ 抽取 —— 只有一个旋钮，而且是省钱的那种。
#[derive(Deserialize)]
pub struct ExtractConfig {
    /// 一个群一天切成 `ceil(n / segment_msgs)` 段。**省钱旋钮，不是质量旋钮**
    /// ：切几段都不产生接缝（段之间串行传便签），它只是省掉
    /// 「拿整群去试、注定被截断」那一次调用。无默认值，缺失即报错。
    ///
    /// 它还兼着**便签的保留窗口**：「上一整段都没动静就撤下」复用的就是这个数，
    /// 不另发明一个。两者眼下量级相同才合用 —— 换了输出预算大得多的模型、
    /// 段长跳到几千时要拆成独立常量。
    pub segment_msgs: usize,
}
/// ⑤ 分类 —— 独立批次并发与结果缓存。
#[derive(Deserialize)]
pub struct ClassifyConfig {
    /// 全局最多同时处理的打标批次数，也约束消费者持有的群数和 channel 容量。
    pub concurrency: usize,
    /// `<cache_dir>/<taxonomy_version>-<策略指纹>.sqlite`，摘要 hash 对应标签全集；旧 NDJSON 首次导入。
    ///
    /// **这是承重件不是优化**：模型打标不保证同输入同输出，而非冻结区每天重写
    /// `[T-3, T-2]`、同一批 event 会被反复打标 —— 没有跨运行的持久缓存，报表就会
    /// 抖动而非修正（承重不变量 1）。目录建不出来时进程在启动期就崩，不等到第一次打标。
    ///
    /// 策略指纹包含词表、提示词与模型请求配置；每个文件只允许一个进程持有写锁。
    pub cache_dir: PathBuf,
}

/// `[llm]` —— **共用键写在节头下，两队各自的三个键在子节里**。
///
/// ③ 抽取和 ⑤ 打标是**两个不同的模型**（`[llm.extract]` / `[llm.classify]`）。
/// 拆节的判据不是「它属不属于模型」，而是「**换一队模型时会不会想改它**」：
///   * `model` / `base_url` / `max_tokens` 跟着**任务**走 —— 抽取要整段对话的事件
///     列表（输出上限贴着模型收得下的最大值给），打标只吐几十个小对象（贴着实际
///     需求给），两者差一个量级。这三个进 [`LlmModel`]。
///   * 其余跟着**客户端**走：连接、响应与逻辑调用预算属于传输处置，`reasoning_effort` 两队都要关，
///     `temperature = 0` 是两条线共同的确定性要求。留在这里。
///
/// **全写两遍也能跑，但那会给 `reasoning_effort` 和 `temperature` 各造一个副本，
/// 而这两个字段配错的症状都是静默的** —— 前者是一行没人看的 warn 加一张翻倍的账单，
/// 后者会让打标结果缓存（承重不变量 1）从「同 summary 同答案」退成偶然。
/// 最不该有副本的正是它们。
#[derive(Deserialize)]
pub struct LlmSection {
    /// ⚠️ 绝对不要给这个字段加默认值：ReasoningEffort 自带的 Default 是 Medium ——
    /// 配置里一省掉就变成"开着推理"。实测 qwen3.8-flash 默认开推理，答一个"2"
    /// 要烧 41 个 reasoning token；抽取和打标都不需要它。宁可缺失即报错，
    /// 也不给一个危险的隐式值。注意 "minimal" 不等于关（实测仍有 13 tokens），
    /// 只有 "none" 是真关。
    pub reasoning_effort: ReasoningEffort,

    /// 抽取要可复现，不是创作 —— 生产恒为 0.0。
    /// **打标那条线还额外靠它**：结果缓存是承重件（承重不变量 1），
    /// temperature 一非零，「同 summary 同答案」就从高概率退成偶然。
    pub temperature: f32,

    /// 单次尝试的超时。实测单段调用 118~168s（3742 条样本、约 370 条/段），留 2 倍余量。
    /// ⚠️ 这是【每次尝试】的上限，不是总耗时：底层默认还会重试 3 次，
    /// 最坏情况墙钟是这个值的 4 倍。要卡总时长得在调用方再包一层。
    pub timeout_secs: u64,

    /// 一次逻辑调用的总预算，包含 SDK 等待、传输重试与分类的坏运气重发。
    pub request_timeout_secs: u64,

    /// 只管 TCP+TLS 握手。单独设是为了让"端点连不上"几秒内失败，
    /// 而不是耗满上面那个按分钟计的整体超时。
    pub connect_timeout_secs: u64,

    /// ③ 抽取那一队。
    pub extract: LlmModel,

    /// ⑤ 打标那一队 —— `recompute` 重打标和 `taxonomy review` 试打也走这一份。
    pub classify: LlmModel,
}

/// 一队的模型参数。**只有这三个跟着队走**，其余在 [`LlmSection`] 上共用。
#[derive(Deserialize)]
pub struct LlmModel {
    pub model: String,
    pub base_url: String,

    /// ⚠️ **两件事同时跟着它变**：模型收得下多少（qwen3.8-flash 收 64000，
    /// qwen-plus 只到 32768，超了直接报 range 错），和**这一队跑飞时靠谁喊停**。
    /// 后者是承重的，论证在 `config.toml` 的 `[llm.classify]` 那段 ——
    /// 跟 `segment_msgs` / `mirror_concurrency` 一个规矩，承重数字的理由住在 config.toml。
    /// 两队的关系由 [`load_from_dir`] 里的断言守着。
    pub max_tokens: u32,
}

/// 连接池参数。连接 URL 带密码，不在这里 —— 见 [`MysqlSecrets`]。
#[derive(Deserialize)]
pub struct MysqlConfig {
    /// 池上限。跑批是"调一次 LLM、落一次库"，并发度由外层任务数决定，几条就够。
    pub max_connections: u32,

    /// 池子满了、等一条空闲连接的上限。超时报错，不无限挂着。
    pub acquire_timeout_secs: u64,
}

/// 会话时区。**sqlx 建连接时会把会话设成 `+00:00`**，而 `schema.sql` 里
/// `gmt_created_time` / `gmt_modified_time` 是 `DEFAULT CURRENT_TIMESTAMP` ——
/// 那个默认值按**会话**时区求值，于是这两列比业务本地时间少 8 小时，
/// 且和别的客户端（走服务器全局时区）插进来的行不一致。
///
/// 用固定偏移不用 `'Asia/Shanghai'`：命名时区要 MySQL 导入过 tz 表（多数部署没有），
/// 而中国不用夏令时，`+08:00` 与它恒等。
///
/// ⚠️ **这不影响任何业务时间列。** `first_msg_time` 那几列是 `DATETIME`（MySQL 对它
/// 不做时区转换）且由代码显式绑 `NaiveDateTime` —— 那条链上本来就没有会话时区的事。
/// 这里修的只是 MySQL 自己算的那两个审计列。
pub(crate) const SET_SESSION_TZ: &str = "SET time_zone = '+08:00'";

/// 建连接池。
///
/// ⚠️ 用 `.connect()` 不是 `.connect_lazy()`：这里会立刻握一次手，库连不上就在启动
/// 第一秒炸，跟配置错误一个待遇 —— 而不是跑到落库那步才发现密码是错的。
pub async fn mysql_pool(cfg: &MysqlConfig, url: &str) -> Result<MySqlPool, sqlx::Error> {
    MySqlPoolOptions::new()
        .max_connections(cfg.max_connections)
        .acquire_timeout(Duration::from_secs(cfg.acquire_timeout_secs))
        // 池里每条连接都要拨一次 —— 会话变量是连接级的，只在建池时设一次管不到后开的连接。
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, SET_SESSION_TZ).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
}

#[derive(Deserialize)]
pub struct Secrets {
    pub llm: LlmSecrets,
    pub mysql: MysqlSecrets,
    pub oss: OssSecrets,
}

#[derive(Deserialize)]
pub struct OssConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
}

#[derive(Deserialize)]
pub struct OssSecrets {
    pub access_key_id: String,
    pub access_key_secret: String,
}

#[derive(Deserialize)]
pub struct LlmSecrets {
    pub api_key: String,
}

#[derive(Deserialize)]
pub struct MysqlSecrets {
    /// mysql://user:password@host:3306/dbname
    /// 整条 URL 都算密钥 —— 密码在里面，所以在 secrets.toml 而不是 config.toml。
    pub url: String,
}

/// 配置目录：默认当前目录，第一个命令行参数可覆盖（生产传 /etc/chat2events）。
/// 这条约定是配置契约的一部分，所以它住在这里 ——
/// `boot::Boot::from_args` 用它，于是全部入口共用，曾经多处逐字重复。
pub fn dir_from_args() -> PathBuf {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 从一个目录加载两份配置。生产传 /etc/chat2events，开发传当前目录。
pub fn load_from_dir(dir: &Path) -> (Config, Secrets) {
    let config: Config = load(&dir.join("config.toml"), false);
    for (name, value) in [
        (
            "ingest.mirror_concurrency",
            config.ingest.mirror_concurrency,
        ),
        ("ingest.room_concurrency", config.ingest.room_concurrency),
        ("classify.concurrency", config.classify.concurrency),
        ("extract.segment_msgs", config.extract.segment_msgs),
        (
            "ingest.raw_retention_months",
            config.ingest.raw_retention_months as usize,
        ),
    ] {
        assert!(value >= 1, "{name} 必须至少为 1，改 config.toml");
    }
    // **跨节的约束，`serde` 查不到** —— 缺字段它拦得住，两节之间的关系拦不住。
    // 小于这个和就会有群在落库那几毫秒里排队等连接，`acquire_timeout_secs` 一到，
    // 报出来的是「落库失败」—— 那个群整轮作废，而它的 token 已经烧完了。
    //
    // ⚠️ **打标侧要算两份，不是一份。** 这条此前是 `room_concurrency + classify.concurrency`
    // ——那低估了打标侧：`slots` 那道许可只包住 `update_event_labels`，而
    // `buffer_unordered(classify.concurrency)` 同时让**另一批**群在 `read_events` /
    // `finish_classification` 上各持一条连接。抽取侧 `room_concurrency` 个群握着
    // `write_room` 的事务，三者互不相让。
    //
    // 真实峰值是 `room_concurrency + 2 × classify.concurrency − 1`（同一个群不会
    // 同时在 `slots` 里和 `slots` 外）。**这里取整数不要那个 `−1`** —— 差一个连接，
    // 而 `−1` 要三行注释才说得清；护栏略微保守完全合格，精确得难懂不合格。
    //
    // 低估的断言比没有断言更糟：它给运维一个**会放行错误配置**的数字。
    // 小于这个数就会有群在落库那几毫秒里排队等连接，`acquire_timeout_secs` 一到，
    // 报出来的是「落库失败」—— 那个群整轮作废，而它的 token 已经烧完了。
    let peak = config.ingest.room_concurrency + 2 * config.classify.concurrency;
    assert!(
        config.mysql.max_connections as usize >= peak,
        "mysql.max_connections（{}）必须 ≥ ingest.room_concurrency（{}）\
         + 2 × classify.concurrency（{}）= {peak} —— 抽取持一份连接，打标持两份\
         （`slots` 内的 update 与 `slots` 外的 read/finish 是两批群），\
         小于这个和就会有群在落库时等不到连接，白烧一整轮的 token。改 config.toml",
        config.mysql.max_connections,
        config.ingest.room_concurrency,
        config.classify.concurrency
    );
    // **跨节的约束，`serde` 查不到。** 这条曾经是 `Llm::with_max_tokens` 的无条件钳位
    // （`Classifier::new` 里把打标那份压到 6000，config 写什么都不算数）—— 钳位的毛病是
    // 它把 config.toml 里那个数变成了谎言，而本文件第一条规矩是「配置错要在第一秒炸，
    // 不该被代码悄悄修正」。所以钳位改成拒绝。
    //
    // 它拦的是**唯一的现实失效模式**：把 `[llm.extract]` 整段抄到 `[llm.classify]` 底下
    // 只改 model —— 在「两节长得几乎一样」的结构里，这是最顺手的操作。
    // 抄过去 max_tokens 就是 64000，而打标一批 50 条 × 每条一个小对象正常不过 2000 token。
    // 上限一大，模型跑飞（strict JSON schema 下随机陷入重复生成，实测中招率约三分之一）
    // 就没有任何东西拦得住，只能安静生成到撞满 timeout_secs，再报一个无从下手的 Timeout。
    // 试打 870 条是 18 批，按这个中招率几乎每趟都要挂死几回。
    //
    // 用关系式不用绝对上限：绝对上限要在代码里再养一个魔数，而那正是搬进 config.toml 的
    // 那个数。关系式弱一些（63999 也过），但它恰好盖住整段复制这一种错法。
    assert!(
        config.llm.classify.max_tokens < config.llm.extract.max_tokens,
        "llm.classify.max_tokens（{}）必须小于 llm.extract.max_tokens（{}）—— \
         看起来是把 [llm.extract] 整段抄过去了。打标的输出预算要贴着实际需求给\
         （一批 50 条小对象，6000 已是 3 倍余量），跑飞才会秒撞 Truncated 让 \
         extract_retry 重发；给成抽取那个数，跑飞就只剩 timeout_secs 喊停，\
         每趟挂死几回。改 config.toml",
        config.llm.classify.max_tokens,
        config.llm.extract.max_tokens
    );
    let secrets_path = dir.join("secrets.toml");
    #[cfg(unix)]
    require_owner_only(&secrets_path);
    (config, load(&secrets_path, true))
}

/// `redact` = 这个文件里有密钥，**解析报错不许原样打出来**。
///
/// `toml` 的解析错误会回显出错那一行的源文本：
/// ```text
/// TOML parse error at line 2, column 33
/// 2 | api_key = "sk-SUPERSECRET-abc123
/// ```
/// 而唯一会有人碰 `secrets.toml` 的场合正是轮换密钥、粘歪一个引号的时候。
/// 0600 权限检查挡不住 stderr —— 它会落进 `run.log`、journald、cron 邮件、
/// CI 输出和终端 scrollback。`Secrets` 本身没有 `Debug` / `Serialize`，
/// `Llm` 也没有 `Debug`，这条路是仅剩的泄漏点。
pub(crate) fn load<T: DeserializeOwned>(path: &Path, redact: bool) -> T {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读不到 {}：{e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| {
        if redact {
            panic!(
                "解析失败 {}：详情已省略 —— toml 的报错会连同出错那一行的密钥原文一起打印。\
                 自己打开文件看第 {} 行附近",
                path.display(),
                e.span().map_or(0, |s| text[..s.start].lines().count()),
            )
        }
        panic!("解析失败 {}：{e}", path.display())
    })
}

/// 密钥文件必须 0600 —— 组或其他人有任何一位权限就拒绝加载（照 ssh 对私钥的规矩）。
#[cfg(unix)]
pub(crate) fn require_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("读不到 {}：{e}", path.display()))
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o077,
        0,
        "{} 权限过宽（{:o}），执行：chmod 600 {}",
        path.display(),
        mode & 0o777,
        path.display()
    );
}

#[cfg(test)]
mod tests;
