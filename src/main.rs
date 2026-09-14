//! chat2events —— 从群聊里抽事件。
//!
//! 跑法：
//!   cargo run                            # 读当前目录的 config.toml / secrets.toml
//!   ./chat2events-rs /etc/chat2events    # 生产：从指定目录读
//!
//! 这个文件只做两件事：[`Boot`] 起进程 · 调 [`daily::run`]。
//! **编排不在这里** —— 跑批那一轮干了什么，看 `daily/`（编排在 `daily/run.rs`）。

use chat2events_rs::{Result, boot::Boot, process::daily};

#[tokio::main]
async fn main() -> Result<()> {
    let b = Boot::from_args();

    // 两队模型 + 那行启动日志由 `Boot::llms` 一起给 —— 想拿到两队就一定会打日志。
    let (extract_llm, classify_llm) = b.llms()?;
    let pool = b.pool().await?;

    daily::run(
        &b.config,
        &extract_llm,
        &classify_llm,
        &pool,
        &b.secrets.oss,
    )
    .await
}
