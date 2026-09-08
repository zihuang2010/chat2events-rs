//! 只读工作台入口，与每日跑批分别启动。

use chat2events_rs::{Result, config, web};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert!(
        (2..=3).contains(&args.len()),
        "用法：webui <config_dir> <corpid> [监听地址，默认 127.0.0.1:8787]"
    );
    let (config, secrets) = config::load_web_from_dir(std::path::Path::new(&args[0]));
    config::init_logging(&config.log);
    let pool = config::mysql_read_pool(
        &config.mysql,
        &secrets.mysql.url,
        config.web.query_timeout_secs,
    )
    .await?;
    let address = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("127.0.0.1:8787")
        .parse()?;
    web::serve(
        pool,
        config.ingest.raw_root,
        args[1].clone(),
        address,
        config.web,
    )
    .await
}
