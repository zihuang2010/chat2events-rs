//! 刷新商家摘要表（商家名称 / 商家分组 / 业务经理），由 systemd timer 定时拉起，跑完即退出。
//!
//! ```sh
//! merchant_sync /etc/chat2events
//! # 源码树里：cargo run --locked --bin merchant_sync -- /etc/chat2events
//! ```
//!
//! **这是 `bin` 不是 `example`** —— 它写生产库的 `b_merchant_group_merchant_summary`，
//! 要随 release 产物发出去（理由同 `recover.rs`）。任何一步失败：一行不写，错误打到 stderr，
//! 进程以非零码退出；下一轮定时运行就是重试。配置只要 `[mysql]` / `[log]` / Nacos 三节。
use chat2events_rs::{
    Result, config,
    nacos::Discovery,
    process::merchant_sync::{load_from_dir, run},
};

#[tokio::main]
async fn main() -> Result<()> {
    let (cfg, secrets) = load_from_dir(&config::dir_from_args());
    config::init_logging(&cfg.log);
    // 配错了在第一秒就退出，同工作台：服务名 / 命名空间 / 账号密码写错不带上生产。
    let services = vec![
        cfg.roster.merchant_service.clone(),
        cfg.roster.employee_service.clone(),
    ];
    let discovery = Discovery::start(&cfg.roster, &secrets.roster, services).await?;
    let pool = config::mysql_pool(&cfg.mysql, &secrets.mysql.url).await?;
    run(&pool, &discovery, &cfg.roster).await
}
