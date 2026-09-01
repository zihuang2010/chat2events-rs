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
    pub llm: LlmConfig,
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

/// ① 摄取 —— 拉取（`pull.rs`）和读取（`ingest.rs`）共用这一段。
#[derive(Deserialize)]
pub struct IngestConfig {
    /// 本地 raw 区 = OSS 的字节级镜像，「已拉到第几字节」= 文件大小。
    /// 目录布局与理由见 `ingest.rs` 模块头（布局的唯一权威）。
    pub raw_root: PathBuf,

    /// 回看窗口 N：读 `[T-N, T-1]`，这 N 天就是非冻结区。
    /// ⚠️ N=2 时「周五提问、周一回复」永远拼不起来 —— 没有任何一次运行会同时读到
    /// 周五和周一，该事件会永久算作未回复，而损失系统性落在**周五值班的人**头上。
    /// 判据是「跨 2 天以上才闭合」的占比，且**要按周几分别看**，> 5% 立刻调 4。
    pub lookback_days: u32,

    /// 下载根地址（CDN）。拼上索引表的 `ndjson_object_key` 就是月文件的 URL。
    /// **不是密钥** —— 是个公开域名，所以在 config.toml 而不是 secrets.toml。
    /// ⚠️ 这条路径配了 30 天 CDN 缓存且客户端绕不掉，见 ADR-0005 结尾。
    pub download_base_url: String,

    /// 同时在飞的月文件下载数。跟 ADR-0004 里 `SEGMENT_MSGS` / `ROOM_CONCURRENCY`
    /// 一个规矩：这类值必须由部署环境明确给出。
    pub pull_concurrency: usize,

    /// 同时在处理的群数（ADR-0004 的 `ROOM_CONCURRENCY`）。**段之间仍然串行，
    /// 并行只加在群与群之间。** 无默认值，缺失即报错。
    ///
    /// 它同时是两件事：
    ///   * **内存上界** —— 一次最多持有这么多个 `Conversation`；
    ///   * **墙钟旋钮** —— ③ 接上之后这是唯一能压的那个，届时约束是端点 TPM
    ///     而不是并发数本身，调它之前先看限流。
    ///
    /// 今天只有读取在并发，实测拐点在 8（100 群 / 555 MB / 12 核，串行 9.81s →
    /// k=8 2.48s，k=12 起不再变快）。
    pub room_concurrency: usize,
}

/// ③ 抽取 —— 只有一个旋钮，而且是省钱的那种。
#[derive(Deserialize)]
pub struct ExtractConfig {
    /// 一个群一天切成 `ceil(n / segment_msgs)` 段。**省钱旋钮，不是质量旋钮**
    /// （ADR-0004）：切几段都不产生接缝（段之间串行传便签），它只是省掉
    /// 「拿整群去试、注定被截断」那一次调用。无默认值，缺失即报错。
    ///
    /// 它还兼着**便签的保留窗口**：「上一整段都没动静就撤下」复用的就是这个数，
    /// 不另发明一个。两者眼下量级相同才合用 —— 换了输出预算大得多的模型、
    /// 段长跳到几千时要拆成独立常量（ADR-0004 结尾）。
    pub segment_msgs: usize,
}

#[derive(Deserialize)]
pub struct LlmConfig {
    pub model: String,
    pub base_url: String,

    /// ⚠️ 绝对不要给这个字段加默认值：ReasoningEffort 自带的 Default 是 Medium ——
    /// 配置里一省掉就变成"开着推理"。实测 qwen3.8-flash 默认开推理，答一个"2"
    /// 要烧 41 个 reasoning token；抽取任务不需要它。宁可缺失即报错，
    /// 也不给一个危险的隐式值。注意 "minimal" 不等于关（实测仍有 13 tokens），
    /// 只有 "none" 是真关。
    pub reasoning_effort: ReasoningEffort,

    /// 抽取要可复现，不是创作 —— 生产恒为 0.0。
    pub temperature: f32,

    /// ⚠️ 跟着模型变，不是跟着端点变：qwen3.8-flash 收 64000，qwen-plus 只到 32768。
    /// 换模型报 range 错就来 config.toml 调这里。
    pub max_tokens: u32,

    /// 单次尝试的超时。实测单段调用 118~168s（3742 条样本、约 370 条/段），留 2 倍余量。
    /// ⚠️ 这是【每次尝试】的上限，不是总耗时：底层默认还会重试 3 次，
    /// 最坏情况墙钟是这个值的 4 倍。要卡总时长得在调用方再包一层。
    pub timeout_secs: u64,

    /// 只管 TCP+TLS 握手。单独设是为了让"端点连不上"几秒内失败，
    /// 而不是耗满上面那个按分钟计的整体超时。
    pub connect_timeout_secs: u64,
}

/// 连接池参数。连接 URL 带密码，不在这里 —— 见 [`MysqlSecrets`]。
#[derive(Deserialize)]
pub struct MysqlConfig {
    /// 池上限。跑批是"调一次 LLM、落一次库"，并发度由外层任务数决定，几条就够。
    pub max_connections: u32,

    /// 池子满了、等一条空闲连接的上限。超时报错，不无限挂着。
    pub acquire_timeout_secs: u64,
}

/// 建连接池。
///
/// ⚠️ 用 `.connect()` 不是 `.connect_lazy()`：这里会立刻握一次手，库连不上就在启动
/// 第一秒炸，跟配置错误一个待遇 —— 而不是跑到落库那步才发现密码是错的。
pub async fn mysql_pool(cfg: &MysqlConfig, url: &str) -> Result<MySqlPool, sqlx::Error> {
    MySqlPoolOptions::new()
        .max_connections(cfg.max_connections)
        .acquire_timeout(Duration::from_secs(cfg.acquire_timeout_secs))
        .connect(url)
        .await
}

#[derive(Deserialize)]
pub struct Secrets {
    pub llm: LlmSecrets,
    pub mysql: MysqlSecrets,
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

/// 从一个目录加载两份配置。生产传 /etc/chat2events，开发传当前目录。
pub fn load_from_dir(dir: &Path) -> (Config, Secrets) {
    let config = load(&dir.join("config.toml"));
    let secrets_path = dir.join("secrets.toml");
    #[cfg(unix)]
    require_owner_only(&secrets_path);
    (config, load(&secrets_path))
}

fn load<T: DeserializeOwned>(path: &Path) -> T {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读不到 {}：{e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("解析失败 {}：{e}", path.display()))
}

/// 密钥文件必须 0600 —— 组或其他人有任何一位权限就拒绝加载（照 ssh 对私钥的规矩）。
#[cfg(unix)]
fn require_owner_only(path: &Path) {
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

    /// 仓库里那份 `config.toml` 必须能填满 [`Config`]。所有键必填、代码里没有默认值，
    /// 所以漏一个键就是**进程起不来** —— 让它在 `cargo test` 里炸，别留到跑批那天。
    #[test]
    fn the_shipped_config_toml_fills_every_field() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml"))
            .unwrap();
        toml::from_str::<Config>(&text).unwrap();
    }
}
