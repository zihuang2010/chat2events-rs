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

/// 起日志 —— **`main` 和所有 `examples/` 共用这一处**。
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

    /// 本地 raw 区保留几个月，更老的月目录整个删掉（[`crate::ingest::prune`]）。
    ///
    /// 保留起点锚在**窗口**上（保留「窗口最早月往前推 N-1 个月」起的全部月目录），
    /// 所以 `lookback_days` 配多大都不会把本轮要读的月份删掉。
    ///
    /// ⚠️ **它同时是 webUI 下钻的可见范围。** 超出保留期的事件，`read_by_ids`
    /// 会显式报「取不到这些 msg_id」——「本地没有就回 OSS 取」的兜底还没做。
    /// 调小它之前先想清楚主管要能往回看多久。
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
/// `main` 和 `examples/smoke` 共用，曾经两处逐字重复。
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
    // ⚠️ **是两个消费者的和，不是 `room_concurrency` 一个。** 这条此前只拦
    // `>= room_concurrency`，那是**打标队列独立出来之前**的形状：今天抽取侧
    // `room_concurrency` 个群同时握着 `write_room` 的事务，打标侧
    // `classify.concurrency` 个群同时握着 `read_events` / `update_event_labels` /
    // `finish_classification`，两队互不相让，峰值就是它们的和。
    let peak = config.ingest.room_concurrency + config.classify.concurrency;
    assert!(
        config.mysql.max_connections as usize >= peak,
        "mysql.max_connections（{}）必须 ≥ ingest.room_concurrency（{}）\
         + classify.concurrency（{}）= {peak} —— 抽取和打标是两个消费者、各自持连接，\
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
mod tests {
    use super::*;

    #[test]
    fn invalid_scheduling_values_are_rejected_before_loading_secrets() {
        for (section, field) in [
            ("ingest", "mirror_concurrency"),
            ("ingest", "room_concurrency"),
            ("classify", "concurrency"),
            ("ingest", "raw_retention_months"),
            ("extract", "segment_msgs"),
        ] {
            let mut value: toml::Value = toml::from_str(include_str!("../config.toml")).unwrap();
            value[section][field] = toml::Value::Integer(0);
            let dir = crate::testutil::fresh_root("config", field);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.toml"), toml::to_string(&value).unwrap()).unwrap();
            let error = std::panic::catch_unwind(|| load_from_dir(&dir))
                .err()
                .unwrap();
            let message = error.downcast_ref::<String>().map_or("", String::as_str);
            assert!(
                message.contains(field),
                "应先指出非法配置 {field}：{message}"
            );
            assert!(!message.contains("secrets.toml"), "不应等到读取密钥才报错");
        }
    }

    /// 两条**跨节**约束各自的失效形态 —— serde 一条都查不到。
    ///
    /// 它们防的都是「配置看起来完全正常、进程照常起来、坏事发生在几小时后」：
    ///   * 池子小于两队之和 → 群在落库时等不到连接，报「落库失败」，
    ///     而那时这个群的 token 已经烧完了。
    ///   * 打标预算 ≥ 抽取预算 → 多半是把 `[llm.extract]` 整段抄过去只改了 model。
    ///     跑飞时唯一会喊停的就只剩 `timeout_secs`，每趟挂死几回。
    ///
    /// **两条都只在启动期看得见，所以必须在这里钉住** —— 生产上没有任何后续步骤
    /// 会再检查一遍。
    #[test]
    fn cross_section_constraints_are_rejected_at_startup() {
        /// 改一处 config.toml，走一遍加载，返回 panic 文案。
        fn rejected(tag: &str, edit: impl FnOnce(&mut toml::Value)) -> String {
            let mut value: toml::Value = toml::from_str(include_str!("../config.toml")).unwrap();
            edit(&mut value);
            let dir = crate::testutil::fresh_root("config", tag);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.toml"), toml::to_string(&value).unwrap()).unwrap();
            let error = std::panic::catch_unwind(|| load_from_dir(&dir))
                .err()
                .expect("非法配置必须 panic");
            let message = error
                .downcast_ref::<String>()
                .map_or(String::new(), Clone::clone);
            assert!(!message.contains("secrets.toml"), "不应等到读取密钥才报错");
            message
        }

        // 池子小于两队之和：10 + 6 = 16，给 8 必然不够。
        let message = rejected("pool", |v| {
            v["mysql"]["max_connections"] = toml::Value::Integer(8);
        });
        assert!(
            message.contains("max_connections") && message.contains("classify.concurrency"),
            "报错要点出两个消费者都算进去了：{message}"
        );

        // 把 `[llm.extract]` 的输出上限整段抄给打标 —— 现实中最可能发生的那种错法。
        let message = rejected("budget", |v| {
            v["llm"]["classify"]["max_tokens"] = v["llm"]["extract"]["max_tokens"].clone();
        });
        assert!(
            message.contains("llm.classify.max_tokens"),
            "报错要点名打标那份预算：{message}"
        );
    }

    /// 仓库里那份 `config.toml` 必须能填满 [`Config`]。所有键必填、代码里没有默认值，
    /// 所以漏一个键就是**进程起不来** —— 让它在 `cargo test` 里炸，别留到跑批那天。
    #[test]
    fn the_shipped_config_toml_fills_every_field() {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        toml::from_str::<Config>(&text).unwrap();
    }

    /// 上一条的反面：**缺一个键必须是崩，不是走默认值。**
    ///
    /// 「所有键必填」今天靠的是 serde 在字段缺失时报错，没有任何一处显式检查 ——
    /// 也就是说给某个字段加一个 `#[serde(default)]` 是完全无声的：编译过、
    /// 上面那条测试照样绿（仓库里那份 config.toml 什么都不缺），只有跑批那天
    /// 才发现进程拿着一个谁都没写过的值起来了。这条钉住的就是那个无声改动。
    ///
    /// 拿 `segment_msgs` 开刀是因为它明确「无默认值，缺失即报错」。
    #[test]
    #[should_panic(expected = "解析失败")]
    fn a_missing_key_panics_instead_of_falling_back_to_a_default() {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        let holed: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("segment_msgs"))
            .map(|l| format!("{l}\n"))
            .collect();
        // 注释里也写着 segment_msgs，所以只查赋值行还在不在
        assert!(
            !holed
                .lines()
                .any(|l| l.trim_start().starts_with("segment_msgs")),
            "样本没挖掉那个键，这条测试就白测了"
        );

        let dir = crate::testutil::fresh_root("config", "missing-key");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, holed).unwrap();
        let _: Config = load(&p, false);
    }

    /// **密钥文件的解析报错不许带原文。** `toml` 的错误会回显出错那一行，而唯一
    /// 会有人碰 secrets.toml 的场合正是轮换密钥、粘歪一个引号的时候 ——
    /// 那一行原样进 stderr 就等于进 run.log / journald / cron 邮件 / CI 输出。
    #[test]
    fn a_broken_secrets_file_never_echoes_the_key_into_the_error() {
        let dir = crate::testutil::fresh_root("config", "redact");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secrets.toml");
        // 少一个右引号 —— 粘歪密钥最常见的形态
        std::fs::write(&p, "[llm]\napi_key = \"sk-SUPERSECRET-abc123\n").unwrap();

        let err = std::panic::catch_unwind(|| load::<Secrets>(&p, true))
            .err()
            .unwrap();
        let msg = err
            .downcast_ref::<String>()
            .map_or("", String::as_str)
            .to_string();
        assert!(!msg.contains("SUPERSECRET"), "密钥泄漏进报错了：{msg}");
        assert!(msg.contains("详情已省略"), "{msg}");

        // 反过来钉住 config.toml 那条路仍然打全文 —— 它没有密钥，省掉只会难查
        let c = dir.join("config.toml");
        std::fs::write(&c, "[llm]\nmodel = \"m\n").unwrap();
        let err = std::panic::catch_unwind(|| load::<Config>(&c, false))
            .err()
            .unwrap();
        let msg = err.downcast_ref::<String>().map_or("", String::as_str);
        assert!(msg.contains("TOML parse error"), "{msg}");
    }

    /// 密钥文件权限过宽必须**拒绝加载**（照 ssh 对私钥的规矩）。
    #[cfg(unix)]
    #[test]
    #[should_panic(expected = "权限过宽")]
    fn a_group_readable_secrets_file_is_refused() {
        require_owner_only(&secrets_with_mode("refused", 0o640));
    }

    /// 上一条的对照组 —— 没有它，「拒绝」也可能只是因为这个函数恒崩。
    #[cfg(unix)]
    #[test]
    fn owner_only_secrets_are_accepted() {
        require_owner_only(&secrets_with_mode("accepted", 0o600));
    }

    #[cfg(unix)]
    fn secrets_with_mode(name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testutil::fresh_root("config", name);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secrets.toml");
        std::fs::write(&p, "").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }
}
