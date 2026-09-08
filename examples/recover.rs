//! 人工补齐历史未完成分类：只读既有事实，不调用抽取模型。
use chat2events_rs::{Result, config, daily, llm::Llm};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert!(
        args.len() == 3,
        "用法：recover <config_dir> <since> <until>"
    );
    let (config, secrets) = config::load_from_dir(std::path::Path::new(&args[0]));
    config::init_logging(&config.log);
    let pool = config::mysql_pool(&config.mysql, &secrets.mysql.url).await?;
    let llm = Llm::new(&config.llm, &config.llm.classify, secrets.llm.api_key)?;
    daily::recover(&config, &llm, &pool, args[1].parse()?, args[2].parse()?).await
}
