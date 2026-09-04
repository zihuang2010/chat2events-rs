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

    tracing::info!(
        model = %config.llm.model,
        base_url = %config.llm.base_url,
        reasoning_effort = ?config.llm.reasoning_effort,
        "启动"
    );

    // 全进程建一次，并发跑多个群时 clone 它 —— 连接池和 h2 连接跟着共享
    let llm = Llm::new(&config.llm, secrets.llm.api_key)?;

    // 连不上就在这里炸，别等抽完了才发现库进不去
    let pool = config::mysql_pool(&config.mysql, &secrets.mysql.url).await?;
    tracing::info!(
        max_connections = config.mysql.max_connections,
        "MySQL 连接池就绪"
    );

    daily::run(&config, &llm, &pool).await
}
