//! chat2events —— 从群聊里抽事件。
//!
//! 跑法：
//!   cargo run                            # 读当前目录的 config.toml / secrets.toml
//!   ./chat2events-rs /etc/chat2events    # 生产：从指定目录读
//!
//! 这个文件只做四件事：读配置 · 起日志 · 建资源 · 调 [`daily::run`]。
//! **编排不在这里** —— 跑批那一轮干了什么，看 `daily/`（编排在 `daily/run.rs`）。

use chat2events_rs::{Result, config, daily, llm::Llm};

#[tokio::main]
async fn main() -> Result<()> {
    let (config, secrets) = config::load_from_dir(&config::dir_from_args());
    config::init_logging(&config.log);

    // **两队各打一行。** 抽取和打标是两个不同的模型，而 `run_span` 收的是两个同类型的
    // `&Llm` —— 传反了编译过、测试也过。这两行是第一秒就能看见它的地方：
    // 抽取那行的 max_tokens 该是万级，打标那行该是千级，反了一眼就认得出来。
    tracing::info!(
        extract_model = %config.llm.extract.model,
        extract_base_url = %config.llm.extract.base_url,
        extract_max_tokens = config.llm.extract.max_tokens,
        classify_model = %config.llm.classify.model,
        classify_base_url = %config.llm.classify.base_url,
        classify_max_tokens = config.llm.classify.max_tokens,
        reasoning_effort = ?config.llm.reasoning_effort,
        "启动"
    );

    // 两队各建一次，并发跑多个群时 clone —— 各自的连接池跟着各自共享。
    // 两个 key 目前是同一个（`secrets.toml` 的 `[llm].api_key`），所以第一次要 clone。
    let extract_llm = Llm::new(
        &config.llm,
        &config.llm.extract,
        secrets.llm.api_key.clone(),
    )?;
    let classify_llm = Llm::new(&config.llm, &config.llm.classify, secrets.llm.api_key)?;

    // 连不上就在这里炸，别等抽完了才发现库进不去
    let pool = config::mysql_pool(&config.mysql, &secrets.mysql.url).await?;
    tracing::info!(
        max_connections = config.mysql.max_connections,
        "MySQL 连接池就绪"
    );

    daily::run(&config, &extract_llm, &classify_llm, &pool, &secrets.oss).await
}
