//! 只读工作台入口，与每日跑批分别启动。

use chat2events_rs::{Result, config, web};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert!(
        (2..=3).contains(&args.len()),
        "用法：webui <config_dir> <corpid> [监听地址，默认 127.0.0.1:8787]"
    );
    let (config, secrets) = web::config::load_from_dir(std::path::Path::new(&args[0]));
    config::init_logging(&config.log);
    // 名册发现：登录 Nacos、解析两个服务名。**任一步失败就在这里退出** ——
    // 服务名 / 命名空间 / 分组名 / 账号密码 / 服务端地址写错，第一秒就看得见。
    // ⚠️ 绑住不用是有意的：这一片只铺通路（后台刷新任务靠它起来），
    // 消费者（客服姓名 / 商家名称）由后续改动接进 `WebState`。
    let _roster = web::roster::Discovery::start(&config.roster, &secrets.roster).await?;
    let pool = web::config::read_pool(
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
    web::serve(pool, args[1].clone(), address, config.web).await
}
