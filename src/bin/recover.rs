//! 人工补齐历史未完成分类：只读既有事实，不调用抽取模型。
//!
//! ```sh
//! chat2events-recover /etc/chat2events 2026-08-01 2026-09-05
//! # 源码树里：cargo run --locked --bin recover -- /etc/chat2events 2026-08-01 2026-09-05
//! ```
//!
//! **这是 `bin` 不是 `example`** —— 它写 `classification_status`、发布分类指标，
//! 是承重不变量 5 的执行者。当 example 时它不进 release 产物，而目标机
//! （CentOS 7 / gcc 4.8.5）编不动 bundled DuckDB，`cargo run --example` 在那上面
//! 根本跑不起来：补标这件事在生产机上做不了。
use chat2events_rs::{Result, boot::Boot, process::daily};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert!(
        args.len() == 3,
        "用法：recover <config_dir> <since> <until>"
    );
    let b = Boot::load(std::path::Path::new(&args[0]));
    let pool = b.pool().await?;
    let llm = b.classify_llm()?;
    daily::recover(&b.config, &llm, &pool, args[1].parse()?, args[2].parse()?).await
}
